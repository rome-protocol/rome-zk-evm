# zk-settlement

The on-chain settlement program. Registers chains, accepts posted batch roots (proved or unproved), tracks
their path to finality, and exposes a cross-program-invocable read (`RootView`) that only ever returns a
final root. Also owns the per-chain registration, fee and governance bookkeeping that funds and gates this
system.

## What it guarantees

- **The finality head only advances in order.** Permissionless chains (`chain_id >= 2^32`) accept only
  `PostRootProved`; `PostRoot` rejects them with `UnprovedRootNotAllowed` (83) before any account read,
  write or fee transfer. Rome's reserved-range chains also accept unproved `PostRoot`, which records a
  pending batch. `FinalizeBatch` can finalize it once its challenge window elapses with no open dispute
  and its predecessor is final. `PostRootProved` writes a `Final` batch immediately, but advances the
  finality head only when the batch is next in order. These are the only two instructions that advance it.
- **Every posted batch is checked against its predecessor and against the inbox program's own
  commitment.** `PostRoot`/`PostRootProved` require the caller's claimed previous batch, previous state
  root and first block number to match what the predecessor batch (or the chain's genesis, for the first
  batch) actually recorded — and require the inbox program's batch account to be finalized with its own
  `acc` commitment equal to what the caller claims. The inbox batch account is derived with this
  program's own id as the settlement-program seed, and its recorded settlement program must be this
  program (`WrongInboxAccount` otherwise), so a batch opened through any other settlement program is never
  read. On every chain, `PostRootProved` also requires the
  proof's `parent_hash` to equal the predecessor's last block hash: `root.block_hash` when
  `head_pending_batch == 0`, otherwise the predecessor pending account's `last_block_hash`, whether that
  batch is pending or final. Both proof layouts reject a mismatch with `PredecessorHashMismatch` (84)
  before the pairing check or any write.
- **`RootView` never returns a pending root, to anyone, in any transaction.** This is what lets a single
  Solana transaction combine a root read with an unrelated instruction — moving a bridged asset, for
  example — without that instruction ever acting on unconfirmed rollup state; there is no way to opt out
  of this check from the calling side. A permissionless chain's registration-time genesis (batch 0)
  returns `NotFinal` until its first proved batch advances the head; `ProveExit` uses the same gate.
  Reserved chains keep their genesis as final.
- **The proved fee component is bound to the proof itself.** `PostRootProved` requires the poster's
  declared gas usage to equal the proved header's own `gasUsed` field before charging the gas-proportional
  fee — a poster cannot under-report gas to underpay. `PostRoot` (the unproved path) has nothing to check
  that claim against, so it charges only the fixed per-batch fee, never the variable component.
- **Chain registration cannot be captured or squatted for free.** A reserved chain id (below the
  permissionless range) requires the registry authority's co-signature and a live allow marker; a
  permissionless id is derived deterministically from the caller's own authority key and an
  internally-tracked nonce (the caller cannot choose or front-run it), and requires a refundable deposit —
  eligible for refund once `head_final_batch >= 1` or `posted_batches >= 10`. Under proved-only posting,
  the first proved root already meets the final-root trigger; the ten-post trigger remains defense in
  depth. Anyone can reclaim a chain that has never posted after its reclaim window elapses, sweeping
  its deposit and rent to the treasury.
- **The one governance instruction with no prior authority to check against authenticates against the
  deployment's own upgrade authority**, not an arbitrary argument — see the security notes below.
  Authority rotation afterward is a two-step propose/accept pair, so a typo in a proposed successor key
  can never lock the current authority out: nothing changes until the correct key actively accepts.
- **Every account this program creates is safe against pre-funding griefing**, through the same
  create-or-adopt helper the inbox program uses (see
  [`rome-zk-pda`](../../crates/rome-zk-pda)).
