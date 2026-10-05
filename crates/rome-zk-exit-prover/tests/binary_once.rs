//! Runs the REAL `rome-zk-exit-prover` binary with `--once` against two fake JSON-RPC servers (the L2 node and
//! Solana), with a config this test writes. The crate's other tests drive `poll_once` and the follower over fakes
//! and never start the process, so a process that dies at start (a nested runtime, an exit config that is not
//! there yet) would go unnoticed. What these tests pin is that the process gets through its first reads and ends
//! in an orderly way, never in a panic, and that a chain whose exits are not switched on yet leaves it idle.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use solana_program::pubkey::Pubkey;

const CHAIN_ID: u64 = 4_295_391_538;
const PORTAL: [u8; 20] = [
    0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x16,
];

fn settlement_id() -> Pubkey {
    Pubkey::new_from_array([2; 32])
}

/// Standard base64, enough for account data in a reply (the test crate has no base64 dependency).
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// What a fake server answers. `None` means HTTP 500, as an overloaded node does.
type Handler = Arc<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

struct FakeRpc {
    url: String,
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for FakeRpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl FakeRpc {
    fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (seen, stop) = (seen.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let (seen, stop, handler) =
                                (seen.clone(), stop.clone(), handler.clone());
                            std::thread::spawn(move || serve(conn, handler, seen, stop));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
        }
        FakeRpc { url, seen, stop }
    }

    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }

    fn methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r["method"].as_str().unwrap_or("").to_string())
            .collect()
    }
}

/// One keep-alive connection: reads requests until the client closes it.
fn serve(conn: TcpStream, handler: Handler, seen: Arc<Mutex<Vec<Value>>>, stop: Arc<AtomicBool>) {
    conn.set_nonblocking(false).unwrap();
    conn.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let mut writer = conn.try_clone().unwrap();
    let mut reader = BufReader::new(conn);
    while !stop.load(Ordering::Relaxed) {
        let mut content_length = 0usize;
        let mut got_request_line = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if got_request_line {
                        continue;
                    }
                    break;
                }
                Err(_) => return,
            }
            got_request_line = true;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        if !got_request_line {
            continue;
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let req: Value = serde_json::from_slice(&body).unwrap_or_default();
        seen.lock().unwrap().push(req.clone());
        let Some(result) = handler(&req) else {
            let _ = writer.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        };
        let reply = json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }).to_string();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            reply.len()
        );
        if writer.write_all(head.as_bytes()).is_err() || writer.write_all(reply.as_bytes()).is_err()
        {
            return;
        }
    }
}

/// A Solana node on which the named accounts exist with the given data and every other account is absent.
fn solana(accounts: Vec<(Pubkey, Vec<u8>)>) -> FakeRpc {
    FakeRpc::start(Arc::new(move |req| {
        let ctx = json!({ "slot": 100, "apiVersion": "3.0.0" });
        match req["method"].as_str()? {
            "getAccountInfo" => {
                let key = req["params"][0].as_str()?;
                let value = accounts
                    .iter()
                    .find(|(k, _)| k.to_string() == key)
                    .map(|(_, data)| {
                        json!({
                            "data": [base64(data), "base64"],
                            "executable": false,
                            "lamports": 1_000_000u64,
                            "owner": settlement_id().to_string(),
                            "rentEpoch": 0u64,
                            "space": data.len(),
                        })
                    })
                    .unwrap_or(Value::Null);
                Some(json!({ "context": ctx, "value": value }))
            }
            "getMultipleAccounts" => {
                let values: Vec<Value> = req["params"][0]
                    .as_array()?
                    .iter()
                    .map(|key| {
                        let key = key.as_str().unwrap_or_default();
                        accounts
                            .iter()
                            .find(|(k, _)| k.to_string() == key)
                            .map(|(_, data)| {
                                json!({
                                    "data": [base64(data), "base64"],
                                    "executable": false,
                                    "lamports": 1_000_000u64,
                                    "owner": settlement_id().to_string(),
                                    "rentEpoch": 0u64,
                                    "space": data.len(),
                                })
                            })
                            .unwrap_or(Value::Null)
                    })
                    .collect();
                Some(json!({ "context": ctx, "value": values }))
            }
            "getSlot" => Some(json!(100)),
            "getVersion" => Some(json!({ "solana-core": "3.0.0", "feature-set": 0 })),
            _ => None,
        }
    }))
}

