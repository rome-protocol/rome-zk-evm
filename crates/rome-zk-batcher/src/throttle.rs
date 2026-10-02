//! The backpressure seam (it lowers admission, never drops). The batcher never drops a
//! sub-block or a tx to relieve its own pressure — if it is falling behind the sequencer (the ordered log
//! is growing faster than batches post and finalize), the only lever is to ask the sequencer to admit less
//! new work, never to skip logging or posting anything already committed.
//!
//! Only the no-op implementation exists so far; wiring a real signal (e.g. an HTTP/gRPC call into
//! `rome-zk-sequencer`'s admission queue, or a shared atomic the sequencer's admission loop reads) is a
//! documented follow-up — not implemented yet.

/// A pressure signal the pipeline reports after every batch attempt. `Throttle` implementations turn this
/// into whatever admission-lowering action the sequencer side exposes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    /// How many blocks are sitting in the ordered log, sourced but not yet part of a finalized batch —
    /// the backlog this signal is measuring.
    pub blocks_backlogged: u64,
    /// How many consecutive batch attempts have failed (re-derive mismatch, send failure, or timeout)
    /// since the last success — a repeatedly-failing pipeline should ask for less new work, not just a
    /// growing backlog.
    pub consecutive_failures: u32,
}

/// The backpressure seam. Never drops anything itself — see the module doc.
pub trait Throttle: Send + Sync {
    /// Reports current pressure; implementations decide whether/how to act (e.g. lower an admission
    /// rate-limit). Must not block or panic — this is called on the pipeline's hot path after every batch
    /// attempt.
    fn report(&self, pressure: Pressure);
}

/// The only implementation so far: observes pressure and does nothing about it. A real seam
/// (HTTP call, shared atomic, message channel into the sequencer's admission loop) is a follow-up.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpThrottle;

impl Throttle for NoOpThrottle {
    fn report(&self, _pressure: Pressure) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_throttle_accepts_any_pressure_without_panicking() {
        let t = NoOpThrottle;
        t.report(Pressure {
            blocks_backlogged: 0,
            consecutive_failures: 0,
        });
        t.report(Pressure {
            blocks_backlogged: u64::MAX,
            consecutive_failures: u32::MAX,
        });
    }
}