- **A registry entry's `scheme` names the ZisK release a proof is checked under.** `PostRootProved` looks the
  proof's programVK up among the BN254 entries under a ZisK scheme (`registry::find_zisk`), and the matching
  entry's scheme picks the release row in Veritas. The proof never names its release, so a poster cannot choose
  the key. A release that is withdrawn is refused as `ZiskVersionWithdrawn` (87), which is what every entry
  written before the release table (scheme 1, ZisK 1.2.0-alpha) now gets. The proof's `rootCVadcopFinal`
  (`proof_abi[800..832]`) must equal the value pinned for that release, or it is refused as
  `RootCNotOfVersion` (88). Both refusals come before the layout checks and the pairing, so they are cheap:
  13,364 to 14,864 compute units for a wrong recursion root and 11,851 to 16,351 for a withdrawn release (measured
  on real BPF; the range is a spread between test runs, not a property of the proof: the program derives its
  accounts with a bump search, and the number of attempts depends on the program id and the chain id). A
  programVK with no entry is still `RegistryEntryNotFound`. A real ZisK 1.3.1 layout-1 batch proof, posted under
  a scheme-2 entry, finalizes in 468,334 to 490,834 compute units (measured on real BPF). The figure is not
  fixed: the same proof posted under different program ids and chain ids costs different amounts, and the
  differences are multiples of 1,500, the cost of one more bump attempt. It stays well under the prover's
  700,000 limit.
- **What may be written to a chain's registry depends on the release.** `SetRegistryEntry` (and every genesis
  entry of `InitChainV2` on the reserved path) refuses an unknown scheme number and a ZisK scheme on a curve other
  than BN254 (`UnknownCurveOrScheme`). A new entry or a moved activation slot is refused under a withdrawn release
  (`ZiskVersionWithdrawn`, 87) and under a closing one (`ZiskVersionClosing`, 89). Retiring an entry that is
  present is always allowed, whatever the release's status. A new ZisK entry may not take a programVK that a
  live entry already holds under another release (`VkeyUnderOtherZiskVersion`, 90), so one programVK can never
  pick between releases: retire the other release's entry first.
- **A registry entry's `layout_id` selects what a proof is bound to.** Layout 2 (the header fallback)
  binds a single block's header fields only. It binds neither the chain id nor the inbox commitment, so `SetRegistryEntry` refuses it on a permissionless chain (`HeaderFallbackNotAllowed`); only reserved chains may register it. Layout 1 binds a whole batch range's public values —
  `chain_id`, first/last block, the guest's own drift-bound inputs (`open_unix_ts`, `max_drift_secs`),
  summed `gas_used`, and both the inbox and forced-outcome commitments — checked against the poster's
  claimed `args`, the inbox batch account's own committed clock reading, and the chain's `chain_config`
  drift bound, all before the pairing runs. A 512-byte publics blob that is not a v2 packing (a word over
  `u32::MAX`, a non-zero tail word) is refused as `BadPublicValuesPacking`, and 208 bytes that do not
  decode as `BadPublicValues` — never a bare `InvalidInstructionData`, which is what the pairing itself
  returns. A chain still on `chain_config` v1 (not yet brought forward by `MigrateChainV2`) cannot post
  under layout 1 at all (`DriftBoundUnset`) — there is no drift bound on record to check a proof
  against yet.

## Instructions (by discriminant)

