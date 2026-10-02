//! `release_exit_example_refuses_without_confirm` — the `release-exit` example, run in its DEFAULT mode (no
//! `--confirm`), against an RPC endpoint nothing listens on, exits non-zero with a NAMED error (never a bare panic
//! backtrace) before it could ever reach a send. The example reads `exit_record`/`vault_config` before touching the
//! payer keypair or branching on `--dry-run`/`--confirm` at all, so this same failure fires in EITHER mode —
//! proving an unreachable RPC never lets the tool silently proceed.
//!
//! Shells out to `cargo run --example release-exit --features devnet-driver` rather than invoking `main`
//! in-process: the example's own arg parsing reads `std::env::args()` directly (mirrors
//! `zk-settlement-client/examples/governance.rs`), which only a real separate process can exercise.

use std::process::Command;

#[test]
fn release_exit_example_refuses_without_confirm() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // A DEDICATED, isolated `CARGO_TARGET_DIR` for this nested `cargo run` — never the outer test run's
    // own (inherited via the parent process's environment, e.g. a CI or make wrapper's `CARGO_TARGET_DIR`).
    // Sharing it caused a real, reproducible race: this nested build racing the outer `cargo test`'s own
    // concurrent compilation of `zk_settlement_client` corrupted the outer run's doctest linking
    // (`error[E0463]: can't find crate for zk_settlement_client`) — a genuine cargo multi-process hazard
    // on one target dir, not a bug in either build. `~/.cargo/registry`'s download/build cache is still
    // shared (unaffected by `CARGO_TARGET_DIR`), so this costs a fresh link step, not a fresh download.
    let isolated_target_dir =
        std::env::temp_dir().join(format!("release-exit-example-test-{}", std::process::id()));
    let output = Command::new(env!("CARGO"))
        .current_dir(manifest_dir)
        .env("CARGO_TARGET_DIR", &isolated_target_dir)
        .args([
            "run",
            "--quiet",
            "--example",
            "release-exit",
            "--features",
            "devnet-driver",
            "--",
            "--settlement",
            "11111111111111111111111111111111",
            "--bridge",
            "11111111111111111111111111111111",
            "--chain-id",
            "200101",
            "--message-hash",
            "0x7777777777777777777777777777777777777777777777777777777777777777",
            "--payer-keypair",
            "/nonexistent/payer.json",
            "--rpc-url",
            "http://127.0.0.1:1",
        ])
        .output()
        .expect("spawn `cargo run --example release-exit`");
    let _ = std::fs::remove_dir_all(&isolated_target_dir);

    assert!(
        !output.status.success(),
        "an unreachable RPC must exit non-zero, not silently continue toward a send.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exit_record") && stderr.contains("not found"),
        "the failure must be named (exit_record not found), reached before the payer keypair is ever \
         read (that path is untouched here) and before any --dry-run/--confirm branch: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("-- sent:"),
        "must never reach a send: {stdout}"
    );
}
