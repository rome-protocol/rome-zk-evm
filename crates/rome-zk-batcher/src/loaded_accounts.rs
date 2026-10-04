//! The V1 header's `loaded_accounts_data_size_limit` must be derived from the **live** target program at process start,
//! never trusted as a literal — under SIMD-0186 every loaded account costs 64 B + its own data length, **and a
//! loader-v3 program's `ProgramData` account is counted even though it is never one of a transaction's own
//! `AccountMeta`s.** Tiber's inbox `ProgramData` is 133,077 B, so a chunk-lane transaction actually loads roughly
//! 143–163 KB depending on `max_frames_per_batch` — the 131,072-B (128 KiB) default this crate used to ship was
//! computed from the batch account alone and never accounted for `ProgramData` at all; it fails every batcher
//! transaction on a real cluster (`MaxLoadedAccountsDataSizeExceeded`, live-verified — see `tests/devnet_probe.rs`).
//!
//! [`required_loaded_accounts_data_size`] is the pure formula (trivially unit-testable, no RPC); [`check`]
//! adds the fail-closed comparison against a configured limit, taking an [`AccountLenReader`] so it too
//! stays unit-testable against a fake; [`run`] is the thin async wrapper that reads the two accounts this
//! actually needs from a live RPC endpoint and refuses to let the caller proceed (and spend any fee) if the
//! configured limit is not enough.

// `bpf_loader_upgradeable` moved out of `solana_program`'s root re-export in the
// Agave 4.x line (API fallout) — `id` now lives in `solana_sdk_ids`, `get_program_data_address` in
// `solana_loader_v3_interface`; this local module keeps every existing `bpf_loader_upgradeable::` call
// site below unchanged.
mod bpf_loader_upgradeable {
    pub use solana_loader_v3_interface::get_program_data_address;
    pub use solana_sdk_ids::bpf_loader_upgradeable::id;
}
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::channel::FRAME_HEADER_LEN;

#[derive(Debug, thiserror::Error)]
pub enum LoadedAccountsError {
    /// `message` is `describe_rpc_error(&source)`, redacted at construction time —
    /// `source`'s own `Display` can carry a request URL with a secret (an API key in its query string).
    #[error("inbox program account read failed at {program_id}: {message}")]
    ProgramAccountRead {
        program_id: Pubkey,
        message: String,
        #[source]
        source: Box<solana_client::client_error::ClientError>,
    },
    /// Same redaction as [`Self::ProgramAccountRead`].
    #[error("inbox ProgramData account read failed at {programdata}: {message}")]
    ProgramDataAccountRead {
        programdata: Pubkey,
        message: String,
        #[source]
        source: Box<solana_client::client_error::ClientError>,
    },
    #[error(
        "inbox program {program_id} is owned by the upgradeable BPF loader but its own ProgramData \
         account {programdata} does not exist — a loader-v3 program always has one; this chain's inbox \
         program account is either corrupt or not the address this batcher is configured with"
    )]
    InboxProgramDataMissing {
        program_id: Pubkey,
        programdata: Pubkey,
    },
    #[error(
        "configured loaded_accounts_data_size_limit={configured} is below the {required}-byte \
         requirement this chain's live inbox program actually needs (ProgramData {programdata_len} B, \
         max_frames_per_batch={max_frames_per_batch}) — every chunk-lane, OpenBatch+Grow and \
         FinalizeBatch transaction this process sends would fail on chain with \
         MaxLoadedAccountsDataSizeExceeded; raise loaded_accounts_data_size_limit to at least {required}"
    )]
    ConfiguredLimitTooLow {
        configured: u32,
        required: usize,
        programdata_len: usize,
        max_frames_per_batch: u32,
    },
}

/// The minimal account-reading surface [`check`] needs — just an account's own data length, nothing else.
/// A real [`solana_client::nonblocking::rpc_client::RpcClient`]-backed reader for [`run`]; a fake
/// `HashMap`-backed one in this module's own tests, so `check`'s fail-closed comparison is provable
/// without any RPC.
pub trait AccountLenReader {
    fn account_len(&self, pubkey: &Pubkey) -> Result<usize, LoadedAccountsError>;
}