| # | Instruction | Notes |
|---|---|---|
| 0-2 | `Init`/`UpdateRoot`/`UpdateRootZisk` | disabled — decode, then rejected before touching any account |
| 3 | `InitChain` | RETIRED: body kept as recorded on the devnet, decodes, refused by name (`RetiredInstruction`); superseded by 24 |
| 4 | `PostRoot` | unproved, window-based finality; reserved-range chains only (`UnprovedRootNotAllowed` otherwise) |
| 5 | `PostRootProved` | proved finality — layout 1 (batch-level public values) or layout 2 (header) per the registry entry |
| 6 | `FinalizeBatch` | window-elapsed / already-final head advance |
| 7 | `ClosePending` | rent recycle for a final, non-head batch |
| 8 | `RootView` | CPI-readable, final roots only |
| 9 | `RejectBatch` | reserved (challenge flow, not implemented) |
| 10 | `InitGlobalConfig` | one-time, upgrade-authority-gated |
| 11 | `AllowReservedId` | registry-authority-only |
| 12 | `RevokeReservedId` | registry-authority-only |
| 13 | `SetFee` | registry-authority-only |
| 14 | `SetTreasury` | registry-authority-only |
| 15 | `MigrateChain` | RETIRED: decodes so recorded history stays readable, refused by name (`RetiredInstruction`); superseded by 23 |
| 16 | `RefundDeposit` | permissionless, credits `root.authority` only |
| 17 | `ReclaimChain` | permissionless, window-gated |
| 18 | `SetGlobalConfig` | registry-authority-only |
| 19 | `SetRegistryAuthority` | disabled — replaced by the two-step pair below |
| 20 | `ProposeRegistryAuthority` | registry-authority-only, step one of two |
| 21 | `AcceptRegistryAuthority` | signed by the proposed key, step two |
| 22 | `SetDriftBound` | registry-authority-only; sets a chain's drift bound once it is already on `chain_config` v2 |
| 23 | `MigrateChainV2` | bring a pre-existing chain's `chain_config` to v2 (create if absent, realloc 47→55 if v1, refuse if already v2); `max_drift_secs` is a required argument; registry-authority-only |
| 24 | `InitChainV2` | creates root + registry + `chain_config` v2 (`max_drift_secs` is a required argument); reserved or permissionless id; every `registry_entries` vkey (curve/scheme/vkey_hash) must be distinct — a duplicate, even under a different layout, is refused (`DuplicateRegistryEntry`) before anything is written; a permissionless id must send no `registry_entries` at all (`RegistryEntriesNotAllowed`) |
| 25 | `SetRegistryEntry` | registry-authority-only; registers a verifier key (keyed on curve/scheme/vkey_hash) with an explicit activation delay, or updates that same key's activation slot — including retiring it immediately; realloc's the registry v1→v2 on first use |
| 26 | `ProposeExitConfig` | chain-authority-only (`root.authority`); proposes a delayed change to the chain's exit portal/bridge-program/cap/bond, creating the `exit_config` PDA (create-or-adopt) if absent |
| 27 | `ActivateExitConfig` | permissionless once the proposal's activation slot is reached; copies the pending fields into `exit_config`'s current fields and the root's `exit_cap_per_window`/`poster_bond` |
| 28 | `ProveExit` | permissionless; proves an L2 exit against a FINAL batch's `state_root`, binds the portal from `exit_config`, enforces the per-window cap, burns a persistent replay nullifier |
| 29 | `ConsumeExit` | bridge-PDA-signer-only; releases a PROVED exit, refunds the record's own payer, closes the record — never touches the replay nullifier |

Discriminants never shift once shipped — a disabled variant keeps its slot rather than being removed, and
every addition is appended.

## Exit configuration governance

A chain's exit portal (the L2 contract exits are proved against), bridge program, exit cap and poster bond
are not set at `InitChainV2` time — they are configured afterward, by the chain authority, through a
repeatable two-step delayed process: propose, then activate, as many governance cycles as the chain ever
needs (rotate the portal, raise the cap, and so on) — there is no limit on how many times a chain may go
through this cycle.

1. **`ProposeExitConfig`** — signed by `root.authority`. Takes any subset of `exit_portal`,
   `bridge_program`, `exit_cap_per_window`, `poster_bond` (each an `Option`; an omitted field is simply not
   part of this proposal), and every field `None` is refused outright (`InvalidArgument`) rather than
   accepted as an inert no-op, plus a required `activation_slot`. The delay is enforced, not merely
   suggested: `activation_slot` must be at least `Clock::slot + root.challenge_window_slots` ahead — **one
   full challenge window, with no bootstrap exception** — and a chain whose `challenge_window_slots == 0`
   cannot propose at all (`ChallengeWindowZero`, fail-closed: a chain with no window can never be
   configured for exits). A supplied `exit_portal`/`bridge_program` may never be the zero sentinel. Only
   one proposal may be in flight per chain (`PendingExitConfigExists`) — there is no cancel path; the
   existing proposal must be activated before a new one can be proposed, and once it has been, the very
   next `ProposeExitConfig` starts a fresh cycle against the same `exit_config` account (it is created once,
   on the chain's first proposal, and never closed). Writes only the `exit_config` account's PENDING
   fields; the CURRENT `exit_portal`/`bridge_program` are untouched, so a proposal is invisible to
   `ProveExit` (below) until it is activated. **The bridge program is set once:** once
   `exit_config.bridge_program` is non-zero, a proposal that names a bridge, the same one included, is
   refused (`BridgeProgramSetOnce`); portal, cap and bond proposals are unaffected.
2. **`ActivateExitConfig`** — permissionless; anyone may call it once `Clock::slot >= exit_config.activation_slot`
   (`ActivationNotReached` before then; `NoPendingExitConfig` if nothing is pending). Copies the pending
   portal/bridge-program into `exit_config`'s current fields, and the pending cap/bond into the **root
   account's own** `exit_cap_per_window`/`poster_bond` fields (the root stays the one source of truth for
   both numbers — its byte layout is unchanged by this pair of instructions), then clears every pending
   field, `activation_slot` and the mask back to zero. A pending proposal made before the bridge became
   set-once may still carry a bridge part over a bridge that is already set: activation drops that part (the
   current bridge stays and the dropped one is logged), applies the portal, cap and bond parts and clears the
   slot. It never refuses over it, because a pending proposal cannot be cancelled.

