//! Admission: a bounded queue, not a mempool.
//!
//! Adopted from Nitro `execution/gethexec/sequencer.go`, `DefaultSequencerConfig`:
//! `QueueSize: 1024`, `QueueTimeout: 12 * time.Second`, `NonceCacheSize: 1024`): a bounded FIFO admits
//! txs in arrival order — never a fee-ordered pool (see the crate doc's "No fee ordering" section) — with
//! a per-sender nonce cache that gates admission, and a nonce-gap parking area (Nitro's
//! `nonceFailureCache`, here with an explicit per-entry expiry rather than Nitro's LRU) that releases
//! parked txs in order once the gap fills.
//!
//! Every rejection here is returned to the sender; nothing is ever silently dropped.

use alloy::primitives::{Address, Bytes, TxHash};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use crate::tx::{parse, ParsedTx};

#[derive(Debug, Clone, Copy)]
pub struct AdmissionConfig {
    pub chain_id: u64,
    pub queue_capacity: usize,
    pub queue_timeout: Duration,
    pub park_expiry: Duration,
    pub max_tx_size: usize,
    /// The most nonce-gap entries a single sender may hold parked at
    /// once. Without this, one sender submitting nothing but gap txs (nonce N+50, N+51, ...) can fill the
    /// entire bounded queue by itself, starving every other sender's admission.
    pub max_parked_per_sender: usize,
    /// Bounds the per-sender nonce cache so an unbounded stream of
    /// distinct senders cannot grow it forever. Eviction is safe — the next admission from an evicted
    /// sender simply reseeds from the executor's real nonce, exactly like first sight of any sender.
    pub max_next_nonce_entries: usize,
}

impl Default for AdmissionConfig {
    /// Nitro's defaults (`DefaultSequencerConfig`): queue 1024, 12 s queue timeout, park expiry chosen at
    /// 30 s (nonce-gap parking with expiry).
    fn default() -> Self {
        Self {
            chain_id: 1,
            queue_capacity: 1_024,
            queue_timeout: Duration::from_secs(12),
            park_expiry: Duration::from_secs(30),
            max_tx_size: 128 * 1024,
            max_parked_per_sender: 16,
            max_next_nonce_entries: 100_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("admission queue is full")]
    QueueFull,
    #[error("tx waited in queue past the timeout")]
    QueueTimeout,
    #[error("tx nonce-gap park expired without the gap filling")]
    ParkExpired,
    #[error("duplicate tx hash already admitted")]
    DuplicateTx,
    #[error("wrong chain id: expected {expected}, got {got:?}")]
    WrongChainId { expected: u64, got: Option<u64> },
    #[error("tx too large: max {max} bytes, got {got}")]
    TxTooLarge { max: usize, got: usize },
    #[error("nonce {got} is stale, sender's next admittable nonce is {expected}")]
    StaleNonce { expected: u64, got: u64 },
    #[error("malformed tx: {0}")]
    Malformed(String),
    #[error("sender already has {max} txs parked behind a nonce gap")]
    TooManyParked { max: usize },
}

/// Whether a just-admitted tx went straight to the ready queue or was parked behind a nonce gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitOutcome {
    Ready,
    Parked,
}

struct Entry {
    parsed: ParsedTx,
    enqueued_at: Instant,
}

/// One tx handed back by [`Admission::drain_ready`], carrying its original admission time
/// — needed so a tx later carried forward via [`Admission::requeue_front`] still times
/// out against the instant it was *first* admitted, not the instant it was re-queued.
///
/// Carries the full [`ParsedTx`], not just its raw bytes: a tx the
/// executor didn't reach (`Executor::execute_sub_block`'s `not_executed`) is fed straight back into
/// [`Admission::requeue_front`] by the sequencer actor, and it already paid the cost of admission's
/// checks — including ECDSA recovery — the first time it was admitted. Carrying the parsed form through
/// means a carried tx never pays that cost twice, however many ticks in a row it gets carried.
#[derive(Debug)]
pub struct DrainedTx {
    pub parsed: ParsedTx,
    pub enqueued_at: Instant,
}

