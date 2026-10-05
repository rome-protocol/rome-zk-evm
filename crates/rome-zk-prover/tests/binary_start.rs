//! Runs the REAL `rome-zk-prover` binary against a fake JSON-RPC server, so the way the process starts and makes its
//! first reads is exercised end to end. The follower's own tests drive `follower::run` with fakes and never start the
//! binary, which is how a process that exits at start went unnoticed.
//!
//! The fake server answers every account read with "no such account", so the chain it describes has nothing on it. That
//! is enough to reach the first read in each mode; what these tests pin is that the process gets through the read and
//! ends in an orderly way, never in a panic.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// How the fake server treats every request.
#[derive(Clone, Copy)]
enum Mode {
    /// Valid replies; every account is absent.
    EmptyChain,
    /// HTTP 500 on every request, as an overloaded node does.
    Failing,
}

struct FakeRpc {
    url: String,
    methods: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for FakeRpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl FakeRpc {
    fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let methods = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (methods, stop) = (methods.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let (methods, stop) = (methods.clone(), stop.clone());
                            std::thread::spawn(move || serve(conn, mode, methods, stop));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
        }
        FakeRpc { url, methods, stop }
    }

    fn methods_seen(&self) -> Vec<String> {
        self.methods.lock().unwrap().clone()
    }
}