The poster bond is recorded as a plain number in this program — no escrow, no unit reconciliation against the
gwei-denominated exit cap (that ships later); see the security notes below.

## Exit proving (`ProveExit`, 28)

Once a chain's exit governance is active, anyone may prove an individual L2 exit and burn its replay
nullifier — this instruction does not move any funds itself (that is a minimal `zk-bridge` program's job,
authorised by the registered `bridge_program`); it only establishes, on chain, that a
withdrawal was really initiated and admits it against the chain's per-window cap.

**What it proves.** An exit is an Ethereum Merkle-Patricia proof of two facts against a FINAL batch's
`state_root` (the same commitment `PostRootProved` binds and `RootView` reads — see
[`crates/rome-zk-mpt`](../../crates/rome-zk-mpt)): that the configured exit portal account exists with a
given `storage_root`, and that `sentMessages[message_hash]` is set to `1` in that account's storage —
`message_hash = keccak256(abi.encode(nonce, l2Sender, solRecipient, asset, amount))`, the same message the
L2 exit portal contract committed. `ProveExit`'s caller supplies the account and storage proof nodes
(`rome_zk_mpt::ExitProof`); the program supplies the finality gate and the root — the caller cannot pick
which `state_root` a proof is checked against, only which already-Final batch to name.

**Finality — the same gate `RootView` uses.** Both instructions call `final_root_tuple`. It returns
`NotFinal` if the requested batch is ahead of `root.head_final_batch`, or if an older batch's pending
account is not `Final` or records a different batch number. It also returns `NotFinal` while a
permissionless chain's `head_final_batch == 0`: the genesis its owner supplied at registration is not a
final root, so neither `RootView` nor `ProveExit` can use it. The first proved batch establishes that
chain's first final root. Reserved chains keep their genesis as final. For the current head, the tuple
comes directly from the root account; an older batch needs its own still-live pending PDA.

**The portal address is bound on chain, never supplied by the caller.** `ProveExit` reads
`exit_config.exit_portal` (set by the chain authority's `ProposeExitConfig`/`ActivateExitConfig` cycle
above) and proves inclusion against exactly that address — there is no portal field anywhere in
`ProveExit`'s own arguments for a caller to name instead. An absent `exit_config` account (never
configured) reads as the zero portal, refused `ExitConfigUnset`, matching the "absent = disabled"
convention `exit_config` already documents.

**The cap gate, and why a refusal never burns the nullifier.** `window_index = Clock::slot /
root.challenge_window_slots` (`root.challenge_window_slots == 0` refuses `ChallengeWindowZero`, the same
fail-closed rule `ProposeExitConfig` enforces at configuration time); `units = cap_units(message.amount)`
(`ceil(amount / 1 gwei)`, `ExitAmountOverflow` if that does not fit a `u64`); the window's new spent total
must not exceed `root.exit_cap_per_window` (`0` refuses `ExitCapUnset`) or the call is refused
`ExitCapExceeded` — landing exactly on the cap is allowed, only strictly over it is refused. Every check
this instruction makes, including the nullifier's own `bit_is_set` replay read, runs to completion before
any account is written: a call refused for being over the cap leaves the message's nullifier bit clear, so
the exact same exit can be re-proved once the client re-queues it into a later window — the cap
gate and the replay gate never interfere with each other.

**Replay: a persistent, per-nonce bit, shared across exits by design.** `page = nonce >> 13` selects one
of `exit_nullifier`'s 8,192-exit bitmap pages; a page already set for `message.nonce` refuses
`ExitAlreadyProved`. The nullifier page and the window account are **shared state** — many exits on the
same page or in the same window is the normal case, not an edge case — so both are created with the same
create-or-adopt-on-`!already_exists` pattern already used for `exit_config`: two exits sharing a window or
a page both succeed, each only ever incrementing/OR-ing into what the other left, never re-creating (and
thereby reverting) an account the other one already made.
`exit_record` follows the same guarded pattern defensively, though in practice a genuine re-prove of the
same message is always caught by the nullifier first.