/// The bounded admission queue for one sequencer. Not thread-safe by itself — the sequencer wiring owns
/// one `Admission` behind a single task/actor (see `sequencer.rs`).
pub struct Admission {
    config: AdmissionConfig,
    ready: VecDeque<Entry>,
    /// Per-sender, keyed by nonce: txs admitted out of order, waiting for the gap to fill.
    parked: HashMap<Address, BTreeMap<u64, (Entry, Instant)>>,
    /// The next nonce this admission layer will accept for a sender, seeded from the executor on first
    /// sight. This is an admission-time gate, independent of (and checked again by) the executor.
    /// LRU-bounded: eviction is safe because the next admission from an
    /// evicted sender simply reseeds from the executor, exactly like first sight of any sender.
    next_nonce: lru::LruCache<Address, u64>,
    /// Hashes currently held (ready or parked) or inside the recent-inclusion dedup horizon below, for
    /// O(1) duplicate rejection.
    seen: HashSet<TxHash>,
    /// A two-block dedup horizon of recently *included* hashes.
    /// `included_ring[0]` accumulates the block in progress; `included_ring[1]` is the block before it.
    /// A hash is folded in here (rather than dropped from `seen`) the instant it's included, so a
    /// resubmission of the exact same hash shortly after inclusion is still caught as a duplicate; once a
    /// hash ages out of both buckets (past 2 block boundaries) it is finally dropped from `seen`, which
    /// is what keeps `seen` from growing forever as the chain runs.
    included_ring: [HashSet<TxHash>; 2],
}

impl Admission {
    pub fn new(config: AdmissionConfig) -> Self {
        let nonce_cap = NonZeroUsize::new(config.max_next_nonce_entries.max(1)).unwrap();
        Self {
            config,
            ready: VecDeque::new(),
            parked: HashMap::new(),
            next_nonce: lru::LruCache::new(nonce_cap),
            seen: HashSet::new(),
            included_ring: [HashSet::new(), HashSet::new()],
        }
    }

