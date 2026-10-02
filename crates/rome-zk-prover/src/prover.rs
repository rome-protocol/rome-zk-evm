//! The `Prover` trait and its implementation, `LocalCargoZisk` — a `cargo-zisk` subprocess
//! (`cargo-zisk prove --plonk` behind a `Prover` trait). Verified against `cargo-zisk` 1.2.0-alpha:
//! `-e/--elf`, `-i/--inputs`, `-o/--output`, `--plonk`, `-y/--verify-proof`; `-o <path>` produces the
//! proof at that exact FILE path (not a directory); `--plonk` runs the STARK then the wrap in one
//! invocation; there is no `-g`/GPU flag in this version — "GPU" is a property of which `cargo-zisk`
//! build `zisk_home` points at, not a runtime flag, so `LocalCargoZisk::gpu` is reporting metadata only,
//! never added to the command line.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

/// One finished proof: where the proof file landed, and the per-stage walls parsed from the
/// subprocess's own log output.
#[derive(Debug, Clone)]
pub struct ProofFile {
    pub path: PathBuf,
    pub walls: StageWalls,
}

/// Stage walls parsed from `cargo-zisk`'s own stdout/stderr log lines:
/// `<<< GENERATING_WRAPPER_SNARK_PROOF (Nms)`, `Proof generated in Ns, steps: N`, and the verified
/// marker. Any field that did not appear in the log is `None`/`false` — a missing line is never
/// invented as zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageWalls {
    pub wrapper_snark_ms: Option<u64>,
    pub proof_generated_secs: Option<f64>,
    pub steps: Option<u64>,
    pub verified: bool,
}