**CU and size (measured on an anvil fixture — 3 account nodes + 2 storage nodes).** `ProveExit` consumed
**41,342 CU**; a signed V1 transaction carrying it is **1,179 bytes**, well inside the 4,096-byte SIMD-0385
envelope. The design's own budget (≤ 120k CU for a 16+8-node proof) is a ceiling for a much larger proof
than this fixture — this measurement is a floor at this proof's own size, not evidence against that
ceiling; the real Tiber-scale proof's CU and byte size are a separate measurement, not assumed from this
number.

## Exit release (`ConsumeExit`, 29)

Once an exit is PROVED, the chain's registered bridge program releases it and reclaims the record's rent.

**Authorisation is one PDA, CPI-signed — the whole gate.** `bridge_signer` must be
`find_program_address(["exit_consumer", chain_id], exit_config.bridge_program)`, and it must actually be a
transaction signer — which only an `invoke_signed` CPI *from that exact program* can ever produce (a PDA
has no private key, so a keypair can never satisfy this). There is no other check on who may call
`ConsumeExit`: a different program's own `exit_consumer` PDA derives to a different address, so it can
never collide with the registered bridge's. The real bridge program (`programs/zk-bridge`)
CPIs this the same way; this program's own test suite (`tests/exit_consume.rs`) loads a minimal test-only
stand-in program for the same purpose (never shipped as a deployed program).

**The refund always goes to the record's own payer, never an instruction argument.** `payer_refund` must
equal `exit_record.payer` — the account `ProveExit` itself paid the record's rent from — so a caller
cannot redirect somebody else's rent refund by naming a different account. A record only ever exists as
PROVED (`ExitNotProved` otherwise; a non-existent record fails the owner/seeds check first, which reads
the same either way).

**The record closes; the replay nullifier does not.** `ConsumeExit` drains the record's lamports to
`payer_refund`, reallocs it to zero bytes and reassigns it to the system program (the exact three-call
close shape `ClosePending` already uses) — the record's rent is recycled. The `exit_nullifier` bit for this
message is never touched: it was set once, permanently, by `ProveExit`, and a re-`ProveExit` of the same
message after this call still hits `ExitAlreadyProved` at `prove_exit`'s own nullifier check, which reads
the persistent bit, never the now-recycled record. "Nothing is paid forever" applies to the ONE-BIT
nullifier's rent, not to the record's — which is exactly the rent this instruction gives back.

**CU (measured, real BPF, fixed program ids for reproducibility).** `ConsumeExit`'s own execution —
isolated via `sol_log_compute_units` markers either side of the CPI in the test suite's stub bridge — is
**17,168 CU**; the whole transaction, including the TEST-ONLY stub's own CPI-dispatch overhead, is
**20,480 CU**. Both exceed the earlier planning figure of `ConsumeExit ≤ 15k` — an ASSUMPTION, never
previously measured — reported here as the real number rather than forced to fit it.

## Config

None — this is a Solana on-chain program; all of its behavior is driven by instruction arguments and
account state. A deployment's program id and its global configuration (treasury, registry authority,
default fee schedule) are set up in the operator's deploy config.

## How to test

```sh
cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml --sbf-out-dir target/deploy
cargo build-sbf --manifest-path programs/zk-inbox/Cargo.toml --sbf-out-dir target/deploy
cargo build-sbf --manifest-path programs/veritas/Cargo.toml --sbf-out-dir target/deploy
cargo test -p zk-settlement
```

All three programs need building first: this program's tests exercise real `PostRoot` flows against a
real inbox batch account (so `zk-inbox` must be built) and real `PostRootProved` flows against the real
proof verifier (so `veritas` must be built too) — every compute-unit assertion in this suite
reflects actual BPF execution. `ProveExit`'s own suite (`tests/exit_prove.rs`) needs no extra program
built — it loads `fixtures/exit/*.json` (committed anvil proofs) directly. `ConsumeExit`'s own suite
(`tests/exit_consume.rs`) needs one more real program built first — a minimal, TEST-ONLY stub bridge that
CPI-signs the `exit_consumer` PDA the same way the real `zk-bridge` program does, never
itself shipped as a deployed program:

```sh
cargo build-sbf --manifest-path programs/zk-settlement/tests/fixtures/stub-bridge/Cargo.toml --sbf-out-dir target/deploy
```

## Security notes

The registration-and-revenue surface is the part of this program most exposed to economic attacks, and it
has a correspondingly large dedicated test suite (`tests/registration_revenue.rs`): a reserved id cannot
be registered without the registry authority's signature and a live allow marker
(`reserved_init_chain_rejects_a_signer_that_is_not_the_registry_authority`,
`reserved_init_chain_rejects_an_id_with_no_allowlist_marker`); a permissionless id cannot be claimed under
any id other than the one deterministically derived from the caller
(`permissionless_init_chain_rejects_a_chain_id_that_does_not_match_the_derived_id`); the global
configuration's one-time bootstrap is gated on the program's real upgrade authority, not a caller-supplied
key (`init_global_config_rejects_a_signer_that_is_not_the_upgrade_authority`,
`init_global_config_rejects_an_immutable_program`, `init_global_config_rejects_program_data_at_the_wrong_address`);
authority rotation cannot be hijacked by a third key
(`accept_registry_authority_rejects_a_third_key`) or bricked by a bad proposal
(`propose_registry_authority_rejects_the_default_pubkey`); the proved-path fee cannot be under-reported
(`post_root_proved_rejects_a_gas_in_batch_that_disagrees_with_the_proved_headers_gas_used`); and a
reserved chain with a live allow marker cannot be reclaimed out from under its registry authority
(`reclaim_chain_reserved_has_no_deposit_to_sweep_and_frees_the_id`, alongside the allow-marker revocation
path). See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#threat-model) for these attack classes
described alongside the ones the inbox program closes.