/// An L2 node that has seen no exits.
fn l2_without_exits() -> FakeRpc {
    FakeRpc::start(Arc::new(|req| match req["method"].as_str()? {
        "eth_getLogs" => Some(json!([])),
        "eth_blockNumber" => Some(json!("0x64")),
        "eth_chainId" => Some(json!("0x1")),
        _ => None,
    }))
}

fn failing() -> FakeRpc {
    FakeRpc::start(Arc::new(|_| None))
}

fn exit_config_account(portal: [u8; 20]) -> (Pubkey, Vec<u8>) {
    let (pda, _) = rome_zk_layouts::exit::exit_config::pda(&settlement_id(), CHAIN_ID);
    let data = rome_zk_layouts::exit::exit_config::write(
        &rome_zk_layouts::exit::exit_config::ExitConfigFields {
            chain_id: CHAIN_ID,
            exit_portal: portal,
            bridge_program: [3; 32],
            pending_exit_portal: [0; 20],
            pending_bridge_program: [0; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        },
    );
    (pda, data.to_vec())
}

fn root_account(exit_cap: u64) -> (Pubkey, Vec<u8>) {
    let (pda, _) = rome_zk_layouts::root::pda(&settlement_id(), CHAIN_ID);
    let data = rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
        chain_id: CHAIN_ID,
        number: 0,
        parent_hash: [0; 32],
        state_root: [0; 32],
        block_hash: [0; 32],
        updates: 0,
        profile: 0,
        challenge_window_slots: 172_800,
        prove_window_slots: 0,
        proving_policy: 0,
        poster_bond: 0,
        exit_cap_per_window: exit_cap,
        authority: [0; 32],
        head_pending_batch: 0,
        head_final_batch: 0,
        pending_count: 0,
        max_pending: 0,
    });
    (pda, data.to_vec())
}

/// The settlement program's own account and its ProgramData, as the live devnet program has them: the
/// ProgramData alone is 310,464 bytes.
const LIVE_PROGRAMDATA_BYTES: usize = 310_464;

fn program_accounts(programdata_len: usize) -> Vec<(Pubkey, Vec<u8>)> {
    let programdata = zk_settlement_client::program_data_pda(&settlement_id());
    let mut program = vec![0u8; 36];
    program[0] = 2;
    program[4..].copy_from_slice(programdata.as_ref());
    vec![
        (settlement_id(), program),
        (programdata, vec![0u8; programdata_len]),
    ]
}

/// A Solana node holding the given exit config and root, plus the settlement program.
fn chain_with(exit_config: (Pubkey, Vec<u8>), root: (Pubkey, Vec<u8>)) -> FakeRpc {
    let mut accounts = vec![exit_config, root];
    accounts.extend(program_accounts(LIVE_PROGRAMDATA_BYTES));
    solana(accounts)
}

/// A Solana node holding an active exit config, a root with a cap, and the settlement program.
fn active_chain(programdata_len: usize) -> FakeRpc {
    let mut accounts = vec![exit_config_account(PORTAL), root_account(1_000)];
    accounts.extend(program_accounts(programdata_len));
    solana(accounts)
}

struct Deployment {
    dir: PathBuf,
    config: PathBuf,
}