/// Every account one transaction holding exactly `ixs` (all invoking the same program, true of every
/// shape this batcher ever sends), sent with `fee_payer` as its fee payer, would load under SIMD-0186:
/// every instruction's own `AccountMeta`s, the invoked program itself, its own `ProgramData` account —
/// loaded even though it is never one of those `AccountMeta`s — and `fee_payer` itself, force-included
/// even when no instruction names it: **every real transaction loads its own fee payer**, whether or not
/// any instruction's own `AccountMeta`s happen to include it (before `FinalizeBatch` named an authority
/// signer, `finalize_batch_ix`'s single-account shape never named the fee payer — `FinalizeBatch` was
/// permissionless and needed no signer of its own — so its own requirement undercounted by 64 B without
/// this force-include). `FinalizeBatch` now names its own authority signer too, which in every real send
/// is this same `fee_payer`, so the force-include below stays load-bearing
/// (a caller could in principle pass a different authority than its fee payer, which this force-include
/// also covers correctly). For the chunk-lane and `OpenBatch`+`Grow` shapes `fee_payer` is already one of
/// `ixs`' own accounts (the payer/authority `plan_chunk`/`open_and_grow_batch_ixs` were built with), so
/// force-including it here is a no-op for those two. Built from the real
/// `zk_inbox_client`/`pipeline` instruction builders with placeholder pubkeys/ids (none of which change
/// any account's byte length or count), so this count can never silently drift from what this batcher's
/// own instruction builders actually send.
fn unique_loaded_accounts(ixs: &[Instruction], fee_payer: &Pubkey) -> usize {
    debug_assert!(
        ixs.windows(2).all(|w| w[0].program_id == w[1].program_id),
        "unique_loaded_accounts' plain +1 for ProgramData below is only correct as long as every \
         instruction in `ixs` targets the one program whose ProgramData is being charged — every shape \
         this batcher builds does, but a caller passing a mixed-program list would silently undercount"
    );
    let mut set = std::collections::HashSet::new();
    set.insert(*fee_payer);
    for ix in ixs {
        set.insert(ix.program_id);
        for m in &ix.accounts {
            set.insert(m.pubkey);
        }
    }
    // +1 for ProgramData (SIMD-0186): a distinct pubkey from every one already in `set`, so a plain +1 is
    // correct as long as every ix in `ixs` targets the same one program (see the debug_assert! above).
    set.len() + 1
}

/// The true `loaded_accounts_data_size_limit` requirement for this chain's chunk-lane, `OpenBatch`+`Grow`,
/// and `FinalizeBatch` transaction shapes, at `max_frames_per_batch` — the max of the three, since one
/// config value covers every tuning this binary builds (no chunk-vs-finalize split
/// for this field). `program_account_len` and `programdata_len` are the live target's own account sizes
/// (read by [`run`], supplied directly by [`check`]'s own tests) — SIMD-0186's 64-B-per-account overhead
/// plus each shape's own account data (the batch account at `rome_zk_layouts::batch::account_len_for`, plus
/// the chunk account for the chunk-lane shape) is computed here from the real instruction builders, never
/// hand-counted.
pub fn required_loaded_accounts_data_size(
    inbox_program_id: &Pubkey,
    program_account_len: usize,
    programdata_len: usize,
    max_frames_per_batch: u32,
    max_frame_body_len: usize,
) -> usize {
    // The one owner of the 64-B-per-loaded-account term is `rome_zk_solana_sender::LOADED_ACCOUNT_BASE_BYTES`
    // — this call site keeps its own frame-specific cushion terms (`FRAME_HEADER_LEN`, the batch/chunk
    // account cushion) unchanged.
    let loaded_account_base_bytes = rome_zk_solana_sender::LOADED_ACCOUNT_BASE_BYTES as usize;
    let placeholder = Pubkey::new_from_array([1u8; 32]);

    // Sized for the version `OpenBatch` writes (v2); the account is never larger than this until the
    // inbox writes the next header version.
    let batch_len = rome_zk_layouts::batch::account_len_for(
        rome_zk_layouts::batch::VERSION,
        max_frames_per_batch,
    )
    .expect("the version OpenBatch writes is a known batch version");
    // Counted as a cushion, not a modeled real load: the chunk PDA this term's bytes belong to is created by this
    // very transaction's own `Open` instruction, so it does not exist yet — and SIMD-0186 does not charge a load for
    // an account that doesn't exist — at the point the runtime measures loaded-accounts size. Charging its full
    // post-`Open` size here is deliberate headroom, not a quantity the live probe actually counts — see the
    // live-verified minimums (142,966 B / 290 frames, 162,562 B / 900) this function's own tests pin against, both
    // comfortably below what this cushion produces.
    let chunk_len = zk_inbox_client::CHUNK_HEADER_LEN + FRAME_HEADER_LEN + max_frame_body_len;
    let base = program_account_len + programdata_len + batch_len;

    let chunk_payload = vec![0u8; FRAME_HEADER_LEN + max_frame_body_len];
    let chunk_ixs = crate::pipeline::plan_chunk(
        inbox_program_id,
        &placeholder,
        &placeholder,
        200_101,
        0,
        0,
        &chunk_payload,
    );
    let chunk_requirement = loaded_account_base_bytes
        * unique_loaded_accounts(&chunk_ixs, &placeholder)
        + base
        + chunk_len;

    let open_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        inbox_program_id,
        &placeholder,
        200_101,
        0,
        max_frames_per_batch,
        &placeholder,
    );
    let open_grow_requirement =
        loaded_account_base_bytes * unique_loaded_accounts(&open_grow_ixs, &placeholder) + base;

    let finalize_ixs = [zk_inbox_client::finalize_batch_ix(
        inbox_program_id,
        &placeholder,
        &placeholder,
        200_101,
        0,
        0,
    )];
    // `FinalizeBatch` now names its own authority signer — here the same `placeholder` this
    // function already uses as the real transaction's fee payer, so it contributes no new unique account
    // beyond what `unique_loaded_accounts`'s own fee-payer insertion below already counts (that is still the
    // reason the fee payer must be force-included in the first place: `FinalizeBatch`'s account list alone,
    // before it named an authority signer, named nothing that resolved to it).
    let finalize_requirement =
        loaded_account_base_bytes * unique_loaded_accounts(&finalize_ixs, &placeholder) + base;

    chunk_requirement
        .max(open_grow_requirement)
        .max(finalize_requirement)
}