    /// Total hashes `seen` is currently holding (ready + parked + the recent-inclusion dedup horizon) —
    /// exposed for the bounded-memory test.
    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }

    /// Total txs currently held (ready + parked) — the number the capacity check is against.
    pub fn len(&self) -> usize {
        self.ready.len() + self.parked.values().map(|m| m.len()).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Admit one raw tx. `seed_nonce` is called at most once per sender (on first sight) to seed the
    /// admission-time nonce cache from the executor's real state.
    pub fn admit(
        &mut self,
        raw: Bytes,
        now: Instant,
        seed_nonce: impl FnOnce(Address) -> u64,
    ) -> Result<(AdmitOutcome, TxHash), AdmissionError> {
        if raw.len() > self.config.max_tx_size {
            return Err(AdmissionError::TxTooLarge {
                max: self.config.max_tx_size,
                got: raw.len(),
            });
        }
        let parsed = parse(raw).map_err(|e| AdmissionError::Malformed(e.to_string()))?;
        if parsed.chain_id != Some(self.config.chain_id) {
            return Err(AdmissionError::WrongChainId {
                expected: self.config.chain_id,
                got: parsed.chain_id,
            });
        }
        if self.seen.contains(&parsed.tx_hash) {
            return Err(AdmissionError::DuplicateTx);
        }
        if self.len() >= self.config.queue_capacity {
            return Err(AdmissionError::QueueFull);
        }

        let expected = match self.next_nonce.get(&parsed.sender) {
            Some(v) => *v,
            None => {
                let v = seed_nonce(parsed.sender);
                self.next_nonce.put(parsed.sender, v);
                v
            }
        };
        if parsed.nonce < expected {
            return Err(AdmissionError::StaleNonce {
                expected,
                got: parsed.nonce,
            });
        }

        // Bound how many gap txs a single sender can hold parked, so one
        // sender submitting nothing but gap txs cannot fill the whole bounded queue by itself.
        if parsed.nonce > expected {
            let already_parked = self.parked.get(&parsed.sender).map_or(0, |b| b.len());
            if already_parked >= self.config.max_parked_per_sender {
                return Err(AdmissionError::TooManyParked {
                    max: self.config.max_parked_per_sender,
                });
            }
        }

        self.seen.insert(parsed.tx_hash);
        let tx_hash = parsed.tx_hash;
        let entry = Entry {
            parsed,
            enqueued_at: now,
        };

        if entry.parsed.nonce == expected {
            let sender = entry.parsed.sender;
            self.ready.push_back(entry);
            self.next_nonce.put(sender, expected + 1);
            self.release_parked(sender, now);
            Ok((AdmitOutcome::Ready, tx_hash))
        } else {
            let deadline = now + self.config.park_expiry;
            let sender = entry.parsed.sender;
            let nonce = entry.parsed.nonce;
            self.parked
                .entry(sender)
                .or_default()
                .insert(nonce, (entry, deadline));
            Ok((AdmitOutcome::Parked, tx_hash))
        }
    }

    /// After the executor rejects a tx: admission's own nonce cache
    /// already advanced `sender` past this tx on the assumption it would succeed, but the executor's real
    /// nonce did not move. Reset `next_nonce[sender]` to the executor's authoritative value and
    /// re-evaluate the sender's parked entries against it — those now contiguous release into `ready` in
    /// order, the rest stay parked. Without this, every later tx from `sender` is judged against a nonce
    /// that never actually advanced and parks forever (past `park_expiry`, permanently locked out).
    pub fn reconcile_sender_nonce(&mut self, sender: Address, real_nonce: u64, now: Instant) {
        self.next_nonce.put(sender, real_nonce);
        self.release_parked(sender, now);
    }

    /// The executor rejected `drained` as `Reason::NonceTooHigh` — a
    /// genuine future nonce, not a tx to bounce back to its sender. Parks it exactly like a fresh
    /// out-of-gap admission (same bucket, same expiry, no admission checks re-run — it already passed
    /// them), so it releases automatically once the gap fills; the sender is never told it was
    /// rejected. Does NOT touch `next_nonce` — a `NonceTooHigh` rejection says nothing wrong with the
    /// sender's LOWER nonces, so there is nothing to reconcile.
    ///
    /// Returns `drained` back to the caller (to fall back to the ordinary reject-to-sender path) if
    /// `max_parked_per_sender` is already at capacity for this sender — this must never silently
    /// exceed the same per-sender bound a fresh admission is held to.
    pub fn park_rejected(
        &mut self,
        drained: DrainedTx,
        now: Instant,
    ) -> Result<(), Box<DrainedTx>> {
        let sender = drained.parsed.sender;
        let nonce = drained.parsed.nonce;
        let already_parked = self.parked.get(&sender).map_or(0, |b| b.len());
        if already_parked >= self.config.max_parked_per_sender {
            return Err(Box::new(drained));
        }
        let deadline = now + self.config.park_expiry;
        self.parked.entry(sender).or_default().insert(
            nonce,
            (
                Entry {
                    parsed: drained.parsed,
                    enqueued_at: drained.enqueued_at,
                },
                deadline,
            ),
        );
        Ok(())
    }

    /// Called once per sealed sub-block with the hashes the executor
    /// actually included or rejected this tick. A rejected hash is no longer held by admission at all and
    /// is dropped from `seen` outright; an included hash is folded into the current block's dedup-horizon
    /// bucket (it stays `seen` for roughly the next 2 blocks, so an immediate resubmission of the exact
    /// same hash is still caught, without keeping every included hash ever seen forever).
    pub fn on_sub_block_sealed(&mut self, included: &[TxHash], rejected: &[TxHash]) {
        for hash in rejected {
            self.seen.remove(hash);
        }
        for hash in included {
            self.included_ring[0].insert(*hash);
        }
    }

    /// Called once per block boundary (every 20th sub-block). Rotates the two-block dedup horizon,
    /// evicting the oldest bucket's hashes from `seen` — this is what actually bounds `seen`'s size over the life of
    /// the process.
    pub fn on_block_sealed(&mut self) {
        let evicted = std::mem::take(&mut self.included_ring[1]);
        for hash in &evicted {
            self.seen.remove(hash);
        }
        self.included_ring[1] = std::mem::take(&mut self.included_ring[0]);
    }

    /// After admitting `sender`'s nonce up to `next_nonce[sender] - 1`, move any now-contiguous parked
    /// txs into the ready queue, in nonce order.
    fn release_parked(&mut self, sender: Address, now: Instant) {
        loop {
            let expected = self.next_nonce.get(&sender).copied().unwrap_or(0);
            let Some(bucket) = self.parked.get_mut(&sender) else {
                break;
            };
            let Some((entry, _deadline)) = bucket.remove(&expected) else {
                break;
            };
            self.ready.push_back(Entry {
                parsed: entry.parsed,
                enqueued_at: entry.enqueued_at,
            });
            self.next_nonce.put(sender, expected + 1);
            if bucket.is_empty() {
                self.parked.remove(&sender);
            }
            let _ = now; // parked-duration metrics could be derived here later
        }
    }

    /// Drain every currently-ready tx (called by the sealer once per sub-block tick), each tagged with
    /// its original admission time: a tx later carried forward via
    /// [`Self::requeue_front`] must keep timing out against when it was *first* admitted, not when it was
    /// re-queued.
    pub fn drain_ready(&mut self) -> Vec<DrainedTx> {
        self.ready
            .drain(..)
            .map(|e| DrainedTx {
                parsed: e.parsed,
                enqueued_at: e.enqueued_at,
            })
            .collect()
    }

    /// Re-insert txs the executor did not reach last tick (`SubBlockOutcome::not_executed`) at the
    /// **front** of the ready queue, in order, ahead of anything admitted since, preserving each tx's
    /// **original** `enqueued_at` (resetting it here would let a tx carried forward tick after tick dodge
    /// `queue_timeout` forever).
    /// These already passed every admission check the first time they were admitted, including ECDSA
    /// recovery — `DrainedTx` carries the already-`ParsedTx`, so this is an O(1)
    /// re-insertion with no re-parse, however many ticks in a row a tx gets carried forward. In
    /// particular this never runs the duplicate-hash check either, since a carried tx's hash is
    /// (correctly) still marked `seen`.
    pub fn requeue_front(&mut self, not_executed: Vec<DrainedTx>) {
        for drained in not_executed.into_iter().rev() {
            self.ready.push_front(Entry {
                parsed: drained.parsed,
                enqueued_at: drained.enqueued_at,
            });
        }
    }

    /// Peek without draining — used by metrics (queue depth).
    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// Expire anything that has waited too long: ready-queue entries past `queue_timeout`, and parked
    /// entries past `park_expiry`. Returns the hashes and reasons so the caller can notify senders —
    /// admission never silently drops.
    pub fn expire(&mut self, now: Instant) -> Vec<(TxHash, AdmissionError)> {
        let mut expired = Vec::new();

        while let Some(front) = self.ready.front() {
            if now.duration_since(front.enqueued_at) >= self.config.queue_timeout {
                let entry = self.ready.pop_front().unwrap();
                self.seen.remove(&entry.parsed.tx_hash);
                expired.push((entry.parsed.tx_hash, AdmissionError::QueueTimeout));
            } else {
                break;
            }
        }

        let mut empty_senders = Vec::new();
        for (sender, bucket) in self.parked.iter_mut() {
            let expired_nonces: Vec<u64> = bucket
                .iter()
                .filter(|(_, (_, deadline))| now >= *deadline)
                .map(|(nonce, _)| *nonce)
                .collect();
            for nonce in expired_nonces {
                if let Some((entry, _)) = bucket.remove(&nonce) {
                    self.seen.remove(&entry.parsed.tx_hash);
                    expired.push((entry.parsed.tx_hash, AdmissionError::ParkExpired));
                }
            }
            if bucket.is_empty() {
                empty_senders.push(*sender);
            }
        }
        for sender in empty_senders {
            self.parked.remove(&sender);
        }

        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::signed_raw_tx;
    use alloy::signers::local::PrivateKeySigner;

    fn config(capacity: usize) -> AdmissionConfig {
        AdmissionConfig {
            chain_id: 1,
            queue_capacity: capacity,
            ..AdmissionConfig::default()
        }
    }

    #[test]
    fn full_queue_rejects_with_error_not_silent_drop() {
        let mut a = Admission::new(config(2));
        let s1 = PrivateKeySigner::random();
        let s2 = PrivateKeySigner::random();
        let s3 = PrivateKeySigner::random();
        let now = Instant::now();
        assert_eq!(
            a.admit(signed_raw_tx(&s1, 1, 0), now, |_| 0).unwrap().0,
            AdmitOutcome::Ready
        );
        assert_eq!(
            a.admit(signed_raw_tx(&s2, 1, 0), now, |_| 0).unwrap().0,
            AdmitOutcome::Ready
        );
        let err = a.admit(signed_raw_tx(&s3, 1, 0), now, |_| 0).unwrap_err();
        assert_eq!(err, AdmissionError::QueueFull);
    }

    #[test]
    fn nonce_gap_parks_then_releases_in_order() {
        let mut a = Admission::new(config(10));
        let sender = PrivateKeySigner::random();
        let now = Instant::now();

        // Submit nonce 2 first: no nonce 0/1 seen yet, so it parks.
        let (outcome, _) = a.admit(signed_raw_tx(&sender, 1, 2), now, |_| 0).unwrap();
        assert_eq!(outcome, AdmitOutcome::Parked);
        assert!(a.drain_ready().is_empty());

        // Submit nonce 1: still a gap (0 missing), parks too.
        let (outcome, _) = a.admit(signed_raw_tx(&sender, 1, 1), now, |_| 0).unwrap();
        assert_eq!(outcome, AdmitOutcome::Parked);

        // Submit nonce 0: fills the gap — 0, then the parked 1 and 2, release in order.
        let (outcome, _) = a.admit(signed_raw_tx(&sender, 1, 0), now, |_| 0).unwrap();
        assert_eq!(outcome, AdmitOutcome::Ready);

        let released = a.drain_ready();
        assert_eq!(released.len(), 3);
        let nonces: Vec<u64> = released.iter().map(|d| d.parsed.nonce).collect();
        assert_eq!(
            nonces,
            vec![0, 1, 2],
            "parked txs must release in nonce order"
        );
    }

    #[test]
    fn parked_tx_expires_after_park_expiry() {
        let mut config = config(10);
        config.park_expiry = Duration::from_millis(1);
        let mut a = Admission::new(config);
        let sender = PrivateKeySigner::random();
        let now = Instant::now();

        a.admit(signed_raw_tx(&sender, 1, 5), now, |_| 0).unwrap(); // parks (gap)
        assert_eq!(a.len(), 1);

        let later = now + Duration::from_millis(2);
        let expired = a.expire(later);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].1, AdmissionError::ParkExpired);
        assert_eq!(
            a.len(),
            0,
            "an expired park entry must be removed, not left dangling"
        );
    }

    #[test]
    fn duplicate_tx_hash_rejected() {
        let mut a = Admission::new(config(10));
        let sender = PrivateKeySigner::random();
        let now = Instant::now();
        let raw = signed_raw_tx(&sender, 1, 0);
        a.admit(raw.clone(), now, |_| 0).unwrap();
        let err = a.admit(raw, now, |_| 0).unwrap_err();
        assert_eq!(err, AdmissionError::DuplicateTx);
    }

    #[test]
    fn wrong_chain_id_rejected() {
        let mut a = Admission::new(config(10)); // configured for chain_id 1
        let sender = PrivateKeySigner::random();
        let raw = signed_raw_tx(&sender, 999, 0);
        let err = a.admit(raw, Instant::now(), |_| 0).unwrap_err();
        assert_eq!(
            err,
            AdmissionError::WrongChainId {
                expected: 1,
                got: Some(999)
            }
        );
    }

    #[test]
    fn ready_queue_timeout_expires_oldest_first() {
        let mut config = config(10);
        config.queue_timeout = Duration::from_millis(1);
        let mut a = Admission::new(config);
        let sender = PrivateKeySigner::random();
        let now = Instant::now();
        a.admit(signed_raw_tx(&sender, 1, 0), now, |_| 0).unwrap();
        assert_eq!(a.ready_len(), 1);

        let later = now + Duration::from_millis(5);
        let expired = a.expire(later);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].1, AdmissionError::QueueTimeout);
        assert_eq!(a.ready_len(), 0);
    }

    #[test]
    fn seeded_nonce_is_only_requested_once_per_sender() {
        let mut a = Admission::new(config(10));
        let sender = PrivateKeySigner::random();
        let now = Instant::now();
        let mut seed_calls = 0;
        a.admit(signed_raw_tx(&sender, 1, 5), now, |_| {
            seed_calls += 1;
            5
        })
        .unwrap();
        a.admit(signed_raw_tx(&sender, 1, 6), now, |_| {
            seed_calls += 1;
            5
        })
        .unwrap();
        assert_eq!(
            seed_calls, 1,
            "seed_nonce must only be consulted on first sight of a sender"
        );
    }

    /// Txs the executor didn't reach (`not_executed`) go back to the
    /// FRONT of the ready queue, in order, ahead of anything admitted since — and without re-running
    /// admission checks, since they already passed those the first time (in particular: they must not be
    /// rejected as duplicates, even though their hash is still marked `seen`).
    #[test]
    fn requeue_front_reinserts_ahead_of_newly_admitted_txs_in_order() {
        let mut a = Admission::new(config(10));
        let carried_sender = PrivateKeySigner::random();
        let new_sender = PrivateKeySigner::random();
        let now = Instant::now();

        // These were admitted, drained for a tick, but the executor didn't reach them (gas/deadline cut
        // off before they were evaluated) — never included, never rejected, just not reached.
        let carried_raw: Vec<_> = (0..3u64)
            .map(|n| signed_raw_tx(&carried_sender, 1, n))
            .collect();
        let carried_entries: Vec<DrainedTx> = carried_raw
            .iter()
            .map(|raw| DrainedTx {
                parsed: parse(raw.clone()).unwrap(),
                enqueued_at: now,
            })
            .collect();

        // Meanwhile a new tx was admitted normally for the next tick.
        a.admit(signed_raw_tx(&new_sender, 1, 0), now, |_| 0)
            .unwrap();

        a.requeue_front(carried_entries);

        let drained = a.drain_ready();
        assert_eq!(
            drained.len(),
            4,
            "the 3 carried txs plus the 1 newly admitted one"
        );
        let drained_raw: Vec<Bytes> = drained.iter().map(|d| d.parsed.raw.clone()).collect();
        assert_eq!(
            &drained_raw[..3],
            &carried_raw[..],
            "carried txs must come first, in their original order"
        );
    }

    /// `requeue_front` must not re-parse a carried tx — it already
    /// passed admission's checks (including ECDSA recovery) the first time. Proved here by carrying a
    /// `DrainedTx` whose `raw` bytes are deliberately malformed (would fail `crate::tx::parse`), but
    /// whose `parsed` metadata (sender/nonce/hash) is otherwise valid: under the old
    /// reparse-on-requeue behavior this entry would be silently dropped (the `Ok(parsed) = parse(...)`
    /// guard would fail and skip it); with the parsed tx carried through directly, it must survive
    /// unchanged.
    #[test]
    fn requeue_front_does_not_reparse_and_survives_malformed_raw_bytes() {
        let mut a = Admission::new(config(10));
        let sender = Address::repeat_byte(0x11);
        let tx_hash = TxHash::repeat_byte(0x22);
        let malformed_raw = Bytes::from_static(&[0xFF, 0x00, 0x01]); // fails crate::tx::parse
        assert!(
            parse(malformed_raw.clone()).is_err(),
            "fixture must actually be unparseable"
        );
        let parsed = ParsedTx {
            raw: malformed_raw.clone(),
            tx_hash,
            sender,
            nonce: 7,
            chain_id: Some(1),
        };
        let now = Instant::now();
        a.requeue_front(vec![DrainedTx {
            parsed,
            enqueued_at: now,
        }]);

        let drained = a.drain_ready();
        assert_eq!(
            drained.len(),
            1,
            "a carried tx with malformed raw bytes must not be silently dropped — requeue_front must \
             not reparse it"
        );
        assert_eq!(drained[0].parsed.raw, malformed_raw);
        assert_eq!(drained[0].parsed.tx_hash, tx_hash);
        assert_eq!(drained[0].parsed.sender, sender);
        assert_eq!(drained[0].parsed.nonce, 7);
        assert_eq!(drained[0].enqueued_at, now);
    }

    /// A tx carried forward (requeued at the front) must keep timing
    /// out against its **original** admission instant, not the instant it was re-queued — otherwise a tx
    /// repeatedly carried tick after tick could dodge `queue_timeout` forever.
    #[test]
    fn requeued_tx_still_expires_from_its_original_enqueue_time() {
        let mut config = config(10);
        config.queue_timeout = Duration::from_millis(50);
        let mut a = Admission::new(config);
        let sender = PrivateKeySigner::random();
        let original_admit_time = Instant::now();

        a.admit(signed_raw_tx(&sender, 1, 0), original_admit_time, |_| 0)
            .unwrap();
        let drained = a.drain_ready();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].enqueued_at, original_admit_time);

        // Carried forward well after the original admission — if requeue_front reset enqueued_at to
        // "now", the timeout below would never fire.
        let carry_time = original_admit_time + Duration::from_millis(40);
        a.requeue_front(drained);

        // Past the timeout measured from the ORIGINAL admission time, not the carry time.
        let past_timeout = original_admit_time + Duration::from_millis(60);
        assert!(carry_time < past_timeout);
        let expired = a.expire(past_timeout);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].1, AdmissionError::QueueTimeout);
    }

    /// One sender submitting nothing but nonce-gap txs must not be able to
    /// fill the whole bounded queue by itself.
    #[test]
    fn per_sender_parked_cap_rejects_beyond_the_configured_limit() {
        let mut config = config(1_000);
        config.max_parked_per_sender = 3;
        let mut a = Admission::new(config);
        let sender = PrivateKeySigner::random();
        let now = Instant::now();

        for nonce in 1..=3u64 {
            let outcome = a
                .admit(signed_raw_tx(&sender, 1, nonce), now, |_| 0)
                .unwrap()
                .0;
            assert_eq!(outcome, AdmitOutcome::Parked);
        }

        let err = a
            .admit(signed_raw_tx(&sender, 1, 4), now, |_| 0)
            .unwrap_err();
        assert_eq!(err, AdmissionError::TooManyParked { max: 3 });
    }

    /// After the executor rejects a tx, admission's nonce cache must be
    /// reset to the executor's real value, and the sender's parked entries re-evaluated against it —
    /// released in a single call if the reconciled value now makes them contiguous, with no further
    /// admission needed.
    #[test]
    fn reconcile_sender_nonce_releases_now_contiguous_parked_entries() {
        let mut a = Admission::new(config(10));
        let sender = PrivateKeySigner::random();
        let now = Instant::now();

        // Admission seeds this sender's expected nonce at 0 (this test's seed closure), but nonce 5 is
        // submitted first — parks behind a gap admission cannot yet see is fillable.
        let (outcome, _) = a.admit(signed_raw_tx(&sender, 1, 5), now, |_| 0).unwrap();
        assert_eq!(outcome, AdmitOutcome::Parked);

        // The executor's real nonce for this sender was already 5 (admission's seed of 0 was stale) —
        // reconciling to it must release the now-contiguous parked entry directly, with no further
        // admission call needed.
        a.reconcile_sender_nonce(sender.address(), 5, now);

        let released = a.drain_ready();
        assert_eq!(
            released.len(),
            1,
            "reconcile alone must release the now-contiguous parked entry"
        );
    }

    /// A tx the executor rejected `NonceTooHigh` (a genuine future
    /// nonce) must be kept parked — released automatically, in nonce order, once the gap fills via a
    /// normal admission — never bounced back to a caller as terminally rejected.
    #[test]
    fn park_rejected_keeps_a_future_nonce_parked_and_releases_it_once_the_gap_fills() {
        let mut a = Admission::new(config(10));
        let sender = PrivateKeySigner::random();
        let now = Instant::now();

        // Simulates what the sequencer actor does: a nonce-1 tx was admitted Ready (admission's cache
        // already expected 1) but the executor rejected it NonceTooHigh (the real state is still at
        // 0) — the actor hands the SAME already-parsed tx to `park_rejected` instead of replying
        // rejected to its sender.
        let raw = signed_raw_tx(&sender, 1, 1);
        let parsed = parse(raw).unwrap();
        let drained = DrainedTx {
            parsed,
            enqueued_at: now,
        };
        a.park_rejected(drained, now).unwrap();
        assert_eq!(a.len(), 1, "the tx must still be held, just parked");
        assert!(
            a.drain_ready().is_empty(),
            "a parked tx must not be ready yet"
        );

        // The gap fills: nonce 0 is admitted normally.
        let (outcome, _) = a.admit(signed_raw_tx(&sender, 1, 0), now, |_| 0).unwrap();
        assert_eq!(outcome, AdmitOutcome::Ready);

        let released = a.drain_ready();
        assert_eq!(
            released.len(),
            2,
            "nonce 0 and the previously-parked nonce 1 must both release, in order"
        );
        assert_eq!(released[0].parsed.nonce, 0);
        assert_eq!(released[1].parsed.nonce, 1);
    }

    /// `park_rejected` must respect the same
    /// `max_parked_per_sender` bound a fresh admission is held to — never silently exceed it.
    #[test]
    fn park_rejected_respects_the_per_sender_parked_cap() {
        let mut config = config(1_000);
        config.max_parked_per_sender = 1;
        let mut a = Admission::new(config);
        let sender = PrivateKeySigner::random();
        let now = Instant::now();

        let raw1 = signed_raw_tx(&sender, 1, 1);
        let parsed1 = parse(raw1).unwrap();
        a.park_rejected(
            DrainedTx {
                parsed: parsed1,
                enqueued_at: now,
            },
            now,
        )
        .unwrap();

        let raw2 = signed_raw_tx(&sender, 1, 2);
        let parsed2 = parse(raw2).unwrap();
        let drained2 = DrainedTx {
            parsed: parsed2,
            enqueued_at: now,
        };
        let err = a.park_rejected(drained2, now).unwrap_err();
        assert_eq!(
            err.parsed.nonce, 2,
            "over the cap, the tx must be handed back to the caller unchanged"
        );
    }

    /// `seen` must not grow without bound as distinct senders come and
    /// go. 10,000 distinct senders, each admitted, drained, and reported included+sealed one sub-block at
    /// a time (20 sub-blocks per block, the default cadence), must leave `seen` bounded by roughly the
    /// queue capacity plus the 2-block dedup horizon — nowhere near 10,000.
    #[test]
    fn seen_stays_bounded_across_ten_thousand_distinct_senders() {
        let mut a = Admission::new(config(1_024));
        let now = Instant::now();

        for i in 0..10_000u64 {
            let signer = PrivateKeySigner::random();
            let raw = signed_raw_tx(&signer, 1, 0);
            let (_, tx_hash) = a.admit(raw, now, |_| 0).unwrap();
            let drained = a.drain_ready();
            assert_eq!(drained.len(), 1);
            a.on_sub_block_sealed(&[tx_hash], &[]);
            if (i + 1) % 20 == 0 {
                a.on_block_sealed();
            }
        }

        assert!(
            a.seen_len() <= 1_024 + 200,
            "seen_len() {} must stay bounded, not grow to 10,000",
            a.seen_len()
        );
    }

    /// The `next_nonce` LRU is bounded — an evicted sender's next
    /// admission must reseed from the executor rather than erroring, since eviction only drops
    /// admission's cached copy, never anything the executor itself tracks.
    #[test]
    fn next_nonce_lru_evicts_and_reseeds_from_the_executor() {
        let mut config = config(10);
        config.max_next_nonce_entries = 2;
        let mut a = Admission::new(config);
        let s1 = PrivateKeySigner::random();
        let s2 = PrivateKeySigner::random();
        let s3 = PrivateKeySigner::random();

        let now = Instant::now();
        a.admit(signed_raw_tx(&s1, 1, 0), now, |_| 0).unwrap();
        a.admit(signed_raw_tx(&s2, 1, 0), now, |_| 0).unwrap();
        // A third distinct sender evicts s1 from the bounded (capacity-2) LRU.
        a.admit(signed_raw_tx(&s3, 1, 0), now, |_| 0).unwrap();

        let mut reseeded = false;
        a.admit(signed_raw_tx(&s1, 1, 7), now, |_| {
            reseeded = true;
            7
        })
        .unwrap();
        assert!(
            reseeded,
            "an evicted sender's next admission must reseed from the executor"
        );
    }
}