impl Drop for Deployment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_deployment(name: &str, l2_url: &str, solana_url: &str, extra: &str) -> Deployment {
    let dir = std::env::temp_dir().join(format!(
        "exit-prover-binary-once-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let payer = solana_keypair::Keypair::new();
    solana_keypair::write_keypair_file(&payer, dir.join("payer.json")).unwrap();
    let config = format!(
        r#"chain_id = {CHAIN_ID}
settlement_program_id = "{settlement}"
settlement_rpc = "{solana_url}"
verifier_rpc = "{l2_url}"
payer_key_path = "{dir}/payer.json"
metrics_addr = "127.0.0.1:0"
poll_interval_ms = 50
{extra}
"#,
        settlement = settlement_id(),
        dir = dir.display(),
    );
    let path = dir.join("exit-prover.toml");
    std::fs::write(&path, config).unwrap();
    Deployment { dir, config: path }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs the real binary to the end, killing it (the child this test started) if it has not exited by the deadline.
fn run_once(config: &Path) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rome-zk-exit-prover"))
        .arg("--config")
        .arg(config)
        .arg("--once")
        .env("RUST_BACKTRACE", "0")
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        out.read_to_string(&mut s).ok();
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        err.read_to_string(&mut s).ok();
        s
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let run = Run {
        code: status.and_then(|s| s.code()),
        stdout: out_t.join().unwrap(),
        stderr: err_t.join().unwrap(),
    };
    assert!(
        status.is_some(),
        "the binary did not exit in time\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    run
}

fn assert_clean_exit(run: &Run) {
    assert!(
        !run.stderr.contains("panicked") && !run.stdout.contains("panicked"),
        "the binary panicked\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        run.code,
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
}

/// A chain that has not switched exits on has no exit_config account at all. The process must wait, not die: it
/// runs under a restart policy, and a crash loop would hide the real state behind restarts.
#[test]
fn no_exit_config_account_leaves_the_prover_idle_and_exits_zero() {
    let l2 = l2_without_exits();
    let sol = solana(vec![]);
    let d = write_deployment("no-config", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    assert!(
        sol.methods().iter().any(|m| m == "getAccountInfo"),
        "never read the exit config"
    );
    assert!(
        !l2.methods().iter().any(|m| m == "eth_getLogs"),
        "scanned for exits although none are configured: {:?}",
        l2.methods()
    );
}

#[test]
fn an_exit_config_with_no_portal_leaves_the_prover_idle() {
    let l2 = l2_without_exits();
    let sol = chain_with(exit_config_account([0; 20]), root_account(1_000));
    let d = write_deployment("zero-portal", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    assert!(
        !l2.methods().iter().any(|m| m == "eth_getLogs"),
        "{:?}",
        l2.methods()
    );
}

#[test]
fn a_zero_exit_cap_leaves_the_prover_idle() {
    let l2 = l2_without_exits();
    let sol = chain_with(exit_config_account(PORTAL), root_account(0));
    let d = write_deployment("zero-cap", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    assert!(
        !l2.methods().iter().any(|m| m == "eth_getLogs"),
        "{:?}",
        l2.methods()
    );
}

/// With the portal set and a cap, the prover scans the portal the exit config names, from the chain's data.
#[test]
fn an_active_exit_config_scans_the_portal_it_names_and_exits_zero() {
    let l2 = l2_without_exits();
    let sol = active_chain(LIVE_PROGRAMDATA_BYTES);
    let d = write_deployment("active", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    let logs: Vec<Value> = l2
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "eth_getLogs")
        .collect();
    assert_eq!(logs.len(), 1, "one scan per poll: {logs:?}");
    let address = logs[0]["params"][0]["address"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert_eq!(address, "0x4200000000000000000000000000000000000016");
    assert!(
        sol.methods().iter().any(|m| m == "getSlot"),
        "the poll never read the slot"
    );
}

/// Both nodes down: a poll is skipped, the process does not panic and does not exit with an error.
#[test]
fn nodes_that_answer_500_do_not_panic_the_prover() {
    let l2 = failing();
    let sol = failing();
    let d = write_deployment("down", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
}

/// Removes terminal colour codes so a log line can be searched as plain text.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The loaded-accounts limit the binary says it will send with, read from its log.
fn logged_limit(run: &Run) -> Option<u64> {
    let text = plain(&format!("{}{}", run.stdout, run.stderr));
    let at = text.find("loaded_accounts_data_size_limit=")?;
    text[at + "loaded_accounts_data_size_limit=".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// ProveExit loads the settlement program's ProgramData, which is hundreds of kilobytes. With no limit set, the
/// binary works the limit out from the live account sizes at start, instead of a small fixed number the
/// network would refuse every proof against.
#[test]
fn an_unset_loaded_accounts_limit_is_worked_out_from_the_live_program_size() {
    let l2 = l2_without_exits();
    let sol = active_chain(LIVE_PROGRAMDATA_BYTES);
    let d = write_deployment("limit-derived", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    let limit = logged_limit(&run).unwrap_or_else(|| {
        panic!(
            "the binary never said which limit it sends with\nstdout:\n{}\nstderr:\n{}",
            run.stdout, run.stderr
        )
    });
    assert!(
        limit >= LIVE_PROGRAMDATA_BYTES as u64,
        "the limit {limit} cannot hold the program data"
    );
    assert_eq!(limit % (32 * 1024), 0, "rounded to whole 32 KiB pages");
    assert_eq!(
        limit,
        10 * 32 * 1024,
        "ten pages for a 310,464-byte program"
    );
}

/// A configured limit below what ProveExit loads is refused at start, naming the setting and the number needed.
#[test]
fn a_configured_limit_below_the_requirement_is_refused_by_name() {
    let l2 = l2_without_exits();
    let sol = active_chain(LIVE_PROGRAMDATA_BYTES);
    let d = write_deployment(
        "limit-too-low",
        &l2.url,
        &sol.url,
        "loaded_accounts_data_size_limit = 16384",
    );
    let run = run_once(&d.config);
    assert_ne!(run.code, Some(0), "started with a limit that cannot work");
    let text = plain(&format!("{}{}", run.stdout, run.stderr));
    assert!(text.contains("loaded_accounts_data_size_limit"), "{text}");
    assert!(text.contains("327680"), "does not name the need: {text}");
    assert!(
        !l2.methods().iter().any(|m| m == "eth_getLogs"),
        "scanned before refusing: {:?}",
        l2.methods()
    );
}

/// A configured limit at or above the requirement is used as given.
#[test]
fn a_configured_limit_at_or_above_the_requirement_is_used_as_given() {
    let l2 = l2_without_exits();
    let sol = active_chain(LIVE_PROGRAMDATA_BYTES);
    let d = write_deployment(
        "limit-generous",
        &l2.url,
        &sol.url,
        "loaded_accounts_data_size_limit = 400000",
    );
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    assert_eq!(logged_limit(&run), Some(400_000));
}

/// Without the program on chain the limit cannot be worked out, so nothing is sent or scanned this poll.
#[test]
fn a_chain_without_the_program_leaves_the_prover_idle() {
    let l2 = l2_without_exits();
    let sol = solana(vec![exit_config_account(PORTAL), root_account(1_000)]);
    let d = write_deployment("no-program", &l2.url, &sol.url, "");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    assert!(
        !l2.methods().iter().any(|m| m == "eth_getLogs"),
        "{:?}",
        l2.methods()
    );
}

/// The binary asks for the portal's logs in pieces no wider than `max_log_range`, up to the head the node reports,
/// never one open-ended call.
#[test]
fn the_portal_is_scanned_in_pieces_no_wider_than_the_configured_range() {
    let l2 = l2_without_exits();
    let sol = active_chain(LIVE_PROGRAMDATA_BYTES);
    let d = write_deployment("chunked", &l2.url, &sol.url, "max_log_range = 40");
    let run = run_once(&d.config);
    assert_clean_exit(&run);
    let ranges: Vec<(String, String)> = l2
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "eth_getLogs")
        .map(|r| {
            (
                r["params"][0]["fromBlock"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                r["params"][0]["toBlock"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let want = [("0x0", "0x27"), ("0x28", "0x4f"), ("0x50", "0x64")];
    assert_eq!(
        ranges,
        want.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<Vec<_>>()
    );
}