**What a proof proves depends on which layout the registry names.** Layout 2 (the header fallback) checks
only `keccak(header) == the guest's committed hash`, then reads fields directly out of that header —
sound for the header fields it claims, but nothing in it binds the proof to the inbox program's own
accumulator commitment or to the forced-inclusion lane. Layout 1's public values close that gap (they
carry `inbox_commitment` and `forced_outcome_commitment` directly, checked against the poster's claim) —
and now has a real guest behind it: the ZisK stateless-validator guest for this chain (see
[`crates/clients/rome/guest`](../../crates/clients/rome/guest) in the reproducible-build fork, and that
crate's own README) commits this layout's public values from a chain's real DA. Read
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#trust-model) before describing either layout as proving
that a batch's execution matches what was posted to the inbox.

**A chain's genesis registry, from `register_chain`.** What `InitChainV2` writes depends on the id.
A reserved chain (`--reserved`) gets three verifier entries, in this order: the layout-1 primary (that
chain's own ZisK stateless-validator guest vkey, built and verified once per chain and passed in as
`--layout1-vkey-json`), then the existing layout-2 (header-RLP) fallback, then a zeroed Groth16
placeholder slot for a future proof system. That is three of the registry's four slots; the fourth stays
free for a rotation. `register_chain --reserved` refuses to run without the layout-1 vkey file, by name,
rather than register a chain that can never finalize a proved root.

A permissionless chain (`chain_id >= 2^32`) gets no entries at all. The program refuses a permissionless
`InitChainV2` that carries any (`RegistryEntriesNotAllowed`, 82), before creating accounts, locking the
deposit or advancing the authority's nonce, so a chain cannot pick the key its own proofs are checked
against. `register_chain --permissionless` accordingly sends an empty registry and refuses
`--layout1-vkey-json` and `--zisk-vkey-json` by name (`VkeysNotAllowedOnPermissionless`), so nobody
thinks a key was registered.

After registration, the chain owner asks Rome to register its layout-1 key. Before adding it, Rome must
rebuild the chain's guest from source, compare the resulting verifying key with the requested key, and
check that the registered genesis number, block hash and state root match the genesis compiled into the
guest. These are Rome's operating procedures; `SetRegistryEntry` does not perform them. The intended
tool, `register-vkey`, does not exist yet. Once those checks pass, Rome uses its registry authority to add
the key with `SetRegistryEntry` (the `governance` example's `set-registry-entry`).

A permissionless chain is proved-only. The unproved `PostRoot` refuses it by name
(`UnprovedRootNotAllowed`, error 83), as the first thing it does: nothing is read or written and no
lamport moves. Its genesis is also refused by `final_root_tuple` with `NotFinal`, so it has no final root
until its first proved batch. That requires an active verifying key Rome registered. Rome's
reserved-range chains keep both their final genesis and the unproved, challenge-window path.

The reclaim window starts at `registered_slot`, when the chain is registered. Adding a key does not
restart it. Rome's turnaround must fit inside `reclaim_window_slots`, leaving time for the first proved
post: a chain with no posted batch can be reclaimed once `registered_slot + reclaim_window_slots` is
reached. That first proved root also makes the deposit eligible for refund (`head_final_batch >= 1`);
the alternative `posted_batches >= 10` trigger remains defense in depth. The
test `permissionless_chain_is_inert_until_the_registry_authority_registers_its_layout1_vkey` walks through
it: it shows a proved submission reaching the pairing once `SetRegistryEntry` has added the key (the blob is
not a real proof, so the pairing is where it stops); it does not show a full accepted post.

The challenge state machine (non-exclusive per-block disputes, bonds, the forced-inclusion lane) is
reserved but not implemented: `RejectBatch` decodes as a valid instruction and is then rejected before
touching any account, holding its discriminant's place for when that flow lands.

**Rotating a verifier key.** `SetRegistryEntry` keys on `(curve, scheme, vkey_hash)` — the same key
`PostRootProved`'s own registry lookup uses — never on the layout alone, so registering a new key never
touches an unrelated entry that happens to share a curve, scheme, and layout. To rotate:

1. **Register the new key** with `activation_slot` set to now plus the chain's own rotation delay (its
   `layout_id` must match the ELF actually being registered — `LayoutMismatch` if the same vkey is ever
   named under a different layout, since one vkey is one ELF is one layout). `PostRootProved` refuses a
   proof under this entry (`RegistryEntryNotFound`, the same refusal a never-registered vkey gets) until
   `Clock::slot` reaches the activation slot. The old key is completely unaffected by this call: it keeps
   proving, right up to and past the new key's activation, until it is explicitly retired.
2. **Provers switch to the new ELF** once `Clock::slot` reaches the activation slot; both keys are live at
   that point, so a proof under either one reaches the pairing.
3. **Retire the old key** — once its dispute window has passed, or immediately on compromise — by calling
   `SetRegistryEntry` again for the SAME old vkey with `activation_slot` set to the reserved tombstone value
   (`u64::MAX`, `rome_zk_layouts::registry::RETIRED_SLOT`). From that point on, `PostRootProved` refuses it
   immediately, by the same `RegistryEntryNotFound` name, with no need to wait for any slot: retirement is
   what actually stops a compromised key, and it is a deliberate, separate step, never automatic. Proofs
   already Final before the retirement are not revisited — `PostRootProved`/`FinalizeBatch` never touch an
   already-final batch's root, so a live rotation or retirement can never undo a settled result.

**Retirement is terminal.** A retired key cannot be re-activated: once a vkey's stored activation slot is
the tombstone, `SetRegistryEntry` refuses any OTHER activation slot named against that same vkey
(`EntryRetired`) — retiring the same already-retired vkey again is the one exception, and stays a no-op
success. The only way onto a retired key's slot is a genuinely new vkey. If the old ELF ever needs to
serve again, that is a rotation to a new ELF (a new key, a new registration), never an un-retire.

What this can and cannot promise: A retired vkey cannot be re-activated while its retired entry remains in the registry; once that slot has been reused for a new key the registry no longer remembers it, and naming that vkey again is an ordinary registration. The registry is four slots, not a memory — keep the list of retired ELFs and vkeys off-chain and never re-register one.

A registry entry reused for a NEW vkey is only ever a slot the old key already vacated: once the registry
is at its 4-entry capacity, `SetRegistryEntry` reuses the first slot whose key has been retired (never a
slot still holding a live key); with no retired slot to reuse, registering a 5th distinct vkey is refused
(`RegistryFull`). The registry account itself grows once, in place, from its original fixed size to the
size that also carries an activation slot per entry — the caller's payer tops up the rent difference, and
every already-registered entry's bytes (and `count`) are untouched by that growth.

`SetRegistryEntry` scans the registry for `(curve, scheme, vkey_hash)` in the same order
`registry::find` does — the first populated entry matching that key is the one this instruction acts on,
never a later one — and refuses `InvalidAccountData` if a second populated entry ever shares that same
key. Two entries sharing a vkey are never both meaningful (one vkey is one ELF is one layout), so
`InitChainV2` refuses to create that shape in the first place (`DuplicateRegistryEntry`, above); this scan
is the writer's own defence in depth against ever silently acting on the wrong copy of a duplicate.

**The exit-config gate cannot be bypassed by the registry authority, only the chain authority.**
`ProposeExitConfig` checks the caller's signer key against `root.authority` — the registry authority (a
different key entirely, gating chain *registration*) is refused the same way any other unrelated signer
would be. The activation delay is a real minimum, not a default: an `activation_slot` even one slot short of
`Clock::slot + root.challenge_window_slots` is refused (`ActivationTooSoon`), and a chain whose
`challenge_window_slots` is `0` cannot propose at all — there is no way to configure exits on a chain with no
challenge window (`ChallengeWindowZero`, fail-closed by design, not an oversight to be "fixed" with a
bootstrap exception). `ActivateExitConfig` is intentionally permissionless: nothing about who calls it is
trusted, only the clock and the pending state a real `ProposeExitConfig` already wrote — an attacker gains
nothing by activating a proposal early (they cannot, the slot check is a real minimum) or on someone else's
behalf (the result is identical either way).