/// The fail-closed comparison: reads `inbox_program_id`'s own account length and its `ProgramData`
/// account length (derived via [`bpf_loader_upgradeable::get_program_data_address`], no separate input
/// needed) through `reader`, computes [`required_loaded_accounts_data_size`], and refuses (returns `Err`)
/// if `configured_limit` is below it. Pure given `reader` — no RPC of its own — so this is what this
/// module's own tests drive directly against a fake [`AccountLenReader`].
pub fn check<R: AccountLenReader>(
    reader: &R,
    inbox_program_id: &Pubkey,
    configured_limit: u32,
    max_frames_per_batch: u32,
    max_frame_body_len: usize,
) -> Result<(), LoadedAccountsError> {
    let program_account_len = reader.account_len(inbox_program_id)?;
    let programdata_address = bpf_loader_upgradeable::get_program_data_address(inbox_program_id);
    let programdata_len = reader.account_len(&programdata_address)?;

    let required = required_loaded_accounts_data_size(
        inbox_program_id,
        program_account_len,
        programdata_len,
        max_frames_per_batch,
        max_frame_body_len,
    );
    if (configured_limit as usize) < required {
        return Err(LoadedAccountsError::ConfiguredLimitTooLow {
            configured: configured_limit,
            required,
            programdata_len,
            max_frames_per_batch,
        });
    }
    Ok(())
}

/// An [`AccountLenReader`] over two already-fetched account lengths — everything [`check`] ever queries
/// ([`run`]'s own real-RPC path).
struct FetchedLens {
    program: (Pubkey, usize),
    programdata: (Pubkey, usize),
}

impl AccountLenReader for FetchedLens {
    fn account_len(&self, pubkey: &Pubkey) -> Result<usize, LoadedAccountsError> {
        if *pubkey == self.program.0 {
            Ok(self.program.1)
        } else if *pubkey == self.programdata.0 {
            Ok(self.programdata.1)
        } else {
            unreachable!("check() only ever queries the program account and its own ProgramData")
        }
    }
}

/// The ProgramData length SIMD-0186 actually charges for the inbox program, given what its own account says
/// (`program_owner`) and — only when that matters — whatever `programdata_account_len` was found at
/// `programdata_address`. Pure and RPC-free, so this crate's own tests drive it directly rather than only exercising
/// it through a live `run()` call: a program owned by the upgradeable BPF loader always has a `ProgramData` account,
/// so `None` there means the account is genuinely missing — a named, distinguishable failure
/// (`InboxProgramDataMissing`), not the same undifferentiated "read failed" a transport error produces. Any other
/// owner (a non-upgradeable loader, or none at all) has no `ProgramData` account and SIMD-0186 charges no load for
/// one — `0`, regardless of what `programdata_account_len` says (callers should not even fetch it in that case; see
/// [`run`]).
fn resolve_programdata_len(
    inbox_program_id: &Pubkey,
    program_owner: &Pubkey,
    programdata_address: &Pubkey,
    programdata_account_len: Option<usize>,
) -> Result<usize, LoadedAccountsError> {
    if *program_owner == bpf_loader_upgradeable::id() {
        programdata_account_len.ok_or(LoadedAccountsError::InboxProgramDataMissing {
            program_id: *inbox_program_id,
            programdata: *programdata_address,
        })
    } else {
        Ok(0)
    }
}

