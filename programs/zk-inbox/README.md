# zk-inbox

The on-chain data-availability program. Holds every posted transaction chunk as Solana account data
(one chunk per program-derived account) and reduces the chunks of one batch to a single Merkle
commitment — the only representation of a batch's contents the settlement program ever reads. Solana's
64-account-per-transaction limit rules out ever touching every chunk PDA directly from the settlement
side; this program's batch accumulator is what makes root posting a fixed-size, one-account read instead.

## What it guarantees

- **A chain's inbox accounts are keyed by its settlement program, so they can only be created through
  it.** The batch cursor is `["batch_cursor", settlement_program, chain_id]`, a batch is
  `["batch", settlement_program, chain_id, batch]` and a chunk is
  `["inbox", settlement_program, chain_id, batch, idx]`, all under this program. `OpenBatch` and
  `InitBatchCursor` take the settlement program as an argument and derive from it; the account they read
  the chain's authority from is that program's own root account, so the addresses a chain uses can only
  be created by a caller who satisfies the chain's own settlement program. Chunk `Open` and `SealLeaf`
  read the settlement program from the batch account. Settlement derives the batch with its own program
  id and refuses a batch whose recorded settlement program is anyone else's.

- **A batch id is never reused.** `OpenBatch` requires the caller's batch id to equal a per-chain,
  program-owned sequential cursor's current value (bootstrapped once via `InitBatchCursor`) and advances
  that cursor atomically as part of opening — an abandoned batch's id can never be reopened, so a later
  batch can never accidentally include a stale, mismatched set of chunks under an old id.
- **The batch cursor carries the deposit cursor, and a closed batch advances it.** `InitBatchCursor` creates
  the cursor as a 69-byte version-2 account: the version-1 fields, then `deposit_next` (0),
  `deposit_hash` (the deposit queue's seed hash for the chain) and `deposit_final` (0). `CloseBatch` takes the
  cursor as its fourth, writable account; once the covering root is final, a version-3 batch raises a
  version-2 cursor's `deposit_final` to the batch's `deposit_to` when that is higher, and never lowers it. A
  version-1 cursor, or a batch that carries no deposit range, is left alone. A missing, wrong-owner,
  wrong-address or read-only cursor is refused.
- **Only the batch's own authority can open it, open a chunk under it, or finalize it.** `OpenBatch`'s
  signer must be the chain's registered authority (read from the settlement program's root account);
  `Open` (a chunk) and `FinalizeBatch` both require the signer to match that same batch's stored
  authority — refused (`MissingRequiredSignature`) when the account is absent, unsigned, or simply the
  wrong key, even for a third party who sealed every leaf itself. Sealing, by contrast, is
  permissionless — deterministic given the chunk bytes already on chain, so there is nothing to gate.
- **Seals are order-independent and safe to run in parallel.** `SealLeaf` writes one leaf's hash into the
  batch account and is a no-op if the same leaf is sealed twice with the same hash (an error if sealed
  twice with different hashes) — nothing about this program's correctness depends on chunks being sealed
  in any particular order.
- **Finalization is resumable, and produces a byte-exact, cross-checked commitment.** `FinalizeBatch`
  advances a cursor through the leaf-transform pass a bounded number of leaves at a time (so a very large
  batch does not need to fit in one transaction, and every resumed call needs the same authority signer),
  then combines the whole tree in one pass once every leaf is transformed. The resulting `acc` commitment
  is computed by
  [`rome-zk-layouts`](../../crates/rome-zk-layouts) — the exact same function the settlement program and
  every off-chain client use — so there is only one implementation of "what a batch's commitment means" in
  this whole system, not a program-side copy that could drift from a client-side one.
- **A sealed chunk is immutable in bytes and length.** `Seal { len, body_hash }` requires `body_hash ==
  keccak256(body[..len])` — computed from the account's own bytes, not merely asserted by the caller — so a
  short-seal (a hole a `Write` never covered) is unconstructable, not just a client-side bound. `Write` on
  an already-sealed chunk is rejected outright, and a re-`Seal` that would change the already-stored `len`
  is rejected with a dedicated error; a re-`Seal` with the *same* `len` (a client resubmitting after a
  dropped confirmation) is an idempotent no-op, not an error.
- **`OpenBatch` commits a clock reading, not just a slot.** One `Clock::get()` call writes both
  `open_slot` and `open_unix_ts` (`Clock::unix_timestamp`) onto the batch account — the anchor the
  derivation node's one-sided timestamp drift bound checks every block's declared timestamp against.
  `open_unix_ts` is not part of the accumulator (`acc` still binds only the DA bytes) — it is
  authoritative because this program wrote it. The batch account's header is versioned; a version bump
  (like this one, 1 → 2) is a breaking, non-upgradable change — every reader refuses an old-version
  account by name, and there is no migration path.
- **Every account this program creates is safe against pre-funding griefing.** Every creation site — the
  batch account, the chunk account, the per-chain cursor — goes through
  [`rome-zk-pda`](../../crates/rome-zk-pda)'s create-or-adopt helper; a batch or chunk PDA that was
  pre-funded with lamports before this program's own instruction runs is adopted, not permanently blocked.
