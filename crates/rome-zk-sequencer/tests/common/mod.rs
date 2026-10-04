//! Shared by the end-to-end tests that start the real `rome-zk-sequencer` binary.
//!
//! Every address in those tests' configs is port 0, so the OS picks each port at bind time and the
//! node prints what it bound in its startup line. The tests read the ports from that line instead of
//! guessing a free port first: a port that is probed and released can be taken by another server on
//! a busy machine before the node binds it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// The ports the node reported in its startup line.
pub struct BoundPorts {
    pub rpc: u16,
    pub metrics: u16,
}

/// Parses the startup line, in either of its two forms:
/// `listening on <rpc>, metrics on <metrics>` and
/// `listening on http <http> / ws <ws>, metrics on <metrics>` (the RPC port is then the http one).
fn parse_listening_line(line: &str) -> Option<BoundPorts> {
    let rest = line.split_once("listening on ")?.1;
    let (rpc, metrics) = if let Some(rest) = rest.strip_prefix("http ") {
        let (http, rest) = rest.split_once(" / ws ")?;
        (http, rest.split_once(", metrics on ")?.1)
    } else {
        rest.split_once(", metrics on ")?
    };
    Some(BoundPorts {
        rpc: port_of(rpc.trim()),
        metrics: port_of(metrics.trim()),
    })
}

fn port_of(addr: &str) -> u16 {
    addr.rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in reported address {addr:?}"))
}

/// Spawns `cmd` (the sequencer binary with its arguments) and blocks until it has bound its servers
/// and printed where; returns the child and the ports it reported. Panics, killing the child, if the
/// process exits or hangs before reporting.
pub fn spawn_reporting_ports(cmd: &mut Command) -> (Child, BoundPorts) {
    let mut child = cmd
        // The startup line is an info-level log (the default filter shows errors only), and it must
        // be free of colour codes to parse as plain text.
        .env("RUST_LOG", "error,rome_zk_sequencer=info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn rome-zk-sequencer binary");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    // Echo every line and keep draining until the process exits, so a full pipe can never stall the
    // node; report the ports once, from the first matching line.
    std::thread::spawn(move || {
        let mut reported = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("{line}");
            if !reported {
                if let Some(ports) = parse_listening_line(&line) {
                    reported = true;
                    let _ = tx.send(ports);
                }
            }
        }
    });
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(ports) => (child, ports),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the sequencer did not report its bound ports ({e}); it exited or hung before serving")
        }
    }
}