/// Refused by name — never a silently-swallowed subprocess failure.
#[derive(Debug, thiserror::Error)]
pub enum ProveError {
    #[error("spawn {bin}: {source}")]
    Spawn {
        bin: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cargo-zisk exited {exit:?}:\n{tail}")]
    ProveFailed { exit: Option<i32>, tail: String },
    #[error("cargo-zisk did not finish within {0:?}")]
    ProveTimeout(Duration),
    #[error("cargo-zisk exited successfully but never printed a verified line:\n{tail}")]
    NotVerified { tail: String },
    /// A clean exit that DID print a verified line, but `-o out_file` was never actually written
    /// (or was written empty) — refused by name rather than handed to a caller that would read a
    /// missing or zero-byte proof file as a real one.
    #[error("cargo-zisk exited and printed a verified line, but {path} is missing or empty")]
    OutputMissing { path: PathBuf },
}

/// Produces one PLONK proof file from a guest ELF and a witness input file.
pub trait Prover {
    fn prove(&self, elf: &Path, input_bin: &Path, out_file: &Path)
        -> Result<ProofFile, ProveError>;
}

/// `cargo-zisk prove --plonk` as a subprocess. Makes exactly ONE subprocess
/// invocation per `prove()` call; retrying a failed attempt with a wiped work directory is the
/// follower's own state-machine concern, reading `Config::max_prove_attempts` — this
/// type carries no attempt count of its own.
pub struct LocalCargoZisk {
    pub zisk_home: PathBuf,
    /// Reporting metadata only — see the module doc.
    pub gpu: bool,
    pub timeout: Duration,
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn parse_stage_walls(log: &str) -> StageWalls {
    let mut walls = StageWalls::default();
    for line in log.lines() {
        if let Some(rest) = line.split("GENERATING_WRAPPER_SNARK_PROOF (").nth(1) {
            if let Some(ms_str) = rest.split("ms)").next() {
                walls.wrapper_snark_ms = ms_str.trim().parse().ok();
            }
        }
        if let Some(rest) = line.split("Proof generated in ").nth(1) {
            if let Some((secs_part, tail)) = rest.split_once("s, steps: ") {
                walls.proof_generated_secs = secs_part.trim().parse().ok();
                let steps_str: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                walls.steps = steps_str.parse().ok();
            }
        }
        if line.contains("SNARK proof was verified") || line.contains("Proof verified successfully")
        {
            walls.verified = true;
        }
    }
    walls
}

impl Prover for LocalCargoZisk {
    fn prove(
        &self,
        elf: &Path,
        input_bin: &Path,
        out_file: &Path,
    ) -> Result<ProofFile, ProveError> {
        let call_started = Instant::now();
        // The wait loop below is bounded by `self.timeout`; the drain step after it gets a
        // further half of that as its own bound of last resort (a grandchild the process-group
        // kill did not reach — see the drain comment below). Together that keeps the whole call
        // safely under `2 x timeout` with real margin, not flush against the ceiling.
        let wait_deadline = call_started + self.timeout;
        let drain_deadline = wait_deadline + self.timeout / 2;

        let bin = self.zisk_home.join("bin").join("cargo-zisk");
        let mut child = Command::new(&bin)
            .arg("prove")
            .arg("-e")
            .arg(elf)
            .arg("-i")
            .arg(input_bin)
            .arg("--plonk")
            .arg("-y")
            .arg("-o")
            .arg(out_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Make the child the leader of its own new process group (pgid == its own pid) so a
            // timeout can `killpg` the whole group — the tool itself AND anything it forked —
            // instead of leaving a grandchild to run to its own natural exit.
            .process_group(0)
            .spawn()
            .map_err(|source| ProveError::Spawn {
                bin: bin.clone(),
                source,
            })?;

        // The child leads its own process group (pgid == its pid, captured right after a
        // successful spawn and before any `wait`, so it can never be 0 or a reused pid).
        let pgid = Pid::from_raw(child.id() as i32);

        // Drain both pipes concurrently so a chatty real proof run's output can never fill the
        // pipe buffer and deadlock the child against our own poll loop below. Each reader appends
        // every chunk it reads to a shared buffer AS IT ARRIVES and signals completion over a
        // channel, so (a) whatever the tool printed before exiting is always available for the
        // tail / stage-wall parse, even when a grandchild keeps the pipe's write end open past
        // our bound, and (b) `prove()` itself never blocks on a reader: `recv_timeout` below
        // always returns by `drain_deadline`, leaving an unfinished reader to exit on its own.
        fn spawn_reader<R: std::io::Read + Send + 'static>(
            mut pipe: R,
        ) -> (Arc<Mutex<String>>, mpsc::Receiver<()>) {
            let buf = Arc::new(Mutex::new(String::new()));
            let (done_tx, done_rx) = mpsc::channel();
            let sink = Arc::clone(&buf);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut s) = sink.lock() {
                                s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            }
                        }
                    }
                }
                let _ = done_tx.send(());
            });
            (buf, done_rx)
        }
        let stdout_pipe = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let (out_buf, out_done) = spawn_reader(stdout_pipe);
        let (err_buf, err_done) = spawn_reader(stderr_pipe);

        let status = loop {
            match child.try_wait().map_err(|source| ProveError::Spawn {
                bin: bin.clone(),
                source,
            })? {
                Some(status) => break Some(status),
                None => {
                    if Instant::now() >= wait_deadline {
                        // Kill the whole process group `child` leads (itself and anything it
                        // forked), never just its own pid — a `sh`-run script's own backgrounded
                        // or foreground children are not touched by killing `sh` alone.
                        let _ = killpg(pgid, Signal::SIGKILL);
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        // On the normal-exit path too: anything the tool forked and left behind is by
        // construction a member of the group only the tool populated — a helper that outlives
        // its parent must not outlive this call either. ESRCH (nothing left) is the common case.
        if status.is_some() {
            let _ = killpg(pgid, Signal::SIGKILL);
        }

        // Bounded regardless of path: `recv_timeout` never waits past `drain_deadline`, so a
        // pipe some grandchild is still holding open can delay this call but never hang it —
        // and the buffer holds whatever was read so far either way.
        let wait_bounded = |done: mpsc::Receiver<()>| {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            let _ = done.recv_timeout(remaining);
        };
        wait_bounded(out_done);
        wait_bounded(err_done);
        let take = |buf: Arc<Mutex<String>>| buf.lock().map(|s| s.clone()).unwrap_or_default();
        let stdout = take(out_buf);
        let stderr = take(err_buf);
        let combined = format!("{stdout}\n{stderr}");

        let Some(status) = status else {
            // A killed-on-timeout run may still have left a partial `-o` file behind: a
            // resume gate that only checks file EXISTENCE must never mistake it for a real proof, so
            // this call cleans up after itself rather than leaving a garbage artefact for the next
            // caller to trip over. Best-effort: a file that was never created is not an error here.
            let _ = std::fs::remove_file(out_file);
            return Err(ProveError::ProveTimeout(self.timeout));
        };

        if !status.success() {
            let _ = std::fs::remove_file(out_file);
            return Err(ProveError::ProveFailed {
                exit: status.code(),
                tail: tail_lines(&combined, 40),
            });
        }

        let walls = parse_stage_walls(&combined);
        if !walls.verified {
            let _ = std::fs::remove_file(out_file);
            return Err(ProveError::NotVerified {
                tail: tail_lines(&combined, 40),
            });
        }

        // The proof must be a real, non-empty FILE at the path we asked for: a directory's
        // metadata length is non-zero on ext4, so a length-only check would wave one through.
        let out_ok = std::fs::metadata(out_file)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false);
        if !out_ok {
            return Err(ProveError::OutputMissing {
                path: out_file.to_path_buf(),
            });
        }

        Ok(ProofFile {
            path: out_file.to_path_buf(),
            walls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Every `tests/fake-cargo-zisk/*.sh` script is installed as `<its own tempdir>/bin/cargo-zisk`
    /// exactly ONCE for the whole test binary (not once per test), computed the first time any
    /// test asks for one. Each script needs its own `zisk_home`, since `LocalCargoZisk` always
    /// execs a fixed relative path (`bin/cargo-zisk`) — one shared directory could only ever hold
    /// one script. Installing once and never touching the file again (rather than a fresh
    /// tempdir + copy + chmod per test, racing across parallel test threads) is what used to need
    /// a short ETXTBSY retry loop in `prove()` itself; that loop is gone, and this is why it can
    /// be — a real, persistent spawn failure is the only thing `prove()` needs to handle now, and
    /// this test suite never manufactures a transient one against itself.
    fn fake_home(script_name: &str) -> PathBuf {
        static HOMES: std::sync::OnceLock<std::collections::HashMap<&'static str, PathBuf>> =
            std::sync::OnceLock::new();
        let homes = HOMES.get_or_init(|| {
            const SCRIPTS: &[&str] = &[
                "fails.sh",
                "hangs.sh",
                "no-verify.sh",
                "succeeds.sh",
                "orphans-helper.sh",
                "escapes-group.sh",
                "no-output.sh",
                "fast-exit-leaves-helper.sh",
                "partial-output-then-hangs.sh",
                "partial-output-then-fails.sh",
            ];
            SCRIPTS
                .iter()
                .map(|&name| {
                    let home = tempfile::tempdir().expect("tempdir").keep();
                    let bin_dir = home.join("bin");
                    std::fs::create_dir_all(&bin_dir).unwrap();
                    let src = PathBuf::from(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fake-cargo-zisk"
                    ))
                    .join(name);
                    let dest = bin_dir.join("cargo-zisk");
                    std::fs::copy(&src, &dest)
                        .unwrap_or_else(|e| panic!("copy {src:?} -> {dest:?}: {e}"));
                    let mut perms = std::fs::metadata(&dest).unwrap().permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(&dest, perms).unwrap();
                    (name, home)
                })
                .collect()
        });
        homes
            .get(script_name)
            .unwrap_or_else(|| panic!("no fake cargo-zisk registered for {script_name}"))
            .clone()
    }

    fn prover(zisk_home: &Path, timeout: Duration) -> LocalCargoZisk {
        LocalCargoZisk {
            zisk_home: zisk_home.to_path_buf(),
            gpu: false,
            timeout,
        }
    }

    #[test]
    fn fake_binary_exit_1_is_prove_failed() {
        let home = fake_home("fails.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let err = prover(&home, Duration::from_secs(5))
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .unwrap_err();
        assert!(
            matches!(err, ProveError::ProveFailed { exit: Some(1), .. }),
            "expected ProveFailed{{exit: Some(1)}}, got {err:?}"
        );
    }

    #[test]
    fn fake_binary_that_never_prints_verified_is_not_verified() {
        let home = fake_home("no-verify.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let err = prover(&home, Duration::from_secs(5))
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .unwrap_err();
        assert!(
            matches!(err, ProveError::NotVerified { .. }),
            "expected NotVerified, got {err:?}"
        );
    }

    #[test]
    fn fake_binary_that_sleeps_past_the_timeout_is_prove_timeout() {
        let home = fake_home("hangs.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(200);
        let start = Instant::now();
        let err = prover(&home, timeout)
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, ProveError::ProveTimeout(_)),
            "expected ProveTimeout, got {err:?}"
        );
        assert!(
            elapsed < timeout * 2,
            "prove() should return within 2x the configured timeout ({timeout:?}); took {elapsed:?}"
        );
    }

    /// Repro shape: a fake `cargo-zisk` that forks a background helper (`sleep 3117 &`)
    /// before blocking in the foreground (`sleep 3118`). `child.kill()` on the direct pid alone
    /// never touches the backgrounded grandchild, which keeps the stdout pipe open forever — so
    /// the unconditional `out_handle.join()` after the kill blocks for the life of that helper
    /// (observed: `prove()` only returns after an operator manually `pkill`s the helper). The fix
    /// must spawn in its own process group and `killpg` it on timeout, with a BOUNDED drain, so
    /// `prove()` returns `ProveTimeout` promptly and leaves nothing running behind it.
    #[test]
    fn timeout_kills_the_whole_process_group_and_leaves_no_orphaned_helper() {
        // Best-effort: a previous failed/interrupted run of this exact test may have left a
        // `sleep 311[78]` helper behind; never let that contaminate this run's pgrep assertion.
        let _ = Command::new("pkill").args(["-f", "sleep 311[78]"]).status();

        let home = fake_home("orphans-helper.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(500);
        let start = Instant::now();
        let err = prover(&home, timeout)
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, ProveError::ProveTimeout(_)),
            "expected ProveTimeout, got {err:?}"
        );
        assert!(
            elapsed < timeout * 2,
            "prove() should return within 2x the configured timeout ({timeout:?}); took {elapsed:?}"
        );

        let pgrep = Command::new("pgrep")
            .args(["-f", "sleep 311[78]"])
            .output()
            .expect("pgrep");
        let stdout = String::from_utf8_lossy(&pgrep.stdout);
        assert!(
            stdout.trim().is_empty(),
            "expected no orphaned `sleep 311[78]` helper after prove() returned; pgrep found:\n{stdout}"
        );
    }

    /// A helper that escapes `killpg`'s reach entirely (its own `setsid` session/process group,
    /// while still inheriting the piped fds) is the case ONLY the drain's own bound can save —
    /// the process-group kill above cannot touch it. `prove()` must still return `ProveTimeout`
    /// within `2 x timeout`; this test does not (and cannot) assert the escaped helper is also
    /// gone, so it cleans it up by hand afterward.
    #[test]
    fn timeout_bounds_the_drain_even_when_a_helper_escapes_the_process_group() {
        let _ = Command::new("pkill")
            .args(["-f", "sleep 311[9]|sleep 3120"])
            .status();

        let home = fake_home("escapes-group.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(500);
        let start = Instant::now();
        let err = prover(&home, timeout)
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, ProveError::ProveTimeout(_)),
            "expected ProveTimeout, got {err:?}"
        );
        assert!(
            elapsed < timeout * 2,
            "prove() should return within 2x the configured timeout ({timeout:?}) even when a \
             helper escapes killpg's process group; took {elapsed:?}"
        );

        // Best-effort cleanup: this escaped helper is outside the process group `prove()` can
        // kill, by construction of this test, so it is still alive here and must be reaped by
        // hand rather than left running for the full 3119s.
        let _ = Command::new("pkill")
            .args(["-9", "-f", "sleep 3119"])
            .status();
    }

    /// A directory at the `-o` path is not a proof: `metadata().len()` of a directory is non-zero
    /// on ext4 (4096), so a length-only check would wave it through.
    #[test]
    fn a_directory_at_out_file_is_output_missing() {
        let home = fake_home("no-output.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let out_file = out_dir.path().join("proof");
        std::fs::create_dir_all(&out_file).unwrap();
        let err = prover(&home, Duration::from_secs(5))
            .prove(Path::new("elf"), Path::new("input.bin"), &out_file)
            .expect_err("a directory at out_file must not pass as a proof");
        assert!(
            matches!(err, ProveError::OutputMissing { .. }),
            "expected OutputMissing, got {err:?}"
        );
    }

    /// The tool exits normally but a helper it forked outlives it, still holding the stdout pipe.
    /// The call must return promptly WITH the lines the tool printed (the tail must not be lost
    /// to the bounded drain), and the helper must be gone after the call — the group kill applies
    /// on the normal-exit path too, not only on timeout.
    #[test]
    fn a_helper_that_outlives_a_normal_exit_is_killed_and_the_output_is_still_captured() {
        let home = fake_home("fast-exit-leaves-helper.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(600);
        let started = Instant::now();
        let proof = prover(&home, timeout)
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .expect("a normal exit with a lingering helper is still a success");
        let elapsed = started.elapsed();
        assert!(
            elapsed < timeout * 2,
            "prove() must not wait for the helper: took {elapsed:?}"
        );
        assert!(
            proof.walls.verified,
            "the verified line printed before exit must be captured, not lost to the drain bound"
        );
        assert_eq!(proof.walls.steps, Some(2048));
        // SIGKILL delivery is asynchronous: give the kernel a moment to tear the helper down,
        // but the helper must be gone well before its own 3120 s sleep would end.
        let deadline = Instant::now() + Duration::from_secs(3);
        let listed = loop {
            let survivors = std::process::Command::new("pgrep")
                .args(["-f", "sleep 3120"])
                .output()
                .expect("pgrep");
            let listed = String::from_utf8_lossy(&survivors.stdout).into_owned();
            if listed.trim().is_empty() || Instant::now() >= deadline {
                break listed;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            listed.trim().is_empty(),
            "the helper must not survive a normal exit; pgrep still found after 3 s: {listed}"
        );
    }

    #[test]
    fn fake_binary_success_writes_the_proof_file_and_captures_stage_walls() {
        let home = fake_home("succeeds.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let out_file = out_dir.path().join("proof");
        let proof = prover(&home, Duration::from_secs(5))
            .prove(Path::new("elf"), Path::new("input.bin"), &out_file)
            .expect("should succeed");
        assert_eq!(proof.path, out_file);
        let written = std::fs::read(&out_file).expect("proof file written");
        assert_eq!(written[0], 0x01);
        assert!(proof.walls.verified);
        assert_eq!(proof.walls.steps, Some(4096));
        assert_eq!(proof.walls.wrapper_snark_ms, Some(11238));
        assert!((proof.walls.proof_generated_secs.unwrap() - 0.123).abs() < 1e-9);
    }

    #[test]
    fn a_verified_exit_that_never_wrote_the_output_file_is_refused_by_name() {
        let home = fake_home("no-output.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let out_file = out_dir.path().join("proof");
        let err = prover(&home, Duration::from_secs(5))
            .prove(Path::new("elf"), Path::new("input.bin"), &out_file)
            .unwrap_err();
        assert!(
            matches!(err, ProveError::OutputMissing { .. }),
            "expected OutputMissing, got {err:?}"
        );
    }

    /// A proof killed mid-write leaves a partial `-o` file behind;
    /// `prove()` must delete it before returning `ProveTimeout` — a resume gate that only checks file
    /// EXISTENCE (never content) must never mistake this leftover for a real, resumable proof.
    #[test]
    fn a_timeout_deletes_the_partial_output_file_it_left_behind() {
        let home = fake_home("partial-output-then-hangs.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let out_file = out_dir.path().join("proof");
        let timeout = Duration::from_millis(200);
        let err = prover(&home, timeout)
            .prove(Path::new("elf"), Path::new("input.bin"), &out_file)
            .unwrap_err();
        assert!(
            matches!(err, ProveError::ProveTimeout(_)),
            "expected ProveTimeout, got {err:?}"
        );
        assert!(
            !out_file.exists(),
            "the partial output file must be removed on timeout, not left for a resume gate to find"
        );
    }

    /// Same cleanup on the failed-exit path: a subprocess that wrote a partial `-o` file and then
    /// exited non-zero must not leave it behind.
    #[test]
    fn a_failed_prove_deletes_the_partial_output_file() {
        let home = fake_home("partial-output-then-fails.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let out_file = out_dir.path().join("proof");
        let err = prover(&home, Duration::from_secs(5))
            .prove(Path::new("elf"), Path::new("input.bin"), &out_file)
            .unwrap_err();
        assert!(
            matches!(err, ProveError::ProveFailed { .. }),
            "expected ProveFailed, got {err:?}"
        );
        assert!(
            !out_file.exists(),
            "the partial output file must be removed on a failed exit"
        );
    }

    /// Same cleanup on the never-verified path: `no-verify.sh` writes an output file and exits 0
    /// without a verified line; that file is not a proof and must not survive.
    #[test]
    fn an_unverified_prove_deletes_the_output_file_it_wrote() {
        let home = fake_home("no-verify.sh");
        let out_dir = tempfile::tempdir().unwrap();
        let out_file = out_dir.path().join("proof");
        let err = prover(&home, Duration::from_secs(5))
            .prove(Path::new("elf"), Path::new("input.bin"), &out_file)
            .unwrap_err();
        assert!(
            matches!(err, ProveError::NotVerified { .. }),
            "expected NotVerified, got {err:?}"
        );
        assert!(
            !out_file.exists(),
            "an unverified output file must be removed, not left for a resume gate"
        );
    }

    #[test]
    fn a_nonexistent_binary_path_is_a_named_spawn_error() {
        let empty_home = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();
        let err = prover(empty_home.path(), Duration::from_secs(5))
            .prove(
                Path::new("elf"),
                Path::new("input.bin"),
                &out_dir.path().join("proof"),
            )
            .unwrap_err();
        assert!(
            matches!(err, ProveError::Spawn { .. }),
            "expected Spawn, got {err:?}"
        );
    }
}