- **Data is never deleted before its root is final.** `Close` (a chunk) requires the covering batch to be
  finalized and its settlement-side root to already be final — reading the settlement program's root
  account directly, not trusting a caller's claim. The one exception: a chunk whose batch was never
  posted at all (never created, or `AbandonBatch`ed) may be closed by its own chunk authority alone, since
  there is no data availability left to protect in that case. A chunk `Close` takes the settlement
  program from the owner of the root account it is given, and requires the root, the chunk and the batch
  addresses to be the ones derived under that program; when the batch account still exists it must also
  record that same settlement program. A root owned by any other program therefore cannot be used to
  close a chunk.

## Config

None — this is a Solana on-chain program; all of its behavior is driven by instruction arguments and
account state, never by files or environment variables. `programs.json` (generated at deployment time, not
committed) records which program id a given deployment uses — see
the operator's deploy config.

## How to test

```sh
cargo build-sbf --manifest-path programs/zk-inbox/Cargo.toml --sbf-out-dir target/deploy
cargo test -p zk-inbox
```

Building first is required: this program's own test suite runs its compute-unit-sensitive tests (the
900-leaf and 2,500-leaf finalization tests, in particular) against the real compiled `.so` so their
compute-unit assertions reflect actual BPF execution, not the native-processor fallback
`solana-program-test` would otherwise use.

## Security notes

Every attack class this program specifically defends against has a dedicated test proving it: a batch or
chunk PDA pre-funded by an attacker before the real `OpenBatch`/`Open`/`InitBatchCursor` call still
succeeds (`open_batch_succeeds_even_when_an_attacker_prefunds_its_pda`,
`open_chunk_succeeds_even_when_an_attacker_prefunds_its_pda`,
`init_batch_cursor_succeeds_even_when_an_attacker_prefunds_its_pda`); a batch id that is not the cursor's
current value is rejected outright (`open_batch_rejects_a_batch_id_that_is_not_the_cursors_next_batch`);
abandoning a batch never rewinds the cursor, so its id stays permanently unusable
(`abandon_batch_never_decrements_the_cursor_so_the_same_id_stays_rejected`); closing a chunk before its
root is final is rejected (`close_batch_before_final_root_errors`); and a party who is not the batch's
authority cannot finalize it, even after permissionlessly sealing every one of its leaves itself
(`finalize_batch_by_a_third_party_after_it_permissionlessly_sealed_every_leaf_still_errors`,
`finalize_batch_signed_by_the_wrong_key_errors`,
`finalize_batch_with_an_unsigned_authority_account_errors`,
`finalize_batch_missing_the_authority_account_errors`) — without this, a third party could finalize a
later batch id ahead of the real poster's own still-open one, stranding it.

Accounts under a settlement program other than the chain's own are at addresses the chain never reads:
`tests/third_party_settlement.rs` checks that opening a batch, initialising a cursor, opening and sealing
chunks, and closing chunks through a different settlement program leave the chain's own cursor, batch and
chunk accounts untouched, and that a chunk `Close` against another program's root is refused.
`tests/cu_measurement.rs` prints the compute units of `InitBatchCursor`, `OpenBatch` and chunk
`Open`/`Write`/`Seal`/`SealLeaf` on the compiled program.

A short-seal (a caller claiming a length the account's bytes do not actually hold) is rejected at the core:
`Seal`'s `body_hash` is checked against `keccak256(body[..len])` recomputed from the account itself, and a
sealed chunk's `len` cannot change afterward — `write_after_seal_is_rejected_and_bytes_unchanged` and
`reseal_with_a_shorter_len_after_seal_leaf_is_rejected_and_header_unchanged` (`tests/accumulator.rs`) prove
both halves on real BPF. Without this, an authority could re-`Seal` a shorter `len` after `SealLeaf` had
already committed a leaf hash over the longer body — Solana DA would then no longer reproduce that leaf,
undetectable at the settlement program's `PostRoot`. This program's own error namespace (`ChunkError`,
`Custom(100)`/`Custom(101)`) is numbered apart from the batch accumulator's (`BatchError`, `Custom(1)`
through `Custom(13)`) so the two never alias each other in an error code a caller inspects. See
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#threat-model) for this alongside the other attack
classes this system closes or bounds.

## What the design covers

The chunk header and channel/frame format; the batch account, its sizing and growth, and the accumulator in
full (including the growth-by-instruction sizing and the sequential-cursor batch-id decision); and the
security model, including the DA-deletion-before-finality requirement `Close` enforces, and rent recycling.

## Depends on

[`rome-zk-layouts`](../../crates/rome-zk-layouts) for every account layout and the accumulator's
commitment formula; [`rome-zk-merkle`](../../crates/rome-zk-merkle) for the Merkle reduction underneath
that formula; [`rome-zk-pda`](../../crates/rome-zk-pda) for account creation. Consumed by
[`zk-inbox-client`](../../crates/zk-inbox-client) and, indirectly through it, by
[`rome-zk-batcher`](../../crates/rome-zk-batcher).