**Governance is re-governable, not one-shot.** `exit_config` is created once, by the chain's first
`ProposeExitConfig`, and is never closed — `ActivateExitConfig` only clears its pending fields and mask
back to zero. `ProposeExitConfig` only creates (or adopts a pre-funded) `exit_config` the first time; every
later cycle, on an already-program-owned, correctly-sized account, skips straight to writing the new
pending fields. A chain authority may run the propose→activate cycle again and again — rotate the portal,
raise the cap, change the bridge program — with no cap on how many times, so a value set once is never
frozen there permanently.

**`ProveExit`'s portal binding cannot be bypassed by anything the caller supplies.** There is no portal
address anywhere in `ProveExit`'s own instruction arguments — the account `verify_account` proves inclusion
against is read from the on-chain `exit_config`, always; a proof of any other account (however it was
obtained) diverges from that address's own trie path and is refused `ExitProofInvalid`
(`prove_exit_for_non_portal_account_is_refused`, `tests/exit_prove.rs`) — this is unconstructable, not
merely bounded. Likewise `ProveExit` derives its own nullifier page from `nullifier_page(message.nonce)`
and checks the caller's account against that address; the caller has no separate "page" argument to
mismatch it with (`InvalidSeeds` catches any account that does not match — a wrong nullifier page cannot
even be presented, let alone accepted).

**Sharing the window and nullifier-page PDA across many exits is the design, verified in the duplicated
state, not just the empty state.** `prove_exit_over_cap_refused_and_next_window_admits`,
`two_exits_in_the_same_window_both_succeed` and `two_exits_on_the_same_nullifier_page_both_succeed`
(`tests/exit_prove.rs`) each drive TWO real, distinct exits through the same shared `exit_window`/
`exit_nullifier` account and assert both succeed — an unconditional `create_or_adopt_pda` on either account
reverts the second exit `InvalidAccountData`, which is exactly the lever these tests'
own mutations flip red.

## Design references

The design behind this program is in the project specification, which is not in this repository: the
root, pending and registry accounts in full, root posting and the root-plus-action / root-plus-root
product primitives, the challenge state machine this program reserves a discriminant for but does not
yet implement, the security model, and bonds, caps and windows. The registration-and-revenue surface
follows the project's registration-and-revenue decision, also kept outside this repository, in full
(namespace split, deposit and fee lifecycle, the two-step authority rotation, and the
upgrade-authority-gated bootstrap).

## Depends on

[`veritas`](../veritas) for the proved-post path's proof verification;
[`rome-zk-layouts`](../../crates/rome-zk-layouts) for every account layout;
[`rome-zk-pda`](../../crates/rome-zk-pda) for account creation;
[`rome-zk-mpt`](../../crates/rome-zk-mpt) for `ProveExit`'s Merkle-Patricia proof verification. Consumed by
[`zk-settlement-client`](../../crates/zk-settlement-client) and, indirectly through it, by the batch
poster.
