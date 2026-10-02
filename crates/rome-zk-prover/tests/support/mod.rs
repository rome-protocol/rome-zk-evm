//! Shared test support for `rome-zk-prover`'s own Postgres integration tests: a throwaway `postgres:16`
//! Docker container per test, copied verbatim from `rome-zk-settlement-watcher/tests/support/mod.rs`
//! — same label (`rome-zk-test=1`, so CI's existing leaked-container sweep at
//! `.github/workflows/ci.yml` finds this crate's containers too), same self-destructing `timeout`
//! entrypoint, same random-host-port discovery. The only difference is the migrations directory:
//! `../../migrations/prover` (this crate's own schema), not
//! `../../migrations/explorer`.
#![allow(dead_code)]

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::process::Command;
use std::time::Duration;

pub struct TestPg {
    container: String,
    pub url: String,
    pub pool: PgPool,
}

impl TestPg {
    pub async fn start() -> Self {
        // No `--name`: concurrent `#[tokio::test]`s in one process would collide on a fixed name: see
        // the settlement-watcher harness's own note. Docker assigns each `run` a unique container ID
        // regardless of concurrency.
        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                "rome-zk-test=1",
                "-e",
                "POSTGRES_PASSWORD=fixture-not-a-secret",
                "-e",
                "POSTGRES_DB=rome_zk_prover_test",
                "-P",
                "--entrypoint",
                "sh",
                "postgres:16",
                "-c",
                "exec timeout 600 docker-entrypoint.sh postgres",
            ])
            .output()
            .expect(
                "docker run -- is Docker installed and running? (the test runner needs Docker)",
            );
        assert!(
            out.status.success(),
            "docker run postgres:16 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let container = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            !container.is_empty(),
            "docker run printed no container ID on stdout"
        );

        let port = Self::discover_port(&container);
        let url = format!(
            "postgres://postgres:fixture-not-a-secret@127.0.0.1:{port}/rome_zk_prover_test"
        );
        let pool = Self::wait_for_ready(&url).await;

        Self {
            container,
            url,
            pool,
        }
    }

    fn discover_port(container: &str) -> u16 {
        for _ in 0..20 {
            if let Ok(out) = Command::new("docker")
                .args(["port", container, "5432/tcp"])
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(port_str) = text.trim().rsplit(':').next() {
                    if let Ok(port) = port_str.trim().parse::<u16>() {
                        return port;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("could not discover the published port for container {container}");
    }

    async fn wait_for_ready(url: &str) -> PgPool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match PgPoolOptions::new()
                .max_connections(5)
                .acquire_timeout(Duration::from_secs(2))
                .connect(url)
                .await
            {
                Ok(pool) => return pool,
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::debug!("waiting for test postgres: {e}");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(e) => panic!("postgres never became ready: {e}"),
            }
        }
    }
}

impl Drop for TestPg {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.container]) // -v: postgres:16 declares a data volume; without it every test leaks one
            .output();
    }
}