/// One keep-alive connection: reads requests until the client closes it.
fn serve(conn: TcpStream, mode: Mode, methods: Arc<Mutex<Vec<String>>>, stop: Arc<AtomicBool>) {
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
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let method = req["method"].as_str().unwrap_or("").to_string();
        methods.lock().unwrap().push(method.clone());
        let reply = match mode {
            Mode::Failing => {
                let _ = writer.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
            Mode::EmptyChain => empty_chain_reply(&req, &method),
        };
        let reply = reply.to_string();
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

fn empty_chain_reply(req: &serde_json::Value, method: &str) -> serde_json::Value {
    let ctx = serde_json::json!({ "slot": 100, "apiVersion": "3.0.0" });
    let result = match method {
        "getMultipleAccounts" => {
            let n = req["params"][0].as_array().map_or(0, |a| a.len());
            serde_json::json!({ "context": ctx, "value": vec![serde_json::Value::Null; n] })
        }
        "getAccountInfo" => serde_json::json!({ "context": ctx, "value": null }),
        "getBalance" => serde_json::json!({ "context": ctx, "value": 5_000_000_000u64 }),
        "getSlot" => serde_json::json!(100),
        "getVersion" => serde_json::json!({ "solana-core": "3.0.0", "feature-set": 0 }),
        other => {
            return serde_json::json!({
                "jsonrpc": "2.0", "id": req["id"],
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            })
        }
    };
    serde_json::json!({ "jsonrpc": "2.0", "id": req["id"], "result": result })
}

/// A complete deployment on disk: config, vkey of record, ELF, payer key and a `cargo-zisk` that reports the release.
struct Deployment {
    _dir: tempfile::TempDir,
    config: PathBuf,
}

fn write_deployment(rpc_url: &str) -> Deployment {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();

    // The fixture's own key material, with the ELF hash swapped for the hash of a stand-in ELF written here.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/vkeys/tiber-200101-layout1.zisk-1.3.1.json");
    let mut vkey: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    let elf = b"stand-in elf";
    std::fs::write(p.join("guest.elf"), elf).unwrap();
    vkey["elf_sha256"] = hex::encode(Sha256::digest(elf)).into();
    std::fs::write(p.join("vkey.json"), vkey.to_string()).unwrap();

    // An install that reports the release, with the proving key root the record pins.
    let release = vkey["zisk"].as_str().unwrap().to_string();
    let root_c = hex::decode(
        vkey["rootCVadcopFinal"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
    )
    .unwrap();
    let words: Vec<u64> = root_c
        .chunks(8)
        .map(|c| u64::from_be_bytes(c.try_into().unwrap()))
        .collect();
    let zisk_home = p.join("zisk");
    let key_dir = zisk_home.join("provingKey/zisk/vadcop_final");
    std::fs::create_dir_all(&key_dir).unwrap();
    std::fs::write(
        key_dir.join("vadcop_final.verkey.json"),
        serde_json::to_string(&words).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(zisk_home.join("bin")).unwrap();
    let bin = zisk_home.join("bin/cargo-zisk");
    std::fs::write(
        &bin,
        format!("#!/bin/sh\necho 'cargo-zisk {release} [cpu] (test)'\n"),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let payer = solana_keypair::Keypair::new();
    solana_keypair::write_keypair_file(&payer, p.join("payer.json")).unwrap();
    std::fs::write(p.join("genesis.json"), b"{}").unwrap();

    let config = format!(
        r#"chain_id = 200101
inbox_program_id = "{inbox}"
settlement_program_id = "{settlement}"
solana_rpc_url = "{rpc_url}"
verifier_rpc_url = "http://127.0.0.1:1"
genesis_path = "{dir}/genesis.json"
elf_path = "{dir}/guest.elf"
vkey_json = "{dir}/vkey.json"
zisk_home = "{dir}/zisk"
gpu = false
work_dir = "{dir}/work"
payer_key_path = "{dir}/payer.json"
metrics_addr = "127.0.0.1:0"
poll_interval_ms = 50
"#,
        inbox = solana_program::pubkey::Pubkey::new_from_array([1; 32]),
        settlement = solana_program::pubkey::Pubkey::new_from_array([2; 32]),
        dir = p.display(),
    );
    std::fs::write(p.join("prover.toml"), config).unwrap();
    Deployment {
        config: p.join("prover.toml"),
        _dir: dir,
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs the real binary to the end, killing it (the child this test started) if it has not exited by the deadline.
fn run_binary(config: &Path, args: &[&str]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rome-zk-prover"))
        .arg("--config")
        .arg(config)
        .args(args)
        .env("RUST_BACKTRACE", "0")
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

fn assert_no_panic(run: &Run) {
    assert!(
        !run.stderr.contains("panicked") && !run.stdout.contains("panicked"),
        "the binary panicked\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn follow_dry_run_reads_the_chain_and_exits_zero() {
    let rpc = FakeRpc::start(Mode::EmptyChain);
    let d = write_deployment(&rpc.url);
    let run = run_binary(&d.config, &["--follow", "--dry-run", "--iterations", "1"]);
    assert_no_panic(&run);
    assert_eq!(
        run.code,
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("anchor refused"),
        "stdout:\n{}",
        run.stdout
    );
    assert!(rpc
        .methods_seen()
        .iter()
        .any(|m| m == "getMultipleAccounts"));
}

#[test]
fn follow_dry_run_with_a_failing_node_names_the_failure() {
    let rpc = FakeRpc::start(Mode::Failing);
    let d = write_deployment(&rpc.url);
    let run = run_binary(&d.config, &["--follow", "--dry-run", "--iterations", "1"]);
    assert_no_panic(&run);
    assert_eq!(
        run.code,
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("anchor refused"),
        "stdout:\n{}",
        run.stdout
    );
}

#[test]
fn once_dry_run_reads_the_chain_without_a_nested_runtime_panic() {
    let rpc = FakeRpc::start(Mode::EmptyChain);
    let d = write_deployment(&rpc.url);
    let run = run_binary(&d.config, &["--once", "--dry-run", "--batch", "1"]);
    assert_no_panic(&run);
    // An empty chain has no root account to anchor on: a named refusal and exit status 1, not a panic.
    assert_eq!(
        run.code,
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("root account"),
        "stderr:\n{}",
        run.stderr
    );
    assert!(
        rpc.methods_seen()
            .iter()
            .any(|m| m == "getMultipleAccounts"),
        "the binary never read the chain; stderr:\n{}",
        run.stderr
    );
}

#[test]
fn follow_reads_the_chain_for_one_iteration_and_stops() {
    let rpc = FakeRpc::start(Mode::EmptyChain);
    let d = write_deployment(&rpc.url);
    let run = run_binary(&d.config, &["--follow", "--iterations", "1"]);
    assert_no_panic(&run);
    assert_eq!(
        run.code,
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("root account"),
        "stderr:\n{}",
        run.stderr
    );
    let seen = rpc.methods_seen();
    assert!(
        seen.iter().any(|m| m == "getBalance"),
        "seen: {seen:?}\nstderr:\n{}",
        run.stderr
    );
    assert!(
        seen.iter().any(|m| m == "getMultipleAccounts"),
        "seen: {seen:?}\nstderr:\n{}",
        run.stderr
    );
}