/// Reads the live inbox program's own account first, then — only if it is owned by the upgradeable BPF
/// loader — its `ProgramData` account, and runs [`check`] — the process-startup preflight step (refuse
/// to run rather than let a misconfigured process spend any fee) that proves
/// the configured `loaded_accounts_data_size_limit` is actually enough for the chain this process is about
/// to post to. Every inbox program this batcher has ever targeted is loader-v3, but reading the program
/// account's own owner first — rather than assuming it and blindly fetching `ProgramData` — means a
/// misconfigured `inbox_program_id` pointed at some other kind of program gets a named
/// `InboxProgramDataMissing` (if it happens to be upgradeable-owned with no `ProgramData`, which cannot
/// really happen but is handled) or simply `programdata_len = 0` (any other loader) instead of a
/// misleading "ProgramData account read failed" that reads like an RPC/network problem. Still fail-closed
/// on every real RPC transport error.
pub async fn run(
    rpc: &solana_client::nonblocking::rpc_client::RpcClient,
    inbox_program_id: &Pubkey,
    configured_limit: u32,
    max_frames_per_batch: u32,
    max_frame_body_len: usize,
) -> Result<(), LoadedAccountsError> {
    let program_account = rpc.get_account(inbox_program_id).await.map_err(|e| {
        LoadedAccountsError::ProgramAccountRead {
            program_id: *inbox_program_id,
            message: rome_zk_solana_sender::describe_rpc_error(&e),
            source: Box::new(e),
        }
    })?;
    let programdata_address = bpf_loader_upgradeable::get_program_data_address(inbox_program_id);

    let programdata_account_len = if program_account.owner == bpf_loader_upgradeable::id() {
        let response = rpc
            .get_account_with_commitment(&programdata_address, rpc.commitment())
            .await
            .map_err(|e| LoadedAccountsError::ProgramDataAccountRead {
                programdata: programdata_address,
                message: rome_zk_solana_sender::describe_rpc_error(&e),
                source: Box::new(e),
            })?;
        response.value.map(|account| account.data.len())
    } else {
        tracing::info!(
            program_id = %inbox_program_id,
            owner = %program_account.owner,
            "inbox program is not owned by the upgradeable BPF loader; SIMD-0186 charges no ProgramData \
             load for it, so this chain's loaded-accounts requirement omits it"
        );
        None
    };
    let programdata_len = resolve_programdata_len(
        inbox_program_id,
        &program_account.owner,
        &programdata_address,
        programdata_account_len,
    )?;

    let reader = FetchedLens {
        program: (*inbox_program_id, program_account.data.len()),
        programdata: (programdata_address, programdata_len),
    };
    check(
        &reader,
        inbox_program_id,
        configured_limit,
        max_frames_per_batch,
        max_frame_body_len,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeReader(std::collections::HashMap<Pubkey, usize>);

    impl AccountLenReader for FakeReader {
        fn account_len(&self, pubkey: &Pubkey) -> Result<usize, LoadedAccountsError> {
            Ok(*self
                .0
                .get(pubkey)
                .unwrap_or_else(|| panic!("fake reader has no entry for {pubkey}")))
        }
    }

    /// A fake account reader returning a 133,077-B
    /// `ProgramData` (Tiber's own measured size) at `max_frames_per_batch=900` must
    /// reject the old 131,072 default and accept the corrected 262,144 one.
    #[test]
    fn refuses_a_configured_limit_below_the_live_targets_requirement() {
        let inbox_program_id = Pubkey::new_unique();
        let programdata = bpf_loader_upgradeable::get_program_data_address(&inbox_program_id);
        let mut lens = std::collections::HashMap::new();
        lens.insert(inbox_program_id, 36usize); // UpgradeableLoaderState::size_of_program()
        lens.insert(programdata, 133_077usize); // Tiber's own measured ProgramData length.
        let reader = FakeReader(lens);

        let err = check(
            &reader,
            &inbox_program_id,
            131_072,
            900,
            crate::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        )
        .expect_err("131,072 must be rejected against a 133,077-B ProgramData");
        assert!(matches!(
            err,
            LoadedAccountsError::ConfiguredLimitTooLow { .. }
        ));
        if let LoadedAccountsError::ConfiguredLimitTooLow {
            configured,
            required,
            ..
        } = err
        {
            assert_eq!(configured, 131_072);
            assert!(
                required > 131_072,
                "the computed requirement must actually exceed the rejected config, got {required}"
            );
            assert!(
                required <= 262_144,
                "the computed requirement must not itself exceed the corrected default, got {required}"
            );
        }

        check(
            &reader,
            &inbox_program_id,
            262_144,
            900,
            crate::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        )
        .expect("262,144 must clear the live target's requirement");
    }

    /// Every account this crate's own instruction builders send is accounted for: the requirement must
    /// grow when `max_frames_per_batch` grows (a bigger batch account), and must always exceed both the
    /// `ProgramData` length alone and the 64-B-per-account base term.
    #[test]
    fn requirement_grows_with_max_frames_per_batch() {
        let program_id = Pubkey::new_unique();
        let small = required_loaded_accounts_data_size(
            &program_id,
            36,
            133_077,
            290,
            crate::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        );
        let large = required_loaded_accounts_data_size(
            &program_id,
            36,
            133_077,
            900,
            crate::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        );
        assert!(large > small, "900-frame batch must need more than 290");
        assert!(
            small > 133_077,
            "must account for more than ProgramData alone"
        );
    }

    /// `LOADED_ACCOUNT_BASE_BYTES` (SIMD-0186's 64-byte-per-loaded-account overhead) is pinned here: changing it to `0`
    /// makes this test fail. It pins the 290-frame requirement to its exact real value (computed from the real
    /// `plan_chunk`/`open_and_grow_batch_ixs`/`finalize_batch_ix` instruction lists, Tiber's own measured
    /// program/ProgramData sizes) and checks it against the live-verified minimum from the devnet probe (142,966 B).
    /// The batch account header grew 202 -> 210 B (`open_unix_ts`), so this pin moved +8 B in lockstep — deliberately
    /// recomputed, not silently left stale.
    #[test]
    fn requirement_pins_the_290_frame_batch_against_its_live_minimum() {
        let program_id = Pubkey::new_unique();
        let required = required_loaded_accounts_data_size(
            &program_id,
            36,
            133_077,
            290,
            crate::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        );
        assert_eq!(
            required, 146_788,
            "290-frame requirement drifted from its pinned value — recompute by hand before changing this"
        );
        assert!(
            required >= 142_966,
            "must never fall below the devnet-probe-verified live minimum for a 290-frame batch, got \
             {required}"
        );
    }

    /// The 900-frame counterpart of the pin above — live-verified minimum 162,562 B.
    #[test]
    fn requirement_pins_the_900_frame_batch_against_its_live_minimum() {
        let program_id = Pubkey::new_unique();
        let required = required_loaded_accounts_data_size(
            &program_id,
            36,
            133_077,
            900,
            crate::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        );
        assert_eq!(
            required, 166_384,
            "900-frame requirement drifted from its pinned value — recompute by hand before changing this"
        );
        assert!(
            required >= 162_562,
            "must never fall below the devnet-probe-verified live minimum for a 900-frame batch, got \
             {required}"
        );
    }

    /// A loader-v3 (upgradeable) inbox program always has a
    /// `ProgramData` account — its absence is a distinguishable, named failure, not folded into the same
    /// "read failed" an RPC transport error produces.
    #[test]
    fn resolve_programdata_len_requires_programdata_for_a_loader_v3_program() {
        let program_id = Pubkey::new_unique();
        let programdata = bpf_loader_upgradeable::get_program_data_address(&program_id);

        assert_eq!(
            resolve_programdata_len(
                &program_id,
                &bpf_loader_upgradeable::id(),
                &programdata,
                Some(133_077),
            )
            .unwrap(),
            133_077
        );

        let err = resolve_programdata_len(
            &program_id,
            &bpf_loader_upgradeable::id(),
            &programdata,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LoadedAccountsError::InboxProgramDataMissing { .. }
        ));
    }

    /// Any owner other than the upgradeable BPF loader has no `ProgramData` account at all — SIMD-0186
    /// charges no load for one, so this must be `0`, and the branch must not even consult whatever a
    /// caller happened to pass for `programdata_account_len` (production code never fetches it for a
    /// non-upgradeable owner — see `run`'s own branch — so a stray `Some` here must still be ignored).
    #[test]
    fn resolve_programdata_len_is_zero_for_a_non_upgradeable_owner() {
        let program_id = Pubkey::new_unique();
        let programdata = bpf_loader_upgradeable::get_program_data_address(&program_id);
        let other_owner = Pubkey::new_unique();

        assert_eq!(
            resolve_programdata_len(&program_id, &other_owner, &programdata, None).unwrap(),
            0
        );
        assert_eq!(
            resolve_programdata_len(&program_id, &other_owner, &programdata, Some(999)).unwrap(),
            0,
            "a non-upgradeable owner must yield 0 regardless of any stray programdata_account_len"
        );
    }
}
