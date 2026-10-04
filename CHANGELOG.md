# Changelog

This changelog describes the system as built on `main`, grouped by component. It does not follow a
release-tag cadence yet — see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for what each component does
and how they fit together.

## Inbox cursor v2

- `InitBatchCursor` creates the batch cursor as a 69-byte version-2 account: `deposit_next` 0, `deposit_hash`
  the deposit queue's seed hash for the chain, `deposit_final` 0. It takes no new argument.
- `CloseBatch` takes the chain's cursor as a fourth, writable account. After the final-root check it raises a
  version-2 cursor's `deposit_final` to a version-3 batch's `deposit_to` when that is higher, and never lowers
  it; a version-1 cursor or a version-2 batch is left alone. `close_batch_ix` in `zk-inbox-client` passes the
  cursor.

## Bridge deposit queue setup

- `zk-bridge` gains `InitBridgeConfig` (4), `InitDepositQueue` (5), `ProposeDepositParams` (6) and
  `ActivateDepositParams` (7). Tag 3 is held for the deposit instruction. The bridge config names the one
  settlement program and the one inbox every deposit queue is bound to, and is written once by the bridge's own
  upgrade authority. A queue is created by the chain authority, only for a chain whose root and registry belong
  to the config's settlement program and whose registry names the config's inbox. Parameter bounds (deadline 1 to
  24 hours, at most 256 per batch, a minimum of at least 1 base unit, a fee of at most 0.01 SOL) are fixed in the
  program. Parameter changes take a proposal and an activation at least one challenge window later. `InitVault`,
  `Fund` and `ReleaseExit` are unchanged. `zk-bridge` now depends on `solana-sdk-ids` and
  `solana-loader-v3-interface`.

## Prover input: deposits

- `build_batch_input` fills `deposit_from`, `deposit_hash_from` and `deposits` from a v3 batch header's range and its
  deposit records; a v2 header keeps the empty fields. `fetch_deposit_records` reads the bridge program from the
  chain's `exit_config` (a missing one, a zero bridge program and another chain's are refused by name), reads the
  records from that program's accounts and checks their hash chain against the header, and `fetch_and_verify_batch` checks `acc` with
  the header's range. Over account fixtures of the small synthetic batch the built input equals the committed
  `.bin` byte for byte and its public values equal the sidecar's.

## Synthetic deposit batches

- `synth-deposit-batch` (in `rome-zk-prover-input`) builds a wire v3 guest input that carries deposits, plus a JSON
  sidecar with the expected 208-byte public values and the intermediate values (the hash chain ends, `forced_root`,
  `acc`, each block's withdrawals root). The blocks come from a local reth node (peer discovery off, no peers) driven
  through `testing_buildBlockV1` with each block's deposit withdrawals; the records, hash chain and `forced_root` come
  from the deposit functions in `rome-zk-layouts`, the stream from `set_deposits_end` and `cut_frames`. Keys and
  addresses derive from fixed seeds; nothing is secret or funded. The settlement program is
  `rome_zk_testkit::fixed_settlement_program_id()` and each sender is the pubkey of a fixed-seed keypair
  (`rome_zk_testkit::synthetic_depositor_keypair`), so program tests can sign `Deposit` and reproduce the batch's hash
  chain, `forced_root` and `acc`.
- Two committed batches, `fixtures/prover-input/synthetic-deposits-{small,full}.{bin,json}`: `small` is 3 deposits
  over 2 blocks with an empty block between them, `full` is 4 deposits in each of 60 blocks (240). A unit test
  re-derives every hash value in each sidecar from its `.bin`.
- `scripts/synth-deposit-batch.sh` runs the node and the generator; `scripts/tests/synth_deposit_batch_flags.sh`
  checks that the node starts with discovery off and no peers.

## Prover input: wire v3

- The guest input (`RomePublicInput` in `rome-zk-prover-input`) gains four fields after `blocks`: the settlement
  program, the first deposit index of the batch, the deposit hash chain value at that index, and the batch's deposits
  (sender, recipient and amount in gwei; each index is the first index plus its position). The layout matches the
  guest that takes deposits, and a cross-repository test round-trips it both ways.
- `build_batch_input` writes an empty deposit range for now: the settlement program from its config, first index 0,
  and the queue's starting hash for that settlement program and chain id. Real ranges come with the deposit build.
- The two recorded inputs (`txv1-dev-batch-3930.bin`, `txv1-dev-reset6-batch-1.bin`) are migrated to the new layout
  by a test that shows every earlier field is unchanged.
- The prover can only feed a guest built for this input. A chain proving with an older guest keeps the image it runs.

Node images are published as `ghcr.io/rome-protocol/rome-zk-evm`. `v0.1.2` carries everything up to
"Inbox accounts keyed by the settlement program". `v0.1.3` adds the batcher compute-unit limits, batcher
restart recovery, the withdrawals rule, the deposit hash functions and the channel's `deposits_end` field.
The operator CLI and the rest of "Deposit groundwork" are on `main` and not yet in a published image.

## Bridge program set once

- `ProposeExitConfig` refuses a proposal that names a bridge program once the chain's exit config holds
  one (`BridgeProgramSetOnce`, 86), the same bridge included. Portal, cap and bond proposals are unaffected.
- `ActivateExitConfig` drops the bridge part of an older pending proposal when a bridge is already set, applies
  its portal, cap and bond parts, clears the pending slot and logs the dropped bridge. It never refuses over it,
  since a pending proposal cannot be cancelled. The exit-config layout, every account list and
  `PostRootProved`'s size are unchanged.

## Operator CLI

- New crate `rome-zk-ops` with the binary `rome-zk-ops`: `chain-id`, `register`, `refund-deposit`,
  `exit-config propose|activate|show`, `migrate` and `init-cursor`. A command is a dry run unless `--confirm`
  is given; the dry run prints the signed transaction and sends nothing. Every transaction goes out as a V1
  transaction through `rome-zk-solana-sender`, and keys are read from file paths and never printed. Each
  refusal has a name and an exit code. `refund-deposit` is the first caller of `refund_deposit_ix`: it gives a
  chain's registration deposit back to its authority once a batch has finalized or ten have been posted.
- The bridge commands: `vault init`, `vault fund`, `vault show` and `release-exit`. They reuse
  `zk_bridge_client::vault_tool` and the release-exit logic, send one V1 transaction through
  `rome-zk-solana-sender` (`release-exit` carries the recipient's token-account creation in the same
  transaction), and are a dry run unless `--confirm` is given. The `vault` and `release-exit` examples of
  `zk-bridge-client` are thin wrappers over them with the same flags and the same dry-run default, and its
  `devnet-driver` feature is now empty (it pulled in an RPC client, `base64` and `bincode` that nothing uses).
- The devnet driver examples of `zk-inbox-client` and `zk-settlement-client`, and `find_max_batch_id`, send
  through the V1 sender. The inbox `devnet_driver` no longer probes the deployed program with a legacy
  simulation; it deploys a throwaway program only when given `--throwaway`. The settlement `devnet_driver`
  no longer runs a separate `RootView` simulation before sending it.
- A shell test, `scripts/tests/no_legacy_tx.sh` (run by the shell-tests job), refuses a legacy transaction or
  message constructor in client code under `crates/` and `programs/`, and proves it fails on a planted line.
- The root `Dockerfile` ships `rome-zk-ops` beside the sequencer, batcher and derive binaries, and pins the
  runtime user to uid and gid 999. Published node images up to `v0.1.3` do not include it; the next tag will.
- `rome-zk-solana-sender` can sign with co-signers beside the fee payer, for a transaction with a second
  required signer.
- `zk-settlement-client` gains `ops_plan`: the registration nonce lookup, the recorded nonce and chain id pair,
  and the exit-config `pending_mask`, shared by the CLI and the tests.
- The `register_chain`, `migrate_chain` and `init_cursor` examples, and the three exit-config subcommands of
  `governance`, are now thin wrappers over the CLI with the same flags; they still send unless `--dry-run`
  is given. The other `governance` subcommands send through the V1 sender instead of building a legacy
  transaction. The `devnet-driver` feature no longer pulls in `reqwest`, `serde_json`, `base64` or `bincode`.

## Deposit groundwork

Deposits are not live yet: no program takes a deposit and the sequencer credits none. These changes add
the formats a deposit will use. A chain with no deposits builds, logs, encodes and commits exactly the
bytes it did before, and the tests pin that against the old encodings.

- `rome-zk-layouts::deposit` holds the deposit queue's hash formulas: the starting value `h_0`, a deposit's
  leaf, the hash chain, a batch's `deposits_commitment` and the two-lane `forced_root`. They are pure
  functions, so the programs, the services and the guest compute the same bytes. `forced_root` over an
  empty range is the existing empty constant.
- `rome-zk-channel`: a block gains an optional fifth field, `deposits_end`, the deposit queue's index after
  the block. A block without it encodes as the same four-item list as before, and decoding accepts four or
  five items. A fifth field that does not move past the previous block's value is refused.
- `rome-zk-layouts::deposit_queue` holds the bridge's deposit accounts: the one-time bridge config, a chain's
  deposit queue and a single deposit record, plus `amount_gwei`, which converts a token amount to gwei and
  refuses a mint with more than 9 decimals.
- `rome-zk-layouts` and `zk-inbox-client` read a version 2 batch cursor, which adds the deposit queue's
  cursor (21 to 69 bytes), and a version 3 batch account header, which adds the batch's deposit range (210 to
  290 bytes). Every earlier offset stays where it is. The inbox program still writes cursor version 1 and
  header version 2. `zk-inbox-client::reference_commitment_with_deposits` computes a batch's commitment
  with a deposit range; with an empty range it equals `reference_commitment`.
- Blocks that carry withdrawals: `BlockEnv` gains `withdrawals: Vec<Withdrawal>`, empty for a block with no
  deposits, and is no longer `Copy`. The executor builds a block with its withdrawals credited after its
  transactions. In `rome-zk-log` the sub-block header takes an optional ninth item, `deposits_end`, on the
  first sub-block of a block that credits deposits, and that record carries the block's withdrawals after
  its transactions. The header hash and the sequencer's signature cover `deposits_end`. Sequencer restart
  recovery replays a logged block with its withdrawals.

## Withdrawals rule

- `rome-zk-executor-api` gains `deposit_withdrawal(index, recipient, amount_gwei)`, `withdrawals_root(&[Withdrawal])`
  and `canonical_header_rule_with_withdrawals(chain_id, number, fee_recipient, &[Withdrawal])`. They give the
  sequencer, derive and the stateless validator one definition of a block's withdrawals and their root. The crate
  now depends on `alloy-eips` and `alloy-trie` with default features off. `canonical_header_rule` and
  `EMPTY_WITHDRAWALS` are unchanged, and with an empty list the new rule is exactly the old one. `BlockEnv`
  gained a `withdrawals` field later; see "Deposit groundwork".

## Batcher: deposits in the stream

- `BlockSource` reads each block's withdrawal indices off the ordered log and sets the block's `deposits_end`
  (last index plus one; none for a block without withdrawals). Indices must run on without a gap, and the first
  index after the resume point must equal the cursor's `deposit_next` or the previous batch's end, or the batcher
  stops with a named error. `SizeCappedGrouper` keeps the running value, encodes it with `set_deposits_end`, and
  closes a group before a block that would take it over `max_per_batch` (the stricter of the active and pending
  values; new close reason `deposits`). A log without deposits produces byte-identical frames. The header
  readers and the send path are unchanged.

## Sequencer: deposits in blocks

- `rome-zk-sequencer` has an optional `[deposits]` config section (Solana RPC URL, settlement program, poll
  interval; the bridge program comes from the chain's `exit_config`). With it, a poller reads the deposit queue
  and its records at finalized commitment, one `getMultipleAccounts` per 100 records, and each block credits
  every finalized deposit not yet included, oldest first, up to the queue's per-block cap and never past the
  finalized count. Every credit is `deposit_withdrawal`, logged with its record, and the block's first header
  carries `deposits_end`. A waiting deposit makes a tick non-idle. After a restart `deposits_end` resumes from
  the log. New metrics: oldest waiting age, finalized count, credited total, deposit-to-balance time and poll
  errors. Without the section nothing changes: no poller, no new metric, identical blocks.

## Batcher restart recovery

- A restart no longer abandons a half-written batch. If the batcher stopped between `OpenBatch` and
  `FinalizeBatch`, the old startup sent `AbandonBatch` for that id and posted the blocks again under a new
  one. Settlement posts exactly the next batch id and needs that id's batch finalized, and an id the inbox
  cursor has passed can never be opened again, so the chain stopped for good at the abandoned id. The startup
  sweep and `ResumeAction::Abandon` are gone.
- On start, the batcher now finishes each open, unfinalized batch in the pending window under its own id
  (`recover.rs`, called from `pipeline::startup_recover`). It tries each possible last block, smallest first,
  from the block after the previous batch up to `blocks_per_batch` blocks long, and keeps the first grouping
  whose frames number exactly what the batch expects, whose frames already on chain match byte for byte, and
  whose blocks pass derive's drift rule. It sends the missing frames, finalizes (continuing if finalize had
  started), checks `acc` and hands the batch off. If every frame is already on chain it reads them back and
  does not search. With a posting window deeper than one it resumes the batches in order.
- If no grouping fits (the compressor version, frame size, `blocks_per_batch` or the log changed since the
  crash), the batcher stops with the named error `ResumeImpossible { batch, leaves_present, expected_count }`,
  sends nothing and leaves the batch open. Rerun with the build and config that opened it. The
  `FinalizedAboveOpenBatch` refusal stays.
- Running `AbandonBatch` by hand on a batch settlement still needs halts the chain. The batcher README and
  the devnet guide now say so.
- Tests: `tests/restart_mid_batch_keeps_settlement_live.rs` runs the real inbox and settlement programs
  through a crash after one frame and a restart, and requires settlement's `PostRoot` for that id to succeed
  with no `AbandonBatch` sent. The old tests that asserted the abandon behavior are removed.

## reth-verifier: no peer discovery

- The stock reth verifier in `deploy/rollup` now starts with `--disable-discovery --max-outbound-peers 0
  --max-inbound-peers 0 --addr 127.0.0.1`. derive feeds it every block over the Engine API, so it never needs a peer; with discovery on
  it joined the public Ethereum peer network and dialed hundreds of nodes, which cloud providers flag as cryptocurrency
  activity. A test refuses a stock reth service without these flags or with a published devp2p port, and
  `./rollup check` gains a `verifier peers` item that fails if the verifier has any peer.

## Batcher compute-unit limits

- The default `chunk_compute_unit_limit` is now 100,000 (it was 40,000), and the three shipped batcher
  configs carry the same value. The inbox program derives addresses with a bump search, and every extra
  attempt costs compute units, fixed per address: 4,500 CU per extra attempt on the batch address in the
  open-and-grow transaction, and in a chunk transaction (about 16,400 CU before any extra attempt) 1,500 CU
  per extra attempt on the batch address plus 3,000 CU per extra attempt on the chunk address. The chance
  an address needs at least m extra attempts is 2^-m. The batch cursor cannot skip an id, so a transaction
  that does not fit its limit stops the chain at that id. Measured on the compiled program with the devnet
  program ids: open-and-grow at 900 leaves reached 55,431 CU over 256 batch ids (23,677 at best, 28,431 at
  the median), and a full-body chunk transaction reached 50,903 CU over 900 slots (26,903 at the median).
- New `open_compute_unit_limit` (default 400,000, set in the three shipped configs). The open-and-grow
  transaction is sent once per batch, so it has its own limit instead of sharing the chunk limit: two batch
  ids from a wider scan, 274,100 and 1,895,697, cost 109,536 and 132,063 CU, over 100,000. 400,000 leaves
  room for 83 extra attempts on the batch address.
- New `chunk_retry_compute_unit_limit` (default 400,000, set in the three shipped configs). A chunk-lane
  frame that fails on chain for running out of compute units is resent once at this limit instead of failing
  the batch (`Sender::send_and_confirm_many_retrying_compute`, with the retry inside `RpcSender`'s confirm
  loop). Only that frame pays the higher limit. Setting it to the chunk limit or lower turns the retry off.
- What is left: open-and-grow stops the chain only at 84 or more extra attempts on the batch address (2^-84 per
  batch id); a frame fails after its retry only past 383,600 CU of attempts, which is 128 chunk-address attempts
  on a batch address that needed none and 87 on one that needed 83 (below 2^-86 per frame). These are small
  numbers, not zero.
- Tests: `tests/bump_search_cu_limit.rs` scans those ids and slots, fails if the largest figure does not fit
  its limit, measures open-and-grow at the two tail ids, and requires 40 extra attempts of headroom in the open
  limit. `tests/chunk_compute_retry.rs` forces the retry on the real program. A batcher config test and the
  deploy tests pin the limits the shipped configs carry.
- Cost: the priority fee is the limit times the price, so a chunk transaction at the 1,000 micro-lamport
  starting price pays 100 lamports. The block cost cap charges the limit too, and the payer's ceiling falls
  from about 704 to about 292 frames per second at the 12,000,000 cap (1,408 to 585 at 24,000,000). A config
  that sets `chunk_compute_unit_limit` itself keeps its value, and one that sets no `open_compute_unit_limit`
  now sends open-and-grow at 400,000 instead of its chunk limit.

## Inbox accounts keyed by the settlement program

- Node image `v0.1.2` carries this change; the devnet inbox and settlement programs run it. A node on an
  earlier image derives the old addresses and stops posting against them, so move to `v0.1.2` (set `ROME_ZK_TAG`,
  run `./rollup init` and `./rollup up`), and initialize the cursor at the new address at
  `root.head_pending_batch + 1` (`./rollup register --confirm` does it for a registered chain).
- A chain's inbox accounts are keyed by its settlement program, so they can only be created through it.
  The batch cursor is now `["batch_cursor", settlement_program, chain_id]`, a batch is
  `["batch", settlement_program, chain_id, batch]` and a chunk is
  `["inbox", settlement_program, chain_id, batch, idx]`, all under the inbox program. No instruction
  gains a field and transaction sizes are unchanged.
- Chunk `Open` and `SealLeaf` read the settlement program from the batch account. A chunk `Close` takes
  it from the owner of the root account it is given and requires the root, chunk and batch addresses to
  be the ones derived under it. Settlement derives the inbox batch with its own program id and checks the
  batch's recorded settlement program (`WrongInboxAccount`).
- The batcher, derive, prover, prover-input, clients and bench tooling derive inbox addresses with the
  chain's settlement program. The settlement watcher does not derive a batch address: `run_once` and
  `decode_inbox_tx` take the settlement program it follows, record an `OpenBatch` only when the
  instruction names that program, and record a chunk `Open` only when the chunk account is the address
  derived under it. An open for the same chain and batch under another settlement program creates no row
  and cannot take an existing one. `rome-zk-prover-input`'s `fetch_and_verify_batch` and
  `fetch_real_batch --settlement-program` take it as a new argument.
- Chunk `Close` now derives three addresses, so it costs more compute. A packed group of closes in the
  batcher asks for one chunk compute budget per close.
- The guest, the public values and the verification key are unchanged.
- Existing deployments: accounts created under the old chain-id-only addresses are not read after the
  upgrade, so finish or abandon open batches and close their chunks first, then initialise the cursor at
  its new address with `next_batch = root.head_pending_batch + 1`. Never set it higher: settlement posts
  only that id next, so a higher cursor halts the chain. `examples/find_max_batch_id` now reads the
  root under the configured settlement program and prints that value; it ignores inbox accounts that
  belong to another settlement program.

## Permissionless chain registration and proved finality

- `InitChainV2` on a permissionless id (`chain_id >= 2^32`) now refuses a non-empty `registry_entries`
  with `RegistryEntriesNotAllowed` (82), before creating accounts, locking the deposit or advancing the
  authority's nonce. Rome adds the chain's key afterwards with `SetRegistryEntry`. Reserved chains
  still accept initial entries with the registry authority's co-signature.
- Permissionless chains are proved-only. `PostRoot` rejects them with `UnprovedRootNotAllowed` (83)
  before any account read, write or fee transfer. Rome's reserved-range chains keep the unproved path.
- A permissionless chain has no final root until its first proved batch. `final_root_tuple`, shared by
  `RootView` and `ProveExit`, returns `NotFinal` for the owner-supplied genesis (batch 0) while
  `head_final_batch == 0`. Reserved chains keep their genesis as final.
- On every chain, both `PostRootProved` layouts require the proof's `parent_hash` to match the
  predecessor's last block hash: `root.block_hash` when `head_pending_batch == 0`, otherwise the
  predecessor pending account's `last_block_hash`. A mismatch returns `PredecessorHashMismatch` (84)
  before the pairing check or any write.
- The refund condition remains `head_final_batch >= 1` or `posted_batches >= 10`. With proved-only
  posting, the first proved root already meets the final-root trigger; the ten-post trigger remains
  defense in depth. The reclaim window still starts at registration, so Rome's key-registration
  turnaround must fit inside `reclaim_window_slots` and leave time for the first proved post.
- `SetRegistryEntry` refuses layout 2 (the header fallback) on a permissionless chain with
  `HeaderFallbackNotAllowed` (85), before any write: layout 2 binds neither the chain id nor the inbox
  commitment, so a permissionless chain registers layout-1 keys only. Reserved chains still accept layout 2.

## Zero-balance genesis in `rollup init` (2026-10-03)

- `./rollup init` now renders a genesis with no balances. The exit portal predeploy stays, at balance 0. The
  `genesis.funded_address` key, which minted 1e27 wei to one address, is gone, and `init` refuses a `chain.toml`
  that still sets it with `FundedAddressRemoved`, naming the keys that replace it. A genesis cannot change after
  registration, and a genesis that mints coins could never take deposits safely: its holder could exit coins that
  other people's deposits paid for.
- A chain can declare one backed balance for first gas with `genesis.backed_address` and
  `genesis.backed_balance_lamports`, both or neither. The amount is a whole number of lamports from 1 to
  18446744073709551615, and `init` renders wei as lamports times 1e9, so it converts exactly into the vault's
  wrapped SOL (1 lamport is 1 gwei). The address is refused by name for zero, the precompiles and the exit portal
  (`BackedAddressReserved`), or if malformed (`BackedAddressInvalid`). A balance that is not a plain integer in
  range is refused with `BackedBalanceInvalid`, and one key without the other with `BackedAddressMissing` or
  `BackedBalanceMissing`. Any other key under `[genesis]` is refused with `GenesisKeyUnknown`.
- With a backed balance, `init` prints the exact lamport amount to lock in the chain's vault with `Fund`
  before asking Rome to register the key. Rome checks the lock before it registers the key. The vault lives on
  the shared devnet zk-bridge listed in `deploy/rollup/programs.devnet.json`, and `init` says so.
- The README, `chain.toml.example` and the devnet guide now describe zero balances and the optional backed balance.
- A chain initialised with `funded_address` keeps that genesis and can never take deposits. To use this version, start a
  new chain: move `rendered/` aside, remove `funded_address` from `chain.toml`, and run `init` with a new payer key.

## Chain id range and fee recipient in `rollup init` (2026-10-03)

- `./rollup init` now refuses a derived chain id above 4503599627370476 with `ChainIdNotWalletSafe`, before
  the chain is registered and before anything is sent. That is MetaMask's `MAX_SAFE_CHAIN_ID`, and wallets like it
  cannot add a chain with a larger id; about half of all permissionless ids are above it. The message tells the
  operator to create a new payer key (`solana-keygen new`) and run `init` again. If the chain is already registered
  (`rendered/pdas.env` exists, or the payer's nonce has moved past the recorded one), the id is fixed, so `init`
  prints `ChainIdNotWalletSafe` as a warning once and carries on.
- `chain.toml` takes a required `genesis.fee_recipient`, and the rendered genesis coinbase is that address
  (the template used to hard-code zero, so a chain's priority fees went to an address nobody can spend from). `init` refuses it by name when it is
  missing (`FeeRecipientMissing`), malformed (`FeeRecipientInvalid`) or reserved (`FeeRecipientReserved`: zero, a
  precompile address `0x00..00` to `0x00..ff`, or the exit portal at `0x42..16`). The sequencer and derive read the
  fee recipient from the genesis coinbase, so nothing else needs to change.

## Portable rollup deploy for outside operators (2026-10-02)

- New `deploy/rollup`: one folder an outside operator can use to run their own chain with a sequencer, batcher,
  deriver and prover. `./rollup init` reads `chain.toml` and the operator's payer key, reads the registration
  nonce from Solana, derives the chain id from it, records it, and renders every config and the genesis from
  that one file. `./rollup register` registers the chain, `./rollup up` starts the services and `./rollup check`
  tests them by name. Nothing in it reaches a cloud service, and every published port is on loopback.
- The prover image checks its proving keys with `deploy/rollup/prover/check-keys.sh` before every start. The
  key hash is now computed bytewise (`LC_ALL=C`) over paths relative to the key directory, so the pin made on
  the host and the check inside the container agree whatever the host's locale or the keys' location.
- `register` can be run again after a partial failure. It first checks that the payer key is the authority
  `init` recorded and stops with `AuthorityChanged` if it is not. If the payer's nonce has moved, it reads the
  chain's root account at `confirmed`; a chain that is already registered is reported as such and the run
  carries on with the batch cursor, and nothing is sent twice. Only a nonce that moved with no root account
  stops with `NonceAdvanced`. If the root account cannot be read on the RPC, it stops with `RootLookupFailed`
  and says to run `register` again.
- A lookup that fails on the RPC now says the request failed (`ChainIdLookupFailed`, `RootLookupFailed`,
  and `NonceLookupFailed` in `register_chain`), so a rate-limited node is not mistaken for a missing account.
- `register_chain` has a `chain-id` subcommand that prints the next permissionless chain id with the
  authority and nonce it came from, and takes `--nonce` and `--expect-chain-id` so the registration sends the
  values `init` recorded and the program refuses a pair that went stale. `init_cursor` exits 0 when the cursor
  already exists.

## Fast-finality defaults for sending (2026-10-02)

- A send now counts as sent only once it is `finalized`. `rome-zk-solana-sender` has a new
  `confirm_commitment` setting, `finalized` by default and `confirmed` when chosen; the batcher, prover and
  exit-prover each take it in their config. On devnet (Alpenglow) `finalized` trails `confirmed` by 0 to 1
  slot, so this costs nothing there; on a cluster still running TowerBFT it is about 31 slots.
- `confirm_timeout_secs` now defaults to 15 seconds (it was 60). It no longer decides when to resubmit: a send
  is resubmitted only after its blockhash has expired, a transaction already `confirmed` is never resubmitted
  (it is waited on until `finalized`), every signature a send has used is watched so a late original still
  counts, and the timeout is the unit of the give-up ceiling (ten times it): past it nothing is resubmitted and no queued frame or later stage is started, but a send that has landed or can still land is waited on, and the error is returned only once none can, or once the cluster's block height has not advanced for twenty times the timeout (a stall, not wall time, so a slow cluster that keeps advancing never fails a send). A send that had landed when the stall ended it gets its own error saying it executed and may still finalize. The single-send path (OpenBatch,
  Finalize, Close, PostRoot, the prover's finalize and close, ProveExit) now runs on the same loop as the
  chunk lane. The exit-prover and prover also get a
  `confirm_poll_interval_ms` setting (default 500), and the batcher's existing one now also paces the
  single-send path. The deployed Tiber batcher config still sets 60 explicitly and is unchanged.
- The exit-prover's comments that said the settlement reader lags by "about 32 slots" now describe the
  cluster's confirmed-to-finalized gap, which was measured on devnet (0 to 1 slot) and mainnet-beta (30 to 31
  slots).

## Veritas replaces the proof verifier (2026-10-01)

- `programs/veritas` is now the only PLONK verifier. It is Rome's own implementation, written from Rome's
  own functional specification of the proof system, and it replaces the earlier verifier program, which
  was a port of a GPL-licensed file and is deleted. `zk-settlement` and `rome-zk-prover` link it with the
  `no-entrypoint` feature; it also builds as a standalone program (`veritas.so`, 55,480 B).
- Old and new agree exactly. A throwaway comparison ran 20,020 cases (every corrupted-proof family, 6,144
  single-bit flips of the real proof through two entry points, and flips of the verifying-key and root
  inputs), and the two verifiers returned the same result in every one: accept, reject, or the same error.
- Compute units on real BPF (SBPF v3), old to new: `verify_zisk` on the block-14 proof 541,225 to 442,210;
  `PostRootProved` that reaches the pairing about 557k to 562k, now about 453k to 459k. The settlement
  tests keep their bounds (a rejection stays under 60k, reaching the pairing stays over 400k) and the verify
  pin is now below 460k.
- On the shared Solana devnet, `veritas` is deployed at `2cLGd9FKC7TiZrEHCvpw291AwXNcGgP9nDT4W3PHLe5k`,
  listed in `deploy/rollup/programs.devnet.json`. Tiber's deploy script now deploys `veritas` instead of the
  old verifier; Tiber gets its program id at its next chain reset.

## Solana crate line bumped to Agave 4.3.0 + SBPF v3 everywhere (2026-10-01)

- The earlier workspace pinned every program and client at the `solana-program = "=2.1.6"` generation. Its
  `solana-program-test` cannot run a program built for SBPF v3 (`InstructionError(0, InvalidAccountData)`
  on the first instruction — the harness, not the program, was the blocker). Public devnet and mainnet-beta
  still deploy and run v0 programs today (SIMD-0500 and SIMD-0161 are not active), so v3 is not forced on us
  yet; we build v3 now so we keep working after they activate, and `scripts/check-sbpf-v3.sh` is the only
  guard against a v0 artifact (the 4.3.0 test harness runs v0 programs fine, so no test would notice). The Agave crate graph no longer shares one version number across its component crates, so
  this bump assembles ONE consistent set from `solana-program-test`'s and `solana-client`'s own published
  dependency requirements (two earlier blanket-`sed` attempts failed to resolve) and pins it once, in the
  root `Cargo.toml`'s new `[workspace.dependencies]`, inherited by every one of the 20+ in-workspace
  manifests via `<crate>.workspace = true` — never a per-manifest literal version again
  (`scripts/check-workspace-deps.sh`, wired into CI).
- Every on-chain program (`zk-inbox`, `zk-settlement`, the proof verifier, `zk-bridge`) and the test-only
  stub-bridge fixture now build `cargo build-sbf --arch v3`; `scripts/check-sbpf-v3.sh` reads each produced `.so`'s
  ELF header and refuses a v0/v1/v2 artifact by name, wired into the CI `test` and `build-sbf` jobs and the deploy
  script's own pre-deploy check; `scripts/check-platform-tools.sh` refuses a stale installer (< v1.54) before any
  `--arch v3` build runs.
- The earlier "two generations" split in `rome-zk-batcher`/`rome-zk-solana-sender` (`solana-client` vs.
  `solana-client-v1`, etc.) is no longer a bridge between two semver-incompatible crate versions — both names
  now resolve to the identical workspace-pinned package, kept only so existing
  `solana_client_v1::`/`solana_transaction_status_client_types_v1::` call sites need no rename. The current
  `spl-token`/`spl-associated-token-account` releases now resolve onto the workspace's single `solana-program`
  4.1.0 and accept its `Pubkey`; `programs/zk-bridge/src/token.rs`'s hand-rolled SPL wire is unchanged
  (replacing it is a separate decision, not this bump's). The pinned CU figures were re-measured on the real
  v3 artifacts under the new crate set (old value in brackets): `ProveExit` 40,248 (41,342); `ConsumeExit`
  whole tx 20,455 (20,480); `ConsumeExit` nested in `ReleaseExit` 15,835 (15,794); `InitVault` 30,753
  (39,998); `ReleaseExit` whole tx 45,873 (54,506); `Fund` 8,902 (7,354); `Seal` of a 3,681-B body 2,872
  (2,841); ZisK PLONK `verify_zisk` 541,225 (about 543k); `ProposeExitConfig` 9,355 (12,614);
  `ActivateExitConfig` 5,248 (6,744); 900-leaf `FinalizeBatch` 334,459 (372,951, now also the batcher's
  `MEASURED_900_LEAF_FINALIZE_CU`). Nothing crossed a documented budget. Two of the first-draft figures did
  not reproduce because their tests used `Pubkey::new_unique()`, so the PDA bump search and the printed CU
  depended on which tests ran in the same process: `InitChain` and `InitChain` against pre-funded PDAs now use
  the fixed settlement program id (27,917 and 34,545), and the `Fund` test now uses the fixed mint (it printed
  8,902 alone and 7,402 in a full run, so the 7,354 to 8,902 "rise" is not a like-for-like comparison).
  `InitChainV2`/`PostRoot` CU (32.0k-44.3k and 26.3k respectively) are new measurements, never previously
  pinned in this file.
- Follow-up fixes: `scripts/check-workspace-deps.sh` now parses every tracked `Cargo.toml` with
  `tomllib` and sees all dependency shapes (inline, `[dependencies.x]` table, renamed `package =`, and
  `target.*` sections), and compares the versions in the crates outside the workspace with the workspace
  ones; `scripts/check-sbpf-v3.sh` refuses a file that is not a 64-bit BPF ELF before it reads `e_flags`;
  the Tiber program deploy script checks every artifact is v3 before its first cloud call,
  top-up or deploy; `examples/find_max_batch_id.rs` stops on an account it cannot decode instead of
  skipping it. Stale "pinned to 2.1.6" comments, the runner and prover-image Rust versions (1.97.1) and
  the claims that devnet and mainnet-beta run v3 only were corrected.
- Living docs: `docs/ARCHITECTURE.md` (toolchain line), `README.md` and the contributor notes (prerequisites),
  the test validator's README and entrypoint script (SIMD-0500 deactivation stays until that validator's own
  programs are redeployed under `--arch v3` — a separate deploy step, not part of this change), and every
  crate README that named the old `2.1.6` pin.

## Tiber reset #6 (2026-09-15)

- Reset #5's chain had its inbox cursor bootstrapped at 0, so its first batch (blocks 1..60) could never
  be posted: settlement requires the first postable batch to be 1 with `first_block == 1`. With batch ids
  now 1-based everywhere, the chain was re-registered on freshly deployed programs (secret versions
  advanced again; the previous programs stay deployed and recoverable). Same genesis, same layout-1
  verifier key first in the registry, `chain_config` v2 (drift 60 s), `blocks_per_batch` 60, cursor at 1.
- Pins moved together: the deploy config's program list, the README's program/PDA table,
  `crates/rome-zk-layouts/tests/pda_pins.rs`, the batcher config.
- `zk-settlement-client` gained `layout1_proof_abi` and the `post_root_proved` example (the end-to-end
  gate's one-off sender) in the previous release step; recorded here since that change carried no entry.

## Tiber reset #5 (2026-09-14)

- Chain 200101 re-registered on freshly deployed programs (program keypair secrets advanced to their next
  version; the previous programs stay deployed and upgrade-authority-recoverable, their chain state
  orphaned). The genesis registry now carries three verifier entries with the layout-1 primary first —
  the reproducible guest ELF's own key, from `fixtures/vkeys/tiber-200101-layout1.json` — so a proved
  root is reachable for the first time; `chain_config` is v2 from birth (drift bound 60 s); the sequencer
  profile's `blocks_per_batch` is 60 and the inbox and root both start at batch 1.
- Pins moved together: the deploy config's program list, the README's program/PDA table,
  `crates/rome-zk-layouts/tests/pda_pins.rs`, the batcher config.

## Initial build

### Tiber deploy — exit-pipeline plumbing and prover alerting

Four file-level changes, no service/cluster touched (files, tests, and dry-runs only — the recreate,
program deploy, and governance sends each need the operator's go-ahead and run after merge):

- **Verifier proof window.** `reth-verifier` (`docker-compose.settlement.yml`) now carries `--rpc.eth-proof-window
  ${VERIFIER_PROOF_WINDOW:-1000000}` — reth's own default (0) bounds `eth_getProof` to the current tip, which the
  exit prover cannot use (it proves against an older, already-finalized root; this was found live on Tiber). Raising
  the window costs nothing at rest (the node already runs archive, no `--full` flag); `VERIFIER_PROOF_WINDOW=0` is
  refused by name (`VerifierProofWindowZero`) in the settlement config render step.
- **`zk-bridge` joins the deploy list.** The Tiber deploy script's own `PROGRAMS`
  array gains `zk-bridge` (its `.so` was already in every CI `programs-<sha>` artifact — the
  `build-sbf` job globs `programs/*/`, nothing there needed to change). A new `--only <program>` flag
  lets a single-program deploy (zk-bridge, the first time it lands) skip re-deploying or re-rotating
  the other three; the `programs.json` write merges rather than overwrites when `--only` is used. No
  reader of `programs.json` assumes a fixed key count. The live, committed `programs.json` stays at
  three keys until an operator deploy actually writes the fourth — no id invented here.
- **Prover alert + GPU panel.** Grafana gains two alert rules — a prover-down rule (`up{job="prover"} == 0` for 5m)
  and a prover-behind-batches rule (`rome_zk_prover_batches_behind > 3` for 10m) — routed to a new `tiber-slack`
  contact point via Grafana's per-rule "simplified routing". No Slack webhook secret exists in the secret store yet,
  so the contact point is committed with its webhook read from an environment variable (Grafana expands it from the
  container environment; compose supplies a documented no-op default until the Slack webhook URL is set in `.env` —
  see "Tiber deploy, more fixes" below for how this shape replaced a render script). A new prover dashboard (picked
  up by the existing file-based dashboard provider) adds GPU utilization/memory/present panels (the GPU metrics
  textfile script's own metric names) plus prover batches-behind/stage-wall-time/posts panels (`rome-zk-prover`'s
  own registered metric names) — every panel expr was checked against the real producers, not earlier planned metric
  names, some of which were never shipped.
- **Vault setup guide.** The Tiber deploy README gains a step-by-step "wire the vault" section — the
  verifier recreate, the `zk-bridge`-only deploy, `propose-exit-config --bridge-program` through
  `activate-exit-config`, `InitVault`/`Fund`, and a `release-exit --dry-run` — each step a
  copy-pasteable command, with the operator-only boundary marked.

New shell tests in the Tiber deploy tests directory: the proof-window test, the program-deploy dry-run test,
the Grafana alerting test — the last boots the real, pinned `grafana/grafana:11.4.0` image in CI and reads
back the provisioned rules/contact point over Grafana's own API.

### Tiber deploy fixes — the proof window knob, monitoring gaps, deploy hygiene, and the vault tool

Five problems closed, each test-first; still files/tests/dry-runs only, nothing live touched:

- **The proof window knob is now real.** `docker compose` substitutes `${VERIFIER_PROOF_WINDOW:-1000000}` from the
  Tiber deploy directory's `.env` (or the shell) — never from the rendered Tiber env file, which is what the docs
  advertised and the old guard (in the settlement config render step) validated: a value in that env file had ZERO
  effect on the rendered stack (confirmed live, then fixed). A new render step for the proof window, called by the
  step that pushes the Tiber files, right after the Tiber env file is rendered, resolves + validates the key
  (refuses a literal `0` or a non-integer BY NAME, `VerifierProofWindowZero`/ `VerifierProofWindowInvalid`, before
  writing anything) and carries the resolved value into `.env` — the one file compose actually reads, same
  `.block-gas-limit.env` -> `.env` pattern `BLOCK_GAS_LIMIT` already uses. The old guard is removed from the
  settlement config render step (it validated a file compose never reads). Exporting this variable in your own shell
  still overrides `.env` (Compose's own precedence) and bypasses the guard — documented, not supported.
- **Grafana now boots from the committed tree with no render step.** `contactpoints.yml` is COMMITTED
  (no longer gitignored+rendered-only): a fresh checkout previously had `rules.yml` (committed)
  referencing the `tiber-slack` contact point by name with no `contactpoints.yml` to satisfy it —
  `Error: ✗ alert rules: receiver tiber-slack does not exist`, a restart loop (verified live against
  `grafana/grafana:11.4.0`), taking every Tiber health check item
  that reads Prometheus through Grafana down
  with it. (That render step was later removed altogether — see below.)
- **A `prover-node` Prometheus job** scrapes the prover host's node-exporter on `:9100` (the same host
  `PROVER_TARGET` already names for `:9003` — the Prometheus render step derives the port swap, never a second
  independently-set host); the prover infra script's firewall rule already opens both ports, so `prover_gpu_*`
  finally has a scrape source. The prover-behind-batches rule gains a no-data state of OK (unlike the prover-down
  rule's `up{job="prover"}`, which Prometheus always populates once the job has any target, this rule's own metric
  is genuinely absent with no live prover host in dev — NoData is the correct steady state, not a signal). The
  alerting-render test now boots the mount-check off the grafana service's own `docker compose config --format json`
  volumes (not a whole-file grep) and tokenizes every dashboard panel `expr` so an invented metric alongside a real
  one is caught (a substring check would have missed it).
- **Program deploy script hygiene.** `--artifacts-dir`/`--only` with no value now refuse BY NAME (`MissingArgValue`)
  instead of bash's own raw "unbound variable", or silently swallowing the next flag as this one's value.
  `OnlyRequiresExistingProgramsJson` moved from write time (after the deploy loop) to right after `--only` is
  parsed, so `--dry-run` reaches it too — a fixture with no `programs.json` refuses before even printing the dry-run
  plan. The program-deploy dry-run test no longer touches the real Tiber
  deploy directory at all: its settlement config
  render check now runs an actual scratch copy of that script (its `batcher.settlement.toml`/`derive.toml`/`jwt.hex`
  outputs are not independently overridable) rather than `rm -f`-ing the real, live `jwt.hex` (the
  `reth-verifier`<->`derive` shared secret on the VM) after every run.
- **The vault tool.** `crates/zk-bridge-client/examples/vault.rs` — `init-vault`, `fund`, `show-vault`, same
  subcommand/`--dry-run`-default shape as `release-exit`/`governance`. `fund` reads `vault_config` back
  on-chain and refuses BY NAME (`VaultSettlementMismatch`, `zk_bridge_client:: check_vault_settlement`) before
  building `fund_ix` if the vault's own recorded `settlement_program` disagrees with what the command was
  given — `vault_config`'s address is a PDA *derived from* that value, so nothing on the wire otherwise
  catches a stale/wrong one. The Tiber deploy README's vault setup guide, steps 4 and 5, now use this tool
  instead of "no CLI wraps this yet".

New/changed tests, in the Tiber deploy tests directory: the proof-window test (rewritten — proves the
`.env` carry end to end via `docker compose --env-file`, not a shell-exported var),
the Grafana alerting test (committed-tree boot, tokenized metric check, format-json mount check),
the program-deploy dry-run test (`MissingArgValue`, the
moved-up `OnlyRequiresExistingProgramsJson`, no
real-directory mutation), the settlement config render
test (the `prover-node` job),
`crates/zk-bridge-client/tests/vault_example.rs` (new — unreachable-RPC refusals for both subcommands, plus a
real-BPF `solana-program-test` case for `VaultSettlementMismatch`).

### `rome-zk-exit-prover` — stateful follower: the chain is the state

The bin's old poll loop advanced its one `from_block` cursor for every log BEFORE looking at that
message's own `attempt_exit` outcome, so a `StateUnavailableAtRoot`/`QueuedForWindow` message was dropped
for good the moment the cursor passed its block — and `attempt_exit` never read the on-chain nullifier
bit before sending, so a restart that re-scanned from genesis re-sent (and paid a fee for) every
already-proved exit.

- `core::attempt_exit` now reads the `exit_nullifier` page for the message's `nonce >> 13` and checks its
  bit BEFORE `read_pending`/`eth_getProof`/the `Sender` are ever touched — an absent page account means
  the bit is clear (no exit on it has ever been proved). A set bit returns `Outcome::AlreadyProved`
  immediately, zero fee. This is ADDED to, not a replacement for, the existing on-chain
  `ExitAlreadyProved` (65) send classification, which still catches the rarer in-flight race.
  `SettlementReader` gained `read_nullifier_page`; `zk-settlement-client` gained
  `NullifierPageAccount`/`decode_exit_nullifier_account` (the header + bitmap read
  `programs/zk-settlement::exit::prove_exit` itself already performs, now shared).
- New `follower::Follower`: a pure, `Deps`-over-fakes state machine (mirrors `rome-zk-prover::follower`'s
  own "cursor derived from chain state" shape) keeping a `pending` map (keyed by message hash, each
  waiting on `Now`/`NextFinal{seen_head_final}`/`Slot(retry_slot)`) and a `stuck` map
  (`MaxSendAttempts`/`ProofTooLarge`). `ingest` decodes every log the RPC returned, counts (never `?`'s
  out) an undecodable one, dedupes by hash, and advances `scan_from_block` past every log regardless of
  whether it decoded. `due` returns which hashes to attempt now. `apply` routes an outcome: done
  (`Sent`/`AlreadyProved`) removes it; `QueuedForWindow` waits on the retry slot; every other named
  refusal is TRANSIENT (waits for the next Final root — a message initiated after the current Final
  batch's own last block being unavailable there is not a bug); `ProofTooLarge` goes straight to `stuck`,
  re-tried only once `head_final_batch` advances; `SendFailed`/`Err` counts toward `max_send_attempts`
  before going `stuck`. The bin is now wire-only: `ingest → due → attempt_exit → apply`, `--once` = one
  pass. No disk cursor, no database — a restart rebuilds everything from `eth_getLogs(portal,
  portal_from_block)` plus one nullifier-page read per pending message, at zero fee.
- New config knobs: `portal_from_block` (default `0`; Tiber's own deployed portal starts at `208399`,
  as recorded in the Tiber deploy config) and `max_send_attempts` (default `5`).
- New metrics: `rome_zk_exit_pending` (gauge), `rome_zk_exit_stuck{reason}` (gauge), `rome_zk_exit_scan_from_block`
  (gauge), `rome_zk_exit_log_decode_errors_total` (counter).
- `zk-bridge-client` gained `create_recipient_ata_idempotent_ix` (Associated Token Program
  `CreateIdempotent`, hand-rolled — no `spl-associated-token-account-interface` Cargo dependency, same
  reasoning as `programs/zk-bridge::token`) and the `release-exit` example: reads a PROVED `exit_record` +
  the vault's `vault_config`, derives the recipient's ATA, and sends
  `[create_recipient_ata_idempotent_ix, ReleaseExit]` — `--dry-run` by default, `--confirm` to send.
  Closes the gap that `ReleaseExit` reverts if `record.sol_recipient` has no ATA, at the client
  (the program only derives and checks the ATA, `release.rs:133-137`; provisioning is the caller's job).
- `programs/zk-bridge/tests`: real-BPF `create_ata_idempotent_then_release_lands_for_a_recipient_with_no_ata`
  loads the real Associated Token Program and proves the create-then-release lands, and that a second create
  against an already-existing ATA is a genuine no-op. `Makefile`'s new `BUILD_SPL_ATA_SBF` gets that `.so` via
  a read-only `solana program dump` of the real, deployed devnet program (its sha256 checked against a pin
  kept in the Makefile and CI) rather than a from-source build — `spl-associated-token-account`'s own
  `spl-token-2022` dependency floats to a release with a broken `solana-zk-token-sdk` combination on a fresh,
  uncommitted-lockfile resolve. Also fixes a latent quoting bug in `BUILD_SPL_TOKEN_SBF`/`remote-test` (a
  single quote inside an already-single-quoted `ssh` argument closed it early on the local shell) — caught
  here since this is the first change to actually exercise that exact recipe.

### `rome-zk-exit-prover` — follow-up fixes: read errors are free, cap pre-checks, exhaustive refusal routing

Three fixes to the follower above, closed in the same crate:

- **A read failure is no longer conflated with a send failure.** `follower::Follower::apply`'s
  `Ok(Outcome::SendFailed(_)) | Err(_)` arm used to treat BOTH the same way. Only `SendFailed` means the
  `Sender` was actually touched (a fee may have been spent) and counts toward `max_send_attempts`; an
  `Err(CoreError::{Read,Verifier,Build})` means `attempt_exit` refused before ever reaching the `Sender` —
  it is now free: the message stays `Wait::Now` with `send_attempts` untouched, and a new counter,
  `rome_zk_exit_read_errors_total{kind="settlement"|"verifier"|"build"}`, tracks it instead.
- **Cap pre-checks run before any fetch, not just before the send.** `QueuedForWindow` used to be reached
  only after an included, fee-paying `ExitCapExceeded` (68) response. `core::attempt_exit` now checks,
  after the existing `ExitCapUnset`/nullifier-bit gates and before `read_pending`/`eth_getProof`: (a) a
  message whose own `cap_units(amount)` exceeds the WHOLE `root.exit_cap_per_window` refuses
  `Refusal::ExceedsWindowCap` — no window under the current cap could ever admit it; (b) a new
  `SettlementReader::read_exit_window(window_index)` read (absent account = `0` spent, mirroring
  `read_nullifier_page`'s convention) checks whether the window this attempt would land in already has
  enough spent to overflow it, queuing to the next window at zero fee if so. The on-chain
  `ExitCapExceeded` classification stays for the race. `Follower::Pending` gained `window_requeues: u32`,
  bounded by a new `max_window_requeues` config knob (default `3`) the same way `send_attempts` is bounded
  by `max_send_attempts`; past the bound the message is `stuck`/`StuckReason::MaxWindowRequeues`.
  `Refusal::ExceedsWindowCap` itself goes straight to `stuck`/`StuckReason::ExceedsWindowCap`, re-tried
  only once `head_final_batch` advances (the cap is governance-changeable).
- **`apply` now matches every `Refusal` variant by name — no catch-all.** `Verify(RootMismatch |
  ProofInvalid(_) | ExitNotSent)` moves to `stuck`/`StuckReason::ProofInvalid{seen_head_final}` (re-tried
  once `head_final_batch` advances, same trigger as before, but now surfaced on the stuck gauge rather
  than folded into the merely-transient `NextFinal` bucket — a failed proof verify against the current
  root is worth distinguishing from ordinary "waiting on config/infra"). The remaining refusals
  (`StateUnavailableAtRoot`, `NoFinalBatch`, `ExitConfigUnset`, `ExitCapUnset`, `ChallengeWindowZero`) stay
  transient as before.
- **The bin's loop body is now testable.** `run::poll_once` lifts `ingest → due → attempt_exit → apply`
  out of `main` into a function driven by the same `Deps`-over-fakes fixtures the rest of the crate uses.
  An `eth_getLogs` failure is no longer propagated with `?` (which used to abort the whole process): it
  counts `rome_zk_exit_rpc_errors_total{method="eth_getLogs"}`, warns, and skips the poll. A `get_slot`
  failure no longer falls back to `.unwrap_or(0)` (which would have sent a `ProveExit` tagged for the
  WRONG challenge window at a real fee): it counts the same counter with `method="get_slot"` and skips the
  whole attempt round. `read_root`'s own conservative fallback (`head_final_batch` stays `0` on failure)
  is unchanged. The bin is now `loop { poll_once(...); sleep }`.
- Hygiene: the byte-identical duplicated comment block in `core::attempt_exit` is gone (one
  copy); `Makefile`'s `BUILD_SPL_ATA_SBF` now runs the same `sha256sum --check` CI's workflow step does,
  against a Makefile variable (`SPL_ATA_SHA256`) a new `make check-pins` target keeps in sync with
  `.github/workflows/ci.yml`'s own inline copy.

### `rome-zk-exit-prover` — more fixes: the healthy path is never "stuck", the bound never abandons, own sends count

Three further fixes to the change above, plus smaller ones that could be tested, made in the same change:

- **`Verify(ExitNotSent)` is transient again.** The previous change routed EVERY `Verify(_)` to
  `stuck`/`ProofInvalid`, but `ExitNotSent` is the `Verify(Absent)` case: the exclusion proof every fresh exit
  gets at a Final root older than the message — the ORDINARY first outcome, not a failure. It stays
  `pending`/`Wait::NextFinal`; only `RootMismatch`/`ProofInvalid(_)` are `stuck`. The inner `Verify(_)`
  wildcard is gone too (matched by variant). The test `verify_absent_is_transient_until_the_next_final_root`
  is back under its own name.
- **`MaxWindowRequeues` is a bound that alarms, not a terminal state.** It now carries
  `seen_head_final` and is re-tried (fee-free — the window pre-check refuses locally) once
  `head_final_batch` advances, like `ProofTooLarge`/`ExceedsWindowCap`/`ProofInvalid`; three congested
  windows no longer strand a valid exit until a restart.
- **The follower's own confirmed sends count against the window before the chain can see them.**
  `Outcome::Sent` now carries `{ window_index, units }`; `Follower::sent_units` accumulates them per
  window (pruned to the current and previous window) and `run::poll_once` passes
  `sent_units_in(window)` as the new `AttemptParams::local_spent_units`; `core::attempt_exit`'s window
  pre-check takes `max(chain spent at finalized, local)`. Before this, the second over-cap exit in a
  window still paid the on-chain 68 fee because the `finalized` read lagged the first send by ~32 slots.
- Smaller fixes: `read_root`'s `.unwrap_or(0)` in `poll_once` is gone — its failure skips the poll's attempts and
  counts `rome_zk_exit_rpc_errors_total{method="read_root"}` like the two sibling reads; a message
  returning from `stuck` keeps its `send_attempts`/`window_requeues` (carried on `Stuck`), never a fresh
  budget per Final root; the on-chain `UnsupportedAsset` gate (v1 native-asset only) has a local mirror
  (`Refusal::UnsupportedAsset` → `StuckReason::UnsupportedAsset`, terminal) so a non-native message never
  burns `max_send_attempts` fees; `rome_zk_exit_stuck`'s HELP text names all six reasons and the render
  test asserts every reason label plus the two counters the previous fixes added; `make check-pins` runs in CI's `test`
  job before the Associated Token Program dump.

### Tiber deploy, more fixes: the knob is real end to end, the alert key Grafana honours, no secret in a tracked file, the vault tool's guards pinned

Three further fixes to the change above that could be tested here, plus smaller ones:

- **`noDataState`, not `no_data_state`.** Grafana 11.4.0 silently ignores the snake_case key the earlier fix wrote;
  `/api/v1/provisioning/alert-rules` showed the prover-behind-batches rule at the default `NoData` while a file grep
  passed. The key is fixed and the Grafana alerting test now reads `noDataState` back through the API on both
  boots.
- **The render-env step** assembles the Tiber `.env` from an already-rendered Tiber env file (pure local — the
  push-files step writes the Grafana password and the database connection name, fetched with the cloud CLI, INTO
  that env file, and this step reads them back; two environment variables override them for tests).
  The proof-window test drives the REAL target against a scratch deploy directory and reads the result through
  `docker compose config` — dropping the sidecar carry from the recipe now turns the test red (before, the test
  re-implemented the `cat` itself). The recursive `$(MAKE)` call sits on its own recipe line: GNU make executes
  `$(MAKE)` lines even under `-n`, so sharing a line with the cloud CLI fetches made a `make -n` of the push-files
  step run them. The example Tiber env file is documented as the ONE place to set `VERIFIER_PROOF_WINDOW` (the
  rendered env file is regenerated on every push); `SQL_INSTANCE_CONNECTION_NAME` is now a key in it.
- **No webhook in a tracked file.** The Grafana alerting render script and `contactpoints.yml.template` are gone.
  The committed `contactpoints.yml` carries a `url` read from an environment variable; docker-compose.yml sets that
  variable from the Slack webhook URL in `.env` (carried by the render-env step when the example-env key is filled
  from the secret store) with a documented no-op loopback default when unset. The test boots the committed tree with
  the default and with a fixture URL, and proves the expansion is real by booting with an EMPTY value — the alerting
  provisioner refuses.
- **`zk-bridge-client::vault_tool`.** `plan_fund`/`plan_init_vault`/`execute` + `Mode::from_args` hold
  every decision the `vault` example used to make inline, pinned over fakes: the `VaultSettlementMismatch`
  refusal before ANY instruction is built (the only guard — `programs/zk-bridge::fund` never re-derives
  the address from a caller-supplied settlement program), an existing vault is never
  re-created, dry-run is the DEFAULT, `--confirm` calls `send` exactly once. The example is wire-only
  (RPC read, keypair, real send) and fails an unreachable RPC by name before any key file is read.
- Smaller fixes: the dry-run plan echoes the `PROGRAMS_JSON` it would actually write; `.gitignore` tidy.

### `programs/zk-bridge` — vault PDAs re-keyed by `settlement_program`, closing the `InitVault` front-run at the core

A follow-up check of the chain-authority gate (below) RUN-proved on real BPF that the gate closes
*provenance* — a config naming a given `settlement_program` can only be created by that settlement's real
`root.authority` — but not the front-run itself: `vault_config` was still keyed by `chain_id` alone, one slot
per chain, so an attacker who named their OWN hostile `settlement_program` and supplied a self-consistent
root could still win that slot first and permanently lock the real authority out (`VaultAlreadyInitialized`,
no reinit/close path). The README and `init_vault.rs` module doc overclaimed this as closed; both are
corrected below.

The fix moves `settlement_program` — already stored in `vault_config` (no layout change, `ZKVC` still LEN
110) — into all three vault PDA seeds: `vault_config_pda`/`vault_token_pda`/`vault_authority_pda` are now
`["vault_config"|"vault"|"vault_authority", settlement_program, chain_id, ...]` (`vault_token` also keeps
`mint`). This makes the real chain authority's vault address itself un-frontrunnable: a hostile
`settlement_program` argument only ever creates a config at `pda(hostile_settlement, chain_id)`, never
`pda(real_settlement, chain_id)`, which requires signing as the real settlement's own `root.authority`. In
either race ordering the two calls target different addresses, so the real authority is never locked out.
`Fund`/`ReleaseExit` read `settlement_program` off the `vault_config` they already decode (no new
instruction argument); `ReleaseExit`'s own `vault_config` seeds check necessarily moves to AFTER the account
is read (the seed is only known once the data is decoded), verifying the account is self-consistent with its
own recorded `(settlement_program, chain_id)`.

New real-BPF tests (`programs/zk-bridge/tests/vault.rs`): `attacker_first_does_not_lock_out_real_authority`
(the attacker-first race — the attacker's hostile-settlement `InitVault` still succeeds, but at its OWN
address; the real authority's later `InitVault` also succeeds, at the real address — not locked out) and
`real_vault_address_is_derivable_from_the_real_settlement_only` (the real address is a fixed function of
`(program, real_settlement, chain_id)` and stays unoccupied by an attacker who cannot sign as the real
`root.authority`). Reverting `vault_config_pda` to derive from `chain_id` alone makes
`attacker_first_does_not_lock_out_real_authority` fail: the two configs collide back into one slot and the
lockout returns. Every earlier gate test stays green under the new seeds. `docs/ARCHITECTURE.md`'s
threat-model row for this front-run is corrected to name the keying (not the provenance gate alone) as the
closing mechanism; the fund-time `vault_config.settlement_program`/`authority` verification downgrades from
the sole drain-prevention to defense in depth.

Re-measured CU (real BPF, `mint`/`recipient` still pinned, bit-exact across 3 repeated runs): `InitVault`
39,998 CU, `ReleaseExit` 54,506 CU, `Fund` 7,354 CU — all down from their earlier figures because
promoting `settlement_program` into the seed shifts each PDA's own bump-seed search depth, not because any
instruction does less work. `ConsumeExit`-via-CPI stays 15,794 CU (it derives `zk-settlement`'s own PDA,
untouched by this re-key).

### `programs/zk-bridge` — `InitVault` gated by the settlement chain authority + smaller fixes

A fund-safety check of the change below found `InitVault` permissionless and first-caller-wins:
`vault_config.settlement_program` is the entire root of trust `ReleaseExit` keys off, so whoever won
`InitVault` for a chain/mint decided it. `InitVault` now requires a `chain_authority` signer plus the
settlement program's own `["root", chain_id]` account (read-only): the `root` account must be owned by
`args.settlement_program` and live at that program's own PDA, and `chain_authority` must sign and equal the
decoded `root.authority` (`NotChainAuthority` otherwise) — the identical gate
`zk-settlement::governance::require_chain_authority` applies to `ProposeExitConfig`. `vault_config.authority`
now records that chain authority (previously the caller/payer). `args.mint_decimals` is also checked against
the mint account's own real decimals byte (offset 44) — `MintDecimalsMismatch` on a mismatch, closing an
operator-typo path that would otherwise mis-scale every future `ReleaseExit` payout.

`InitVault`/`ReleaseExit`'s measured CU is now reproducible: the CU-measuring tests previously derived
`vault_token`/`recipient_ata` PDAs from `Pubkey::new_unique()` mint/recipient values, so the bump-seed search
depth (and therefore CU) varied run to run; both are now pinned to
`rome_zk_testkit::fixed_mint_pubkey`/`fixed_recipient_pubkey`, and the figures are bit-exact across repeated
runs: `InitVault` 50,472 CU (up from 46,072 pre-gate — the gate's own account reads/checks cost ~4,400 CU),
`ReleaseExit` 57,480 CU, `ConsumeExit`-via-CPI 15,794 CU, `Fund` 10,352 CU. A new
`release_to_wrong_payer_refund_is_refused` test exercises `ReleaseExit`'s own `WrongPayerRefund` guard
(previously enforced but untested — without the guard the call is still refused via `zk-settlement`'s
own CPI check, but with a generic error instead of the named one, so the test requires the named error).
The program still does not create a recipient's Associated Token Account; the supplied account must already
exist, and this change does not alter that.

New real-BPF tests (`programs/zk-bridge/tests/vault.rs`): `init_vault_by_chain_authority_succeeds`,
`init_vault_by_non_chain_authority_is_refused`, `init_vault_root_not_owned_by_settlement_is_refused`,
`init_vault_with_wrong_mint_decimals_is_refused`; the two pre-existing `InitVault` tests now sign as the
chain authority and assert `vault_config.authority` records it. Every new guard mutation-tested (dropping
the chain-authority-equality check, the root owner/seed check, and the mint_decimals check each turn their
own test red).

### `programs/zk-bridge` — THE VAULT: releases a proved exit to `record.sol_recipient`, real SPL CPI

New program `programs/zk-bridge` (real, deployed program — not a test-only stub) and new client crate
`crates/zk-bridge-client`: three instructions, `InitVault`/`Fund`/`ReleaseExit`. `ReleaseExit` reads a
settlement `exit_record`, CPIs `zk-settlement`'s `ConsumeExit` (closing the record and refunding its rent to
`record.payer`) *before* an SPL transfer of the decimal-scaled amount to `record.sol_recipient`'s Associated
Token Account, both signed by this program's own PDAs (`["exit_consumer", chain_id]` for the settlement CPI,
`["vault_authority", chain_id]` for the transfer) — a keypair can never sign as either, so once a chain's
`exit_config.bridge_program` names this program (a later governance step, not part of this change), only it
can ever release that chain's exits. The recipient is never an instruction argument: it comes from the
record, and the caller-supplied `recipient_ata` account is checked against an independent derivation, not
trusted. Consume-before-transfer is what makes a second `ReleaseExit` against the same record unconstructable
(the record is gone, owned by the system program, before any transfer could repeat). Decimal scaling
(`amount / 10^(18 - mint_decimals)`) rounds down; the remainder simply stays in the vault. Native asset only
(v1).

The zk lane's first `spl-token` CPI — and, because this workspace's exact `solana-program = "=2.1.6"` pin is
incompatible with every current `spl-token`/`spl-token-interface` release's own newer
`solana-pubkey`/`solana-instruction` requirement, the first place this workspace hand-rolls the SPL Token
wire format (`programs/zk-bridge/src/token.rs`) rather than depending on the crate directly — see that
program's README for the full reasoning and the resulting `.so`-build recipe used by tests and CI (a
throwaway, uncommitted manifest resolves `spl-token = "=9.0.0"` for `cargo build-sbf`, independent of this
workspace's own `Cargo.lock`).

Real-BPF tests (`solana-program-test`, `prefer_bpf`, three programs loaded: `zk_bridge` + `zk_settlement` +
the genuine SPL Token program): `init_vault_creates_a_pda_owned_token_account`,
`init_vault_twice_is_refused`, `fund_increases_vault_balance`,
`release_pays_only_record_recipient_and_closes_record`, `release_to_wrong_recipient_ata_is_refused`,
`release_unsupported_asset_refused`, `release_with_wrong_settlement_owner_refused`,
`release_twice_is_refused`, `decimal_scaling_rounds_down_dust_stays_in_vault`. Every fund-safety guard
mutation-tested (recipient from an ix-supplied account, the `ConsumeExit` CPI skipped, rounding up instead
of down, the asset check dropped, the settlement-owner check dropped) — each turns a named test red.
Measured on real BPF: `ReleaseExit` (incl. the `ConsumeExit` CPI + the SPL transfer) 60,005 CU (the
earlier ≤ 45,000 CU budget was an assumption pending this measurement, and the real figure exceeds it — reported,
not rounded down; still well inside the 1.4M per-transaction ceiling); the `ConsumeExit` CPI's own nested
frame (real bridge, superseding the earlier test-stub figure), 15,794 CU.

### `rome-zk-exit-prover` — the off-chain follower that turns `ExitInitiated` into `ProveExit`

New crate `crates/rome-zk-exit-prover` (off-chain bin, `#![forbid(unsafe_code)]`, no `build-sbf`): watches
the L2 exit portal for `ExitInitiated` (`eth_getLogs`), fetches an Ethereum Merkle-Patricia proof of the
message's inclusion against the newest Final batch's `state_root` (`eth_getProof`), verifies that proof
LOCALLY — the identical `rome_zk_mpt::verify_account`/`verify_storage` check `ProveExit` itself performs on
chain — before ever building a transaction, and only then sends `ProveExit` (28) through
`rome-zk-solana-sender`'s V1 `Sender`, the same way `rome-zk-prover` sends `PostRootProved`.

Four named pre-send refusals, none of which ever touch the `Sender`: `StateUnavailableAtRoot{number}` (the
verifier node's own `--rpc.eth-proof-window` refuses the requested block; the message persists in L2
storage and a later poll against a later Final root retries it), `ProofTooLarge{len,max}` (the projected
signed V1 transaction size, measured via a throwaway-keypair shadow build — every `Pubkey`/`Signature` is
a fixed byte length regardless of value, so no real key material is ever needed for this measurement), and
`RootMismatch`/`ExitNotSent` (the local MPT verify). A send is then classified by its on-chain custom error:
`ExitAlreadyProved` (65) advances past the message with zero re-sends; `ExitCapExceeded` (68) enters
`QueuedForWindow{next_index, retry_slot}` and retries at the next challenge window's boundary — **never a
halt**.

`fixtures/exit/fixtures.sh` gained two new outputs (the committed fixtures from before are untouched):
`anvil_exit_logs.json` (`eth_getLogs` of the base scenario's one `ExitInitiated` event) and
`anvil_block_by_number.json` (the raw `eth_getBlockByNumber` response the prover's own decoder reads,
unextracted). CI's `contracts` job drift check covers both (their block-hash-derived fields excluded from the
raw diff, same pattern as the existing `anvil_state_root*.json` files).

Measured: the anvil exit fixture's `ProveExit` signed V1 tx is **1,179 B**; a synthetic 16-account +
8-storage-node proof measures **7,831 B** — already over the 4,096-B V1 envelope, grounding
`--max-proof-bytes`'s default.

Tested entirely against `fixtures/exit/*.json` and in-process fakes for `SettlementReader`, `VerifierRpc`
and `Sender` — no live cluster or verifier node anywhere in this crate's own suite; the real end-to-end
send against a live Tiber Final root is a later step.

**Fixes:** `attempt_exit` now mirrors the third pre-MPT root gate, `root.exit_cap_per_window == 0`
(`ExitCapUnset`, mirrors the on-chain error 64) — governance can activate `exit_config`'s
`exit_portal` without the cap via an independent `pending_mask` bit, so `(portal set, cap unset)` is a
reachable state on any chain; without this gate the prover would fetch and locally verify a genuine
proof, then send a `ProveExit` the chain refuses, wasting a fee. Also, `proof.rs`'s conversion from a raw
`eth_getProof` response now returns a clean `Err` on a malformed proof-node hex string instead of
panicking the follow loop.

### Tiber deploy — exit-portal deploy honours `DEPLOY_RPC_URL` inside the container

- The exit-portal deploy script with `--confirm` now passes `-e
  DEPLOY_RPC_URL` into the foundry container, so
  the exit portal deploy script's `forge`/`cast` reach the RPC the caller chose instead of always hitting the
  container's `http://127.0.0.1:8545` loopback (which only resolves ON the Tiber VM). This is what lets
  the deploy run from a repo checkout against the public sequencer. Found running the real `--confirm`
  live (it failed `Connection refused` before this). The
  exit-portal deploy dry-run test asserts
  the container receives `-e DEPLOY_RPC_URL`.


### Exit-config ops CLI — `propose-exit-config` / `activate-exit-config` / `show-exit-config`

The `governance` example's three new subcommands are the first send path for `ProposeExitConfig` /
`ActivateExitConfig` (the client instruction builders shipped earlier with no tool wired to them until
now). `propose-exit-config` (chain-authority-only) takes the four optional fields
(`--exit-portal`/`--bridge-program`/`--exit-cap`/`--poster-bond`, each omitted -> `None`) and either
`--activation-slot` or `--activation-delay-slots` (-> `current_slot + delay`); it refuses locally, before
ever building a transaction, when all four optional fields are omitted — mirroring the program's own
`InvalidArgument` rejection rather than spending the chain authority's rent to reach it on-chain.
`activate-exit-config` (permissionless) reads the `exit_config` account first on the real-send path and
prints `not yet — N slots to go` (sending nothing) if the activation slot has not been reached, or refuses
if nothing is pending. `show-exit-config` (read-only) prints the *current* portal/bridge from `exit_config`
and the *current* cap/bond from `root` — the account that actually stores those two numbers, per
`root.rs`'s own layout — plus every pending field, the pending mask decoded to names, and an
activatable-now/not-yet reading against the live slot; it prints "no exit config (exits disabled)" when the
account does not exist yet.

Both senders take `--dry-run`: sign against an all-zero placeholder blockhash (never fetched from any RPC — this
mode makes no cluster call of any kind) and print the decoded instruction, its raw discriminant byte, and the
fully-signed transaction as the same base64 bytes a real `sendTransaction` call would receive, instead of sending.
A new shell test for the exit-config CLI dry run (in the Tiber deploy
tests directory) proves `propose-exit-config
--dry-run` carries discriminant 26 and the supplied `--exit-portal`, `activate-exit-config --dry-run` carries
discriminant 27, and an all-None propose refuses by name before reaching a decoded transaction at all — with
throwaway keypairs generated fresh in a tmp dir, never a real key. The test needs a prebuilt `governance` binary
(`GOVERNANCE_BIN=<path>`) and SKIPs by name in the ubuntu-latest `shell-tests` job (compiling this client's
`devnet-driver` feature there would be the first `cargo` invocation of that job and is too heavy for it); it runs
for real on a build machine against a binary built there.

The Tiber deploy README gained "Configure exits (start the activation clock)": the ordered steps from
upgrading the settlement program through deploying the L2 exit portal, proposing, waiting out the
activation delay, and activating — each step with its exact command and expected output.
`--bridge-program` is left for a later cycle, after the bridge program itself is deployed. The
program, the client's instruction builders/decoders (`propose_exit_config_ix`/`activate_exit_config_ix`/
`decode_exit_config_account`), and the layouts are unchanged — only the example CLI and
docs. Nothing in this change sends anything to any cluster by itself.

### `rome-zk-prover` — config, vkey of record, `Prover` trait, calldata decoder

- **Own workspace**, the same pattern as `rome-zk-prover-input`: path-depends on that crate (inheriting its exact
  `alloy` 2.0.5 pins), so it is excluded from the root workspace and built with `cd crates/rome-zk-prover && cargo
  test --locked`. Verified on a build machine that one lockfile resolves and compiles `alloy` 2.0.5 together with
  both the legacy `solana-program`/`solana-client`/`solana-sdk` 2.1.6 generation and the V1-generation Solana crates
  `rome-zk-solana-sender` sends transactions with — the only real conflict in this family is prover-input's `alloy`
  pins vs `rome-zk-batcher`'s wider range, which does not apply here.
- **`Config` + `VkeyOfRecord`**: the vkey-of-record fixture (`fixtures/vkeys/<chain>-layout1.json`) stays the
  one owner of `programVK`/`rootCVadcopFinal`/`elf_sha256`/`chain_id`/`layout_id` — this crate only reads and
  validates it. `Config` derives `#[serde(deny_unknown_fields)]` and gained `Config::load(&Path)`, so a typo'd
  or stale TOML key is a named parse refusal, never a silently-ignored knob. Refuses by name:
  `VkeyJsonMismatch` (a `layout_id` other than 1, or a `chain_id` disagreeing with the running config),
  `ElfMismatch` (the configured ELF file's sha256 disagreeing with the vkey json's `elf_sha256`), and
  `BadHex`/`BadHexLen` (a vkey json hex field that fails to decode, or decodes to the wrong length).
- **`Prover` trait + `LocalCargoZisk`**: a `cargo-zisk prove --plonk` subprocess spawned as the leader of its own
  process group. Verified on a build machine (`cargo-zisk` 1.2.0-alpha): `-o <path>` writes the proof at that exact
  file path, not a directory; `--plonk` runs the STARK step and the SNARK wrap in one invocation; there is no
  `-g`/GPU flag in this version. Parses per-stage walls from the subprocess's own log; refuses
  `ProveFailed`/`ProveTimeout`/`NotVerified`/`OutputMissing` (a verified exit that never actually wrote the output
  file, or wrote it empty)/`Spawn` by name. A timeout kills the whole process group (`killpg`), not only the direct
  child pid, so a helper the tool forked cannot outlive the call; draining the subprocess's own stdout/stderr is
  itself bounded (a channel + `recv_timeout`) as a second line of defense against a helper that escapes the process
  group entirely. Exercised entirely against a fake `cargo-zisk` binary (`tests/fake-cargo-zisk/`, each script
  installed once for the whole test binary rather than once per test) — no real ZisK toolchain runs in the crate's
  own test suite. Dropped `LocalCargoZisk::attempts`: the follower loop owns retries and reads
  `Config::max_prove_attempts` directly.
- **`calldata::from_zisk_proof_file`**: a Rust port of the earlier Python decoder `zisk_calldata.py`
  (hand-rolled bincode-2 varint decode of the ZisK `Proof` file's `Plonk` variant), producing the ABI
  `zk_settlement_client::layout1_proof_abi` assembles plus the derived public signal (via `zisk_public_signal`
  from the proof verifier crate (now `veritas`), never reimplemented). Decodes `fixtures/s10/block14.zisk.bin`
  (now committed here) byte-for-byte against the already-committed `fixtures/s10/block14.calldata.json`, and
  the assembled ABI verifies on the host. Refuses `NotPlonk`, `TrailingOrShort`, `ProgramVkMismatch`,
  `Malformed` (a wrong `proof_bytes` length, an unexpected word count after the flag strip, or a reserved
  varint tag byte) and `BadVadcopFlag` (the vadcop_final flag word, when present, was not `1`) by name.
  **`check_against_record(&Calldata, &VkeyOfRecord)`**: compares a decoded proof's own `program_vk`/`root_c`
  against the vkey of record before any `layout1_proof_abi` is built from it, refusing
  `ProgramVkNotOfRecord`/`RootCNotOfRecord` by name — comparing the vkey of record itself against the
  registry's active entry is a separate check, the poster's `AnchorError::VkeyNotActive`.
- **`publics::to_post_root_fields`**: builds a `PostRootProved` send's arguments from a decoded proof's public
  values and the poster's chain anchor, mirroring `rome-zk-solana-sender/examples/post_root_proved.rs`'s field
  construction. Tested against the first real proof of the reset chain's batch 1, committed as
  `fixtures/prover-input/txv1-dev-reset6-batch-1.{json,bin,plonk.bin}`: that real proof decodes to the
  registered vkey of record and its public values cover batch 1 (blocks 1..=60), matching the sidecar's own
  recorded hashes field by field.
- **Measurement gate** (CI runner, reproducible ELF `zec-rome`, sha256 `ac5bf47f…`, real
  `cargo-zisk prove --plonk` subprocess, CPU): proving `fixtures/prover-input/txv1-dev-batch-3930.bin` (chain
  200101, blocks 39181..=39190, 10 empty blocks) reported `Proof generated in 822.773s, steps: 596486`
  (subprocess own log; ≈14 m21 s wall end to end, process start to the proof file written); the decoded
  proof's `programVK` = `0xe5ea5c144f19aba3e8a72f897dcb565c1b18bc335b54fd93e06b06689c53cb03`, matching the
  vkey of record, and its public values (`chain_id`, `first_number`, `last_number`, `gas_used`, `state_root`,
  `last_block_hash`) matched the committed sidecar field for field — decoded with this crate's own
  `calldata::from_zisk_proof_file`, not just the reference Python tool.
- The follower loop, the poster, metrics, Postgres job history and the chain writes come in the entries below.

### `rome-zk-prover` — poster + anchor + finality + `--once`

- **`rome-zk-prover-input`**: `AccountFetch` gained `get_multiple_accounts` (default: loops
  `get_account`, overridable) and `fetch_and_verify_batch` now reads every finalized batch's chunks
  through it, paged at 100 keys/call — mirroring `rome-zk-derive`'s own shape. A 250-chunk batch
  costs exactly 3 `get_multiple_accounts` calls (`[100, 100, 50]`) and one `get_account` (the batch
  account itself), never one `get_account` per chunk. The CLI's own `RpcFetch` implements the paged
  read with real `getMultipleAccounts`.
- **Structural record-check gate (`calldata`/`abi`)**: `check_against_record`
  now returns a `RecordChecked` newtype instead of `()`; `abi::layout1_from` — the crate's only
  ABI-building entry point — accepts `&RecordChecked` and nothing else, so an unchecked `Calldata`
  cannot reach the ABI at all (a `compile_fail` doctest pins this at compile time, not by convention).
- **`anchor`**: ONE `getMultipleAccounts` call at FINALIZED (the prover's own read client, distinct
  from the sender's CONFIRMED one) for `[root, registry, chain_config, batch_cursor,
  predecessor pending, candidate batch's inbox batch]`, decoded into an `Anchor` snapshot. Refuses by
  name: `HeadAhead`, `VkeyNotActive` (no active, non-retired layout-1 registry entry for the vkey of
  record — missing / retired / future-activation, each with its own reason), `InboxNotFinalizedYet`,
  `AbandonedInboxBatch` (an abandoned inbox id is alarm-and-stop, never a chain write).
  Counting-fake tests pin exactly one `get_multiple_accounts` call per `anchor()`, refusal or not.
- **`poster`**: `build_post_ix` refuses `ContinuityMismatch` then `LocalVerifyFailed` before any network call;
  `preflight_loaded_accounts_data_size_limit` rounds the shared
  `rome_zk_solana_sender::required_loaded_accounts_bytes` formula (SIMD-0186) up to a 32 KiB page;
  `classify_send_failure` names a failed send by the on-chain program's own error (`AlreadyPosted`/`HeadAhead`
  disambiguate `BadBatchSequence` by a re-anchored head; `PayerLow` for a transaction-level failure; else
  `PostFailed{name}` from a pinned `SettleError` discriminant table). Real BPF (`rome-zk-testkit`): the
  committed gate fixture's real proof posts the reset chain's batch 1 end to end — born Final immediately,
  `root.number == 60`, `root.block_hash == pv.last_block_hash`; CU measured ≈ 572–576k (575,461 in an earlier
  measurement, same ballpark); signed V1 tx ≤ 4,096 B. Adversarial, also real BPF: a tampered
  `state_root`/`gas_in_batch` refuses on chain and classifies by name; a second post of the same batch
  classifies `AlreadyPosted`; a predecessor `last_block` off by one and a flipped proof byte both refuse
  before any send (a counting fake `Sender` proves zero calls).
- **`rome-zk-solana-sender`** gained `required_loaded_accounts_bytes` (the shared generic
  loaded-accounts formula — `rome-zk-batcher` keeps its own frame-specific call site
  unchanged) and `build_v1_tx` (exposes the exact V1 builder every real send uses, for measuring a
  transaction's wire shape without sending it). **`examples/post_root_proved.rs` is deleted** — its
  logic (build the fields, assemble the ABI, send via this crate's own sender) now lives in the
  prover's own `poster` module, exercised by that module's real-BPF tests instead of a standalone
  example.
- **`finality`**: `plan_finalize` (the in-order batch plus every already-Final successor up to the
  walk cap and `head_pending_batch`) and `should_close` (the retention gate — `None` on Tiber by
  default, never closes; `Some(k)` closes strictly behind `head_pending_batch - k`) are pure planning
  functions. Real BPF: a sweep over three out-of-order-Final pending PDAs advances `head_final_batch`
  from 0 to 3 in one `FinalizeBatch(1, walk=[2,3])` call (~12.5–17k CU); `RootView(1)` still succeeds
  with no retention; with `Some(1)`, only batch 1 is closed and `RootView(1)` is then refused,
  `RootView(2)` still succeeds.
- **CLI (`bin/rome-zk-prover.rs`)**: `rome-zk-prover --config prover.toml --once --batch N
  [--dry-run]` drives anchor → input (via `rome-zk-prover-input` as a lib) → `Prover::prove` →
  calldata → local verify → post → finalize sweep against a real chain. `--dry-run` stops after the
  local verify and prints the fields, the signed V1 tx's serialized size, and a read-only
  `simulateTransaction` result — never a broadcast.
- Not yet built: the follower loop (every finalized batch in order, resuming from the chain on
  restart), metrics, Postgres job history.

### `rome-zk-prover` — live loaded-accounts preflight, re-anchor before post, classified send

- **`anchor`**: the one `getMultipleAccounts` call now also fetches `global_config`, the settlement
  program's own account, and its `ProgramData` (address cross-checked against the program account's
  own embedded loader-v3 state, not just derived — a mismatch refuses `SettlementProgramDataAddressMismatch`).
  `Anchor` gained `global_config` (so the CLI's own treasury read needs no second round trip) and
  `account_data_lens: Vec<usize>` — every account `post_root_proved_ix` will load, in that function's
  own order, plus the program and `ProgramData` — the live input to the loaded-accounts preflight.
- **`poster`**: `PostParams` no longer carries a separately-supplied `pv` — `build_post_ix` derives it
  from the checked proof itself (`derive_public_values`) and refuses `ChainMismatch` if the proof's own
  `chain_id` disagrees with the anchor's, before any send. `preflight_loaded_accounts_data_size_limit`
  is now actually wired into the CLI (it had no production caller before this change); the new
  `resolve_loaded_accounts_data_size_limit` refuses a configured limit below the derived requirement
  by name (`LoadedAccountsLimitTooLow`) — right after the first anchor and again before the send — rather
  than silently raising or sending it anyway; `system_program`'s live length is read from the snapshot
  and a signer that is not the root's authority is refused locally (`NotTheAuthority`) — the 256 KiB
  literal is gone. `classify_send_failure`'s `BadBatchSequence` handling now distinguishes three cases
  instead of two: `head == batch` is `AlreadyPosted`, `head > batch` is `HeadAhead` (someone else moved
  ahead), and `head < batch` is the new `StaleAnchor` (the re-anchor's own FINALIZED read is stale —
  this case used to be misclassified `HeadAhead`).
- **`rome-zk-solana-sender`**: `send_and_confirm`'s single-transaction path used to treat a landed-but-
  FAILED transaction as a success — `confirm_transaction_with_commitment` only reports whether a
  signature reached a commitment level, not whether it executed without error. It now polls
  `get_signature_statuses` and returns `SenderError::StepFailed` on an execution error, exactly like
  the batched `send_and_confirm_many` path already did. New `sender_error_transaction_error(&SenderError)
  -> Option<&TransactionError>` extracts the on-chain error from whichever variant carries it (`StepFailed`
  directly, or `Rpc` wrapping a preflight rejection), re-exporting `TransactionError`/`InstructionError`
  so a caller can match on them without a second, independently-pinned dependency.
- **`finality`**: `should_close` gained a `status: u8` parameter — a batch is only ever proposed for
  close when its own pending status (read from the same snapshot the caller already has) is `Final`; a
  batch left `Pending` by a challenge-window post is never proposed, since the program would refuse it.
  The finalize walk's own best-effort-stop behavior (a non-Final successor halts the walk there, without
  failing the whole transaction) is now pinned by a dedicated real-BPF test.
- **`rome-zk-batcher`**: `loaded_accounts.rs`'s own 64-bytes-per-loaded-account term now reads
  `rome_zk_solana_sender::LOADED_ACCOUNT_BASE_BYTES` instead of its own local constant — one owner for
  the SIMD-0186 base term across both crates; the batcher's own frame-specific cushion terms are
  unchanged.
- **CLI (`bin/rome-zk-prover.rs`)**: after `prove()` returns (minutes later), the CLI re-`anchor()`s
  for the same candidate batch before building or sending anything — a stale `HeadAhead` refusal here
  exits by name with zero sends attempted. A failed send is classified via one fresh root re-read
  (never a full re-anchor) and `classify_send_failure`; `AlreadyPosted` exits 0. A new retention-gated
  close step runs after the finalize sweep, reading the candidate batch's own pending status before
  ever building `ClosePending` for it.

### `rome-zk-prover` — the always-on follower loop, preemption-safe resume, metrics

- **`follower` module**: the state machine that proves every finalized inbox batch in order, reusing
  `anchor`/`poster`/`finality`'s own steps rather than copying them. `run_one(fetch, prover, sender,
  verifier, cfg, candidate_batch, metrics)` runs the whole pipeline once — `--once`'s entire job and
  the loop's own per-iteration call — split into `prepare_checked_proof` (anchor, resume-or-rebuild,
  prove, decode, record-check) and `post_checked` (re-anchor, build + send, finalize sweep,
  retention-gated close), so a caller can stop after the first phase without ever sending (the
  `--follow --dry-run` live gate). `run(deps, cfg, until, stop, metrics, sleep)` is the loop: each
  iteration tries its own best guess at the next candidate batch, self-correcting from
  `HeadAhead`/`InboxNotFinalizedYet` rather than needing external bookkeeping, sleeping on
  `Idle`/`Retry`, halting on `AbandonedInboxBatch` (no skip instruction exists yet — alarm
  and stop) or a `VerifierBehind` streak that never clears after `verifier_behind_alarm_polls`
  consecutive retries. A re-anchor showing `HeadAhead` classifies `AlreadyPosted` (exactly our batch)
  or `Superseded` (the chain moved further) — used both at the top of a job (a restart after
  `Relayed`) and after proving (the chain moved on while proving).
- **Preemption-safe resume**: every job's artefacts live under `work_dir/<batch>/{input.bin,
  sidecar.json, proof}`. Entering a job, `resumable` allows skipping straight to `Decoded` only when
  `input.bin` AND `proof` both exist as real files AND `sidecar.json`'s own recorded `elf_sha256`
  equals the vkey of record's; any other case wipes the directory and rebuilds from scratch. A
  restart after `Relayed` needs no special case — the next `anchor()` for that batch id already sees
  `HeadAhead`, classified `AlreadyPosted` before the work directory is ever touched.
- **`metrics` module**: a `prometheus::Registry` with these names — `rome_zk_prover_batches_behind`,
  `rome_zk_prover_head{kind}`, `rome_zk_prover_lag_seconds`, `rome_zk_prover_stage_wall_seconds{stage}` (a
  histogram over input/stark/plonk/verify/post/finalize), `rome_zk_prover_batch_gas_used`,
  `rome_zk_prover_batch_cost_usd` (`gpu_hourly_usd × wall seconds / 3600`, reporting only),
  `rome_zk_prover_posts_total{result}`, `rome_zk_prover_prove_attempts_total`,
  `rome_zk_prover_payer_lamports`, `rome_zk_prover_state{state}`, and `rome_zk_prover_abandoned_batch_alarm` —
  served on `/metrics` by `rome-zk-metrics-http`, the same wiring `rome-zk-batcher`/`rome-zk-sequencer`
  already use.
- **CLI**: `--once --batch N` now calls `follower::run_one` directly (the CLI's own inline once-path,
  including its `reanchor_before_post` helper and its decorative test, is gone; both entry points share one
  implementation). New `--follow [--iterations N]` runs the loop until SIGTERM/SIGINT (graceful — the current
  stage's send always finishes) or the iteration bound; `--follow --dry-run --iterations N` is the read-only
  live gate, printing the anchor/`batches_behind`/would-prove job every iteration without ever calling
  `Prover::prove`.
- **`anchor::AnchorError::InboxNotFinalizedYet`** now carries `head_pending_batch`/
  `cursor_next_batch` from the same snapshot, so a caller can compute `batches_behind` and distinguish
  genuinely idle (nothing opened yet) from busy (a batch is open but not finalized) without a second
  read.
- **`rome-zk-prover-input`**: `build_batch_input` takes a `VerifierFetch` seam
  (`head`/`header`/`block`/`witness`) instead of a bare RPC URL string; `RemoteVerifier` wraps the
  existing free functions unchanged for every production caller. New `fetch_block_number`
  (`eth_blockNumber`) for the follower's own verifier-head wait. This is what lets the follower's own
  tests drive the whole input-building step against a fake, never a live reth node.
- Fake-driven test suite (`follower`'s own tests, no live cluster, no live reth node): batches 1..5
  proved/posted/finalized in order then idle; an abandoned id halting after the ones before it with
  zero further sends; idle making zero prove calls over ten iterations (mutation-tested); `HeadAhead`
  between verify and post classifying `Superseded` with zero sends for that job; `VerifierBehind`
  retried then proceeding once the head advances, and alarming after the configured consecutive-poll
  count; the `batches_behind` gauge matching a scripted scenario; the resume rule across a restart
  before posting, after relaying, and on an ELF sha mismatch (mutation-tested).

### `rome-zk-prover` — bind a proof to its inbox batch; the resume gate re-proves; StaleAnchor retries

- **A proof is bound to the inbox batch it is posted against, locally, before any send.**
  `poster::build_post_ix` now refuses `InboxCommitmentMismatch` (the proof's own packaged
  `inbox_commitment` disagrees with the candidate batch's real inbox account) and `OpenTsMismatch`
  (same, for `open_unix_ts`) — the local mirror of `settle.rs`'s own `AccMismatch`/`OpenTsMismatch`
  refusals, zero fee. Before this fix, a cached artefact carried over a chain reset (same `work_dir`,
  batch ids restarting at 1 against a new inbox) or a hand `--once` against another chain would be
  sent and only refused on chain, after the fee.
- **The resume gate re-proves, it never halts.** The on-disk sidecar is now the full
  `rome-zk-prover-input::build::Sidecar` (expected publics + provenance), not an ELF-sha-only marker.
  A cached proof is trusted only when the sidecar's own `provenance.elf_sha256` matches the vkey of
  record AND its `chain_id`/`inbox_commitment`/`open_unix_ts` match the FRESH anchor's own inbox batch
  AND the proof itself decodes, checks against the record, and reproduces the sidecar's own recorded
  publics. Any failure — a corrupt cached proof, a sidecar bound to a different chain reset — wipes the
  work directory and proves again, bounded by `Config::max_prove_attempts` (now actually read); only a
  run that exhausts every attempt halts, with `ProveAttemptsExhausted { attempts }`. Before this fix, a
  kill between the sidecar being written and `prove()` finishing left a corrupt proof file that
  `resumable` (checking only file existence) treated as real — the loop halted with a decode error on
  every restart, the very preemption the resume rule exists to survive.
- **`StaleAnchor` is a retry, not a halt.** `post_checked` classifies a `StaleAnchor` send failure as
  `Outcome::Retry` for the same candidate — the classifier's own contract (`PostRefusal::StaleAnchor`'s
  doc) always said "retry the re-anchor", since a FINALIZED read can genuinely lag a send that already
  landed. A bounded streak (`stale_anchor_alarm_polls`, default 10) that never clears
  halts with `StaleAnchorAlarm { polls }`.
- **Transient snapshot-fetch failures retry, never panic.** `SnapshotFetch::get_multiple_accounts` now
  returns `Result<_, anchor::FetchError>`; a real RPC failure surfaces as a named error the follower
  retries the same candidate on, rather than the old `.expect(...)` panic.
- **Metrics**: `batch_gas_used`/`lag_seconds` are written at `Relayed` from the posted proof's own
  packaged public values (no extra fetch); `set_state("failed")` on every halting `Err` the loop
  returns; `payer_lamports`/a new `rome_zk_prover_stale_anchor_alarm` counter. `metrics_addr` now
  defaults to `127.0.0.1:9003` (a devnet compose overrides it to `0.0.0.0` for its scrape sidecar).
- **`rome-zk-prover-input`**: `parse_block_number` factored out of `fetch_block_number` and unit-tested
  directly (good and bad input), rather than duplicating its parse logic inline in a test beside it.
- Fake-driven test suite additions: a real-BPF gate-rig test tampering the inbox batch's own `acc`/
  `open_unix_ts` (refused locally, zero sends); a corrupt resumed proof re-proved and posted once; a
  sidecar bound to another chain's inbox commitment wiped and re-proved; `ProveAttemptsExhausted`
  after the configured bound; a `StaleAnchor` retry converging to `AlreadyPosted` with zero further
  sends, and a streak that never clears alarming after the configured poll count; a transient fetch
  error retrying rather than halting or panicking; a genuine failed send halting by name; the idle path
  making exactly one snapshot read per iteration; `close_pending_after_batches = Some(k)` closing the
  batch strictly behind the threshold end to end through the loop.

### `rome-zk-prover` — the resume gate verifies the pairing; honest per-batch fake identity; StaleAnchor never re-sends into a lagging read; every RPC read is a Result

- **The resume gate — and a freshly produced proof — now verify the real BN254 pairing, not merely the
  decode/record/publics match.** `try_resume` and the fresh-prove retry path both call the SAME shared
  `poster::verify_checked(&RecordChecked, &VkeyOfRecord)` `build_post_ix` already ran before every send.
  A proof whose `proof_bytes` were corrupted after the fact (a torn write, a bit flip) still decodes
  clean and still matches the vkey of record's `program_vk`/`root_c`, but no longer satisfies the
  pairing — before this fix, a resumed proof in that shape halted `PreSend(LocalVerifyFailed)` on
  every restart (the very preemption the resume rule exists to survive); now it is wiped and proved
  again, bounded the same way every other resume failure already was. A freshly produced proof that
  fails only the pairing is retry material counted against `max_prove_attempts`, never an unbounded
  halt.
- **Honest fakes.** The follower's own test suite now serves the REAL reset-chain batch-1 chunk bytes
  (decoded straight from the committed `.bin` fixture) for every synthetic batch's DA content, instead
  of a synthetic single-block stand-in — the DA-derived `first_number`/`last_number`/`gas_used` agree
  with the one real reused proof's own packaged publics by construction, so the two sidecar-overwrite
  steps that used to paper over that disagreement are gone; the sidecar is now written once, before
  `prove` runs, from the DA-derived expectation directly. At batch id 1 specifically (the only id where
  reusing one real proof across an arbitrary number of synthetic ids is even mathematically honest —
  the on-chain accumulator binds the batch id itself), the fake's `AccountFetch` view recomputes `acc`
  from that real content and its `SnapshotFetch` view serves the same value; at synthetic ids ≥ 2 the
  snapshot view keeps serving batch 1's `acc` by necessity (one real proof cannot bind another id),
  which is why the `acc`-alone and `chain_id`-alone comparisons in the resume gate are pinned by a
  hand-built anchor batch account rather than by the fake; a new `set_inbox_identity` override lets a test change a batch's `open_unix_ts`
  under a cached proof — a chain reset in miniature.
- **The resume gate's `acc` and `chain_id` comparisons are pinned on their own**
  (`a_fresh_anchors_changed_acc_alone_refuses_the_cached_artefact`): a hand-built anchor batch account
  whose `acc` (then the running `chain_id`) alone differs, while sidecar and proof agree in every field,
  refuses the cached artefact — dropping either comparison is red.
- **The fake chain's lagging read is one consistent older slot:** under `stale_reads_left` the root's
  number/state_root/block_hash and the pending table come from before the send that landed, as a real
  FINALIZED `getMultipleAccounts` at an older slot would — so dropping the no-resend guard now shows the
  fee-bearing second send it exists to prevent (`got Posted`, `send_calls` 2), not a continuity mismatch
  the producer could never return.
- **Transport-level reth-verifier failures retry** (`VerifierError::is_transient`: the connection, a
  timeout, a torn response body) as `TransientFetchError` under the same `fetch_alarm_polls` bound; a
  JSON-RPC error, a decode failure and a missing block remain halts by name.
- `LocalCargoZisk::prove` deleting a partial `-o` file is now pinned on the failed-exit and
  never-verified paths too, not only on timeout.
- **`StaleAnchor` never resends into a lagging read.** A send that lands for real but is refused by a
  FINALIZED read that has not yet caught up to it is remembered per candidate
  (`RunConfig::stale_send_pending`); the next retry does not resend — it waits for a fresh re-anchor to
  confirm the head has actually moved. Before this fix, every retry rebuilt and resent unconditionally
  (`skip_preflight: true` executes on chain and pays the fee regardless), so a multi-poll-interval lag
  could cost several real fees for a single batch.
- **Every RPC read is a `Result`.** `rome_zk_prover_input::inbox::AccountFetch::{get_account,
  get_multiple_accounts}` now return `Result<_, FetchError>` — a genuinely missing account stays
  `Ok(None)`, a transport failure (rate limit, dropped connection, timeout) is a distinct, named error,
  never a panic and never conflated with the account simply not existing. `fetch_and_verify_batch`
  propagates it as `InboxError::Fetch`; the follower retries the same candidate on a transient failure
  wherever it can occur (the chunk/batch-account read, the post-time re-anchor, the finalize/close
  sweep — there, the job's own successful post still stands, only the sweep is deferred to the next
  one), with a bounded `TransientFetchError` streak (`fetch_alarm_polls`, default 30) alarming and
  halting with `FetchAlarm { polls }` if it never clears. The CLI's own RPC-backed `AccountFetch` no
  longer has a `panic!` on a paged `getMultipleAccounts` failure or a bare `.ok()` collapsing a real RPC
  error into "account missing".
- **`payer_lamports` is written.** Sampled every follower-loop iteration via a plain `getBalance` on the
  fee payer — reporting only, deliberately a separate read from the one FINALIZED decision snapshot (a
  stale balance changes no refusal this crate makes) — instead of being registered but never written.
- **`batches_behind` is one shared function.** `cursor_next_batch - 1 - head_pending_batch`, computed
  once and used by both call sites in the follower and by the CLI's `--follow --dry-run` live gate,
  replacing three independent copies of the same subtraction.
- **`work_dir/<batch>` retention.** Removed once a batch reaches a terminal outcome
  (`Posted`/`AlreadyPosted`), keeping the most recent `keep_work_dirs` (default 0) directories — an
  always-on follower proving continuously otherwise keeps every batch's artefacts on disk forever.
- Fake-driven test suite additions: a resumed proof that decodes and checks clean but fails only the
  pairing is wiped and re-proved then posts once; a fresh proof that always fails only the pairing
  exhausts `max_prove_attempts` with zero sends; the fresh anchor's own identity changing under a
  cached proof (not merely a hand-edited sidecar file) wipes and re-proves; a stale extra file plus a
  stale sidecar leave the rebuild holding exactly the three expected files; a `StaleAnchor` streak of
  several consecutive lagging reads never resends, converging to `AlreadyPosted`; a transient fetch
  error on the chunk-page read, the post-time re-anchor, or a full snapshot read each retries rather
  than halting or panicking; a fetch-error streak that never clears alarms after the configured poll
  count; the payer-lamports gauge and the `batch_gas_used`/`lag_seconds` gauges are independently
  distinguishable; every posted batch's own `work_dir` is gone once the run finishes.

### The prover host as code — no cloud mutation

- **The infra script**: cloud provisioning (VM, keys disk, service account + per-secret role bindings, internal
  firewall), `create|grant|start|stop|destroy`, parameterised by `PROVER_ENV=soak|production` — soak requests a
  preemptible machine with an explicit termination action; production requests a persistent one with none. Every
  `create` is describe-guarded; `--dry-run` makes no network call and prints the exact cloud CLI invocation a real
  run would make (one shared argument builder, so a flag regression shows up in the dry-run output too).
- **`docker-compose.yml` + `Dockerfile`**: five services — `reth-verifier` + `derive` (copied verbatim
  from the settlement compose: this box runs its own, independent derivation from the Solana inbox,
  never a tunnel into the sequencer's own verifier), `prover` (the always-on core, `--gpus all`),
  the database proxy, `node-exporter`. The prover crate builds in its own Dockerfile stage (it is its
  own cargo workspace, so the sequencer/batcher/derive image's single build invocation cannot include
  it); the ZisK toolchain binaries live on the same keys disk the bootstrap script populates the
  proving keys onto (attached read-write for bootstrap, read-only once populated — see the follow-up
  entry below), not baked into the image.
- **`prover.toml.template` + the config render script**: renders every config field with no code default from
  `programs.json` + `prover.env` + the vkey-of-record fixture's own ELF hash — no literal chain or program id in the
  committed template — refusing by name on any missing input.
- **The guest ELF fetch script**: pulls the guest ELF by the sha
  named in the vkey-of-record fixture (never a
  literal), refuses `ElfMismatch` and removes the mismatched bytes on a hash disagreement.
- **`keys.sha256` + the keys verify script**: one pinned aggregate hash per proving-key directory; a placeholder
  value stands in for the real hashes until a real box populates them (none exists in this environment yet) and is
  never treated as a match — only an explicit re-hash can replace it.
- **The bootstrap script**: the VM startup script — driver check, ZisK toolchain + keys (skipped entirely when the
  disk already verifies clean), OpenMPI, Node + snarkjs, docker + the GPU container runtime; idempotent across
  restarts.
- **The prover cleanup script / the GPU textfile script**: hourly
  hygiene scoped to the work directory only (never the
  keys); an `nvidia-smi` → node-exporter textfile collector.
- **The prover health check**: ten PASS/FAIL
  operator health-check items.
- **Makefile + the registry-authority rotation script**: `prover-infra
  prover-render-config prover-push-files prover-up prover-down
  prover-start prover-stop prover-check prover-ssh prover-logs prover-destroy prover-split-authority` — the last
  wraps the registry-authority rotation that must happen before any key reaches a prover host, refusing by name when
  the new key's secret does not exist rather than creating it. `prover-up`/`prover-down` never touch the sequencer
  stand's own targets.
- **The Tiber health check**: its "batches advancing" item now
  reports its own retirement (an idle chain posting
  nothing is healthy, not a failure) rather than a check that would fail forever; two new items read the prover's
  own lag metrics, reporting SKIP rather than FAIL while no prover host exists. The Tiber `prometheus.yml` is now
  rendered from a template with the prover's scrape target filled in (a placeholder host until a box exists, never a
  literal IP).
- Every fail-closed guard above ships with a shell test exercised against fixtures and stub tooling — no cloud
  credential, no network call beyond localhost, no GPU. **No cloud resource of any kind is created, modified, or
  destroyed by this change** — every infra-script/Makefile real-infrastructure path is covered here only by tests
  that stub the cloud; none of it runs against real infrastructure in this change.

### The prover host made deployable as coded — the pipes, ports and bootstrap branches above were still gaps

The section above had several places where the code did not yet do what its own comments claimed: `prover-up`
rendered no config and fetched no secret; only `127.0.0.1:8547` was published, so the metrics ports the
observability section already documented were unreachable; the bootstrap script set an env var `ziskup` never reads
and treated a real key mismatch the same as "not populated yet"; and a fresh checkout's Tiber deploy could
bind-mount a missing `prometheus.yml` as an empty directory. This change is the fix, with a shell test pinning every
guard below and no cloud resource created or changed (verified: the instances/disks/service-accounts matching
`prover` are still empty, the ELF bucket is still 404, and the project's secrets are still only the ones from
before).

- **`prover-up` now does the two secret-store pipes**: the Postgres password into `prover.env` before rendering (so
  the real password reaches `prover.toml` only through the rendered, 0600 config file, never typed in by hand), and
  the payer key straight onto the prover host (`install -m 0400 -o 999`) — the same pattern the Tiber settlement
  stand already uses for its own payer key. `prover-push-files` renders `.env` (`SQL_INSTANCE_CONNECTION_NAME` via
  the cloud CLI's instance describe call, `BLOCK_GAS_LIMIT` from Tiber's own `.block-gas-limit.env`) and
  `derive.toml` (a new template, rendered by the same config render script that already renders `prover.toml`),
  refuses by name when `genesis.json`/`vkey.json` are absent, and refuses to overwrite an already-pinned
  `keys.sha256` on the prover host with a still-unpinned committed one (the keys-manifest push guard).
- **The infra script**: service-account creation now runs before the VM that references it (the reverse order could
  never succeed on a genuinely fresh project); `attach-keys --rw|--ro` and `detach-keys` are their own explicit
  subcommands (`create` itself never attaches the keys disk — the populate flow is attach read-write, bootstrap,
  detach, attach read-only); `destroy --with-keys` now actually deletes the keys disk when passed; the VM's own
  `--metadata-from-file` startup script is the bootstrap script, so a fresh boot runs it automatically.
- **Compose**: `prover`'s `:9003` and `node-exporter`'s `:9100` are published on the prover host's own interface
  (never merely `expose`d, never the public firewall — only the internal Tiber-to-prover firewall rule reaches
  them); `ZISK_HOME=/opt/zisk` is set in the prover service's own environment (the variable `cargo-zisk` itself, not
  `ziskup`, reads to find its keys); a writable `zisk-cache` volume sits at `/opt/zisk/cache`, layered on top of the
  read-only keys mount, since `cargo-zisk` writes its own witness/proof cache there at prove time.
- **The bootstrap and keys verify scripts**: `ziskup` is invoked with `--prefix "$ZISK_HOME"` — the flag that
  actually selects its install directory; the `ZISK_HOME` environment variable it exports is never read back by it,
  so setting only the env var silently installed into the wrong home. The keys verify script now reports a distinct
  reason per case (not yet populated / populated-but-unpinned / a real mismatch / clean), and the bootstrap script
  branches on it: only "not yet populated" runs `ziskup`; a real mismatch is refused outright, never re-populated or
  re-pinned over.
- **The prover cleanup script** refuses by name unless
  `/etc/rome-zk-prover-box` exists — a marker the bootstrap script
  creates once real bootstrap completes — so this hourly cron (`docker ... prune`, `rm -rf` under a configurable
  work directory) can never run against an operator's laptop or a shared CI machine by accident.
- **The guest ELF fetch script** re-hashes a file already at the sha-named
  destination before trusting it (a match skips the
  fetch; a mismatch moves the stale bytes aside and re-fetches) instead of
  trusting presence alone; the prover health check's ELF
  item now re-hashes the ELF on the prover host the same way, rather than only checking it exists.
- **The Tiber Prometheus render script** factors the prometheus render out of the settlement config render script so
  the push-files step calls it too — a fresh checkout that has never brought up the settlement stand now always has
  a real `prometheus.yml` for the deploy step's bind mount, instead of Docker creating an empty directory at that
  path.
- New shell tests (in the prover deploy tests directory): compose structure, bootstrap branches,
  cleanup guard, infrastructure confirmation with stubs,
  health-check items, file push,
  keys-manifest push guard. CI gained a `shell-tests` job running every shell test in the
  Tiber and prover deploy tests directories on every push.

### Prover deploy filesystem contract, keys-disk provisioning, and a box-simulation gate

The prover host's own filesystem never had a single written contract, and several scripts assumed a layout nothing
actually produced: the keys disk was never formatted or mounted by anything in the prover deploy directory; the
guest ELF's configured path lived under the read-only keys mount, which nothing ever populated; the Postgres
password reached the prover host in clear inside two different files; and the host's own metadata startup-script ran
from a directory with no sibling files, so it always failed before installing anything. This change is the fix, plus
a new gate (the prover host simulation, in the prover deploy tests directory)
that builds the real image and runs it with the real
mounts as uid 999 — it found a real bug in its own first run (below) before this change ever merged.

- **README gains "What lives where on the prover host"**: a path/owner/mode/rw-ro/writer/reader table that every
  script's own paths, owners and modes now derive from.
- **The bootstrap script now owns the keys disk end to end**: refuses `KeysDiskAbsent` unless the named block device
  exists; formats it with `mkfs.ext4` exactly once (`blkid`-guarded) and mounts it at `$ZISK_HOME`, adding an
  `/etc/fstab` entry so a reboot remounts it without reformatting; pre-creates `$ZISK_HOME/cache` (999-owned,
  unconditionally — a fresh box-sim run found that gating this on "this boot mounted the disk" left it missing
  whenever the disk was already mounted but keys were populated for the first time in the same run, which then made
  a real `docker run` fail at mountpoint creation); best-effort remounts read-only once keys verify; installs
  `/etc/cron.d/rome-zk-prover` (hourly prune, per-minute GPU textfile) after writing its own marker; refuses
  `NotInstalledUnderOptProver` unless it is actually running from `/opt/prover`. It is **no longer a GCE metadata
  startup-script** — that runs from a temporary copy with no sibling files, so the keys verify script and everything
  else it calls alongside itself was unreachable. `make prover-bootstrap` (push the tree, then run it over tunnelled
  ssh) is the new explicit delivery path; the infra script's `create` no longer passes a startup script.
- **The guest ELF's home moves to `/data/elf`**: `elf_path` never pointed anywhere the prover host actually
  populated (the read-only keys disk). The guest ELF fetch script now
  runs locally (`make prover-fetch-elf`, wired into
  `prover-push-files` before the tar — the prover host's own service account has no storage role) and the fetched
  file is pushed into `/opt/prover/elf`, bind-mounted `:ro` into the container at `/data/elf`
  (`docker-compose.yml`'s own new volume) — `prover.toml.template`'s `elf_path` renders to match.
- **`prover.env` and the rendered `prover.toml` (both carry the Postgres password in clear) are excluded from the
  tar**: `prover-up` installs the rendered `prover.toml` straight over stdin (mode 0400, uid/gid 999, the payer
  key's own pattern) and chowns/chmods `jwt.hex` on the prover host the way the Tiber settlement stand already does
  for its own; the config render script chmods its local render 0600
  regardless of the caller's umask. The prover health check
  reads only the six keys it needs from its env file by name instead of sourcing the whole thing.
- **The keys-manifest push guard fails closed on a transport failure**: a failed remote read (auth, network, quota)
  now refuses `KeysManifestUnreadable` instead of being treated the same as "no manifest yet, first push" — the two
  cases previously looked identical to this script.
- **The prover host simulation test** in the prover deploy tests directory
  (`make prover-box-sim`; needs a real docker daemon and
  passwordless sudo — SKIPs by name otherwise, and never runs by default in the shell-tests job): runs the bootstrap
  script and the render/fetch/install steps against a scratch root, builds the real prover image, and runs it as uid
  999 with the real mounts (the writable cache volume, the rendered `prover.toml`, the ELF directory, `jwt.hex`) —
  plus three negative controls, each reproducing a real bug: no `cache/` directory fails a real `docker run` at
  mountpoint creation; `jwt.hex` owned by another uid is unreadable to uid 999; `prover.env` landing in the pushed
  tree is caught by the same assertion that proves it does not.
- **A one-shot `keys-verify` compose service** runs the keys verify script against the keys mount before `prover`
  starts (`depends_on: … condition: service_completed_successfully`) — "verify before every start" (previously only
  bootstrap time), so a keys disk that drifted between boots still refuses.
- **Minor fixes**: the infra script's `create` called `create_service_account` directly, then again via `grant_all`,
  printing a duplicate line under `--dry-run` — collapsed to one attempt. Doc/comment drift fixed (the config render
  script's and `prover.env.example`'s own messages named the wrong Make target for who pipes `PG_PASSWORD`; the
  database proxy's comment claimed a `--credentials-file` flag the command never passes — it authenticates via the
  VM's own service account instead). The prover health check's later items
  (5 to 10) now SKIP alongside the first four when no
  instance exists, instead of reporting a confusing FAIL for a box that was never created. A secret-scan configuration
  allowlist row that never matched anything is dropped.

### Prover deploy — the simulation now runs what the prover host runs, and the image itself works

The simulation gate still stopped short of the prover host's own image: it hand-copied scripts into place instead of
pushing them with the real recipe, and it never ran the keys verify script inside the built image at all — so nobody
had noticed the runtime image carries no `shasum`, and the compose-level `keys-verify` one-shot would exit 127 on a
real prover host before ever reporting a real reason, leaving the prover container stuck `created` forever. The fix
is verified against the real image on a build machine (`ROME_ZK_BOX_SIM=1 make prover-box-sim`), with no cloud
resource created or changed.

- **The keys verify script resolves `sha256sum`/`shasum` through one helper** instead of hardcoding `shasum` (an
  array, not a shell function — `find -exec` cannot invoke one): the runtime image has coreutils' `sha256sum` and no
  perl; macOS has the reverse. Both call sites use it.
- **The prover container's own ENTRYPOINT re-verifies the keys before every start** (`docker-entrypoint.sh`,
  bind-mounting the same keys verify script and `keys.sha256` the compose-level one-shot already uses) — a daemon
  restart under `restart: unless-stopped` now re-verifies too, not only the one-shot at `up` time.
- **`prover-push-files`'s remote extract runs `--no-same-owner --no-same-permissions`** and
  re-asserts jwt.hex (0400 999:999), the ELF (0644) and every script (0755) right after — GNU tar as
  root otherwise preserves the operator's own local mode/owner on every extracted file, silently
  undoing `prover-up`'s own install the next time someone runs a bare push with nothing after it.
  The guest ELF fetch script now chmods the fetched ELF 0644 (`mktemp`
  otherwise leaves it 0600, unreadable to the
  container's own uid 999); the prover health check's sixth item, the
  re-hash, now reads `/opt/prover/elf/`, where the ELF is
  actually pushed (nothing ever populated an `elf/` directory under the read-only keys disk).
- **`prover-bootstrap` no longer depends on `prover-push-files`**: a new, lighter `prover-push-scripts`
  (scripts, `keys.sha256`, `docker-compose.yml` — no `genesis.json`/`vkey.json`/`prover.env`
  prerequisite) breaks a circular bring-up order where bootstrap — the very thing that installs
  docker — could never run on a fresh tree because its own dependency chain refused first for want of
  files bootstrap never needed.
- **The prover host simulation test runs the real Makefile recipes**, not
  a hand-replayed approximation: a stub cloud CLI
  executes every `compute ssh --command=<cmd>` locally under sudo (`PROVER_REMOTE_DIR` rewritten to a
  scratch root as a `make` argument — an environment variable of the same name does not override the
  Makefile's own assignment) with secret-store lookups answered as fixtures, so the real tar pipe, the real
  `install -m 0400 -o 999 -g 999`, and the real post-extract ownership fixups all run for real. Every
  filesystem-contract row is asserted by owner:group and mode after the install, not merely for
  existence, and a second bare push proves ownership survives it. A real `docker compose up` of
  `keys-verify` + `prover` (an override file swaps the prover's command for `--help`, drops the GPU
  reservation, and remaps the keys-disk mount to the same absolute path inside the container as on the
  host — the aggregate keys hash folds that path string in, so the two sides must agree on it for a
  pinned manifest to ever verify) shows the prover container actually leaving `created`, where it
  stayed stuck under the pre-fix image. The ELF is read via a real `sha256sum` as uid 999, never `ls`
  (a bit-flipped file at the right path would still pass an existence check forever); a 0600 ELF fails
  that same read.
- **New standalone check: the keys verify script run directly inside the built image** (`docker run`, uid 999,
  entrypoint overridden to `bash`) — the exact check a hand-replayed install never performed. A flipped-byte control
  and a mutation reverting the `sha256()` helper to the pre-fix hardcoded `shasum` both confirm the check is not
  vacuous.
- **The `prover.env`-exclusion negative control is real now**: the previous version only re-asserted a
  file the test itself had just planted a moment earlier. It now runs the real tar line into a scratch
  archive and asserts `prover.env` absent, then removes the `--exclude` token in a scratch copy of the
  Makefile and asserts it present.
- Every fixture this run drops lands in the real (gitignored) prover deploy directory — the tar
  always reads from there, never a caller-parameterised path — and is removed again in the script's
  own cleanup; `docker ps -a`/`docker volume ls` read identical before and after.

### Prover deploy — manifest guard on every push path, entrypoint re-verify pinned, mode/argv hygiene

Two on-box file/mode bugs turned up, along with a push path that bypassed the manifest guard, and a guarantee
(`docker-entrypoint.sh`'s own re-verify) with zero test coverage. Reproduced RED, fixed, verified against the real
image and a real `docker compose up` on a build machine.

- **The GPU textfile script writes `gpu.prom` 0644**, in both branches — `mktemp`'s own 0600 default left it
  unreadable to node-exporter (runs as `nobody`), so `node_textfile_scrape_error` reported 1 for a file that was
  actually present and well-formed. Verified live: `docker run --user 65534:65534` (the same uid) reads it clean
  through a read-only bind mount.
- **`prover-push-scripts` now runs the keys-manifest push guard before its own tar** — this path also carries
  `keys.sha256` but, unlike `prover-push-files`, never ran the guard: against a prover host whose manifest was
  already pinned it exited 0 and overwrote the pinned manifest with the committed `UNPINNED_PENDING_FIRST_BOOTSTRAP`
  placeholder, desynchronizing the prover host from the real key material bootstrap already verified.
- **Every non-secret config file the tar carries lands 0644 on the prover host**, even when it happened to be 0600
  on the operator's own machine (a real occurrence: a fixture copied `cp -p`, or a restrictive umask) — `vkey.json
  genesis.json derive.toml .env docker-compose.yml keys.sha256 *.template` are all re-chmod'd right after every
  extract, in both push paths; previously only jwt.hex/the ELF/scripts got this treatment.
- **`docker-entrypoint.sh`'s own re-verify is now exercised directly against the built image**: a key
  byte flipped, then a real `docker run` of the image reproduces the container-level (re)start
  path (Docker's own `restart: unless-stopped` after a crash, or a bare `docker
  start`/`docker restart` — neither goes through compose's dependency graph, so the compose-level
  `keys-verify` one-shot cannot be what catches this) — refuses `exit 12`, no `Usage:` text. The same
  flip against a bind-mounted scratch entrypoint with the verify call removed reaches `exit 0`, `Usage:`
  printed — the exact regression this guard exists to catch. The
  compose structure test also now pins the
  image's own `ENTRYPOINT` via a Dockerfile grep, mutation-tested.
- **The Postgres password no longer appears on `sed`'s own argv**: the config render script's `__DATABASE_URL__`
  substitution now goes through `-f <(printf ...)` (a script file) instead of a literal `-e` argument, with
  `\`/`&`/`|` escaped in the replacement text.
- **The prover health check's fifth item surfaces the keys verify script's
  own named refusal**: its `ssh_cmd` discarded stderr,
  where every one of that script's reasons (`KeysShaMismatch`, `KeysManifestUnpinned`, ...) is written — a new
  `ssh_cmd_stderr` (`2>&1`) is used for this one caller.
- **Three previously-untested guards pinned**: `umask 077` in the config render script and `prover-up-install`, and
  `prover-push-scripts`' own `--no-same-owner --no-same-permissions` — the first two are builtins a PATH stub cannot
  intercept, so these are source greps, the only way to test one.
- **The prover host simulation's tar-exclusion mutation test now derives
  the real recipe line from `make -n`** instead of
  hand-copying it as a second implementation, so it can never silently drift out of sync with the Makefile it is
  meant to be checking. Its cleanup now records `docker images` (repository + tag) before anything runs and only
  untags `ghcr.io/paradigmxyz/reth:v2.5.2` or the local `:main` tag if this run's own baseline shows them absent — a
  build machine that already had either cached from something else is left alone; the final equality check now covers
  `docker images` alongside `ps -a`/`volume ls`.
- Doc drift closed: the bootstrap script's header still named `prover-push-files` as its prerequisite (moved to
  `prover-push-scripts` since); the service count was "five" in three places against a compose file that has carried
  six (`keys-verify`, a one-shot) since that service was introduced; the keys verify script's own comment said the
  runtime image lacks perl entirely, when it carries `perl-base` without the separate `shasum` script the full
  `perl` package ships.

### Batch ids are 1-based per chain; 0 stays the sentinel everywhere

- **A fresh chain's inbox cursor now bootstraps at 1, not 0.** Settlement's own continuity check
  (`PostRootProved`/`PostRoot`) requires the first postable batch to be `head_pending_batch + 1 = 1`, so a
  chain whose inbox also starts counting at 0 can never finalize its first batch — found live on a real
  reset. The chain registration script passes `--next-batch 1`;
  `init_cursor` itself defaults to 1 and refuses
  `--next-batch 0` by name unless `--allow-zero` is passed. The inbox program's own `InitBatchCursor`
  handler is unchanged and still accepts any value — the 1-based rule lives entirely in tooling.
- **Derivation's resume anchor starts a fresh chain's numbering at batch 1.** At the settlement genesis
  sentinel (`root.number == 0`, `head_final_batch == 0` — "none"), `start_at_batch` used to short-circuit
  to 0 to match the old 0-based inbox convention; it now collapses to the same `head_final_batch + 1`
  expression used everywhere else (0 + 1 = 1).
- **The batcher's startup sweep and chain-anchor walk are unchanged in behavior** — both already treat an
  absent batch 0 as `Missing`, never an error, and neither needed the old sentinel ambiguity resolved to
  work correctly. A regression test on each pins the shape a 1-based inbox produces at the sentinel.

### `SetRegistryEntry` — verifier-key rotation with an activation delay, retirement is terminal, duplicate vkeys unconstructable

- **`rome-zk-layouts::registry` gains a v2 activation-slot tail**, appended after the existing 35-byte
  entries without changing the shipped v1 layout at all: `OFF_ACTIVATION = 185`, `REGISTRY_LEN_V2 = 217`
  (the fixed `[u64; 4]` LE tail). `read_header`/`entry_at` accept both lengths — a v1-length account's
  entries all read back `activation_slot == 0` (active since genesis, exactly what `InitChainV2` still
  writes); `find` takes an explicit `at_slot: u64` and skips any entry whose `activation_slot` is still in
  the future. `RETIRED_SLOT = u64::MAX` is a reserved tombstone activation slot: no real `Clock::slot`
  ever reaches it, so an entry carrying it is permanently invisible to `find` from the moment it is set.
- **`InitChainV2` refuses two `registry_entries` sharing `(curve, scheme, vkey_hash)`** —
  `DuplicateRegistryEntry`, checked before any account is written. One vkey is one ELF is one layout, so
  this holds even when the two entries name different `layout_id` values; a genesis registry can never be
  constructed with an ambiguous key in it.
- **`SettleIx::SetRegistryEntry`** (discriminant 25): registry-authority-gated, the same pattern as
  `SetFee`. Realloc's the registry account v1 → v2 in place the first time any chain rotates (payer tops
  up rent; every existing entry's bytes and `count` survive untouched). Scans for `(curve, scheme,
  vkey_hash)` in the same first-match order `registry::find` uses — never on the layout alone, and never a
  later duplicate — refusing `InvalidAccountData` if a second populated entry ever shares that key
  (defence in depth; unreachable once `InitChainV2`'s own check holds). A found vkey either has its
  `activation_slot` updated (refusing `LayoutMismatch` if its stored `layout_id` differs — one vkey is one
  ELF is one layout) or, **once its stored slot is the retirement tombstone, refuses any OTHER activation
  slot outright (`EntryRetired`) — retirement is terminal: a retired vkey cannot be re-activated while its
  entry stands**
  (retiring an already-retired vkey again stays a no-op success); an absent vkey is appended when the
  registry has room, or — once at `MAX_ENTRIES` — written into the first slot whose key has been retired
  (`RegistryFull` if none is). Refuses a backdated `activation_slot` by name (`ActivationInPast`; equal to
  the current slot is allowed — immediate activation; `RETIRED_SLOT` always passes this, by construction).
  `PostRootProved` threads `Clock::get()?.slot` into `registry::find`, so an entry registered for the
  future is invisible until then — the same refusal (`RegistryEntryNotFound`) a never-registered vkey
  gets. **Retiring a vkey is the same instruction, on the same vkey, with `activation_slot =
  RETIRED_SLOT`** — no separate instruction or discriminant — and is the one lever that actually stops a
  compromised key; a rotation's new key arriving never, by itself, disables the old one. What retirement
  cannot do on a four-slot registry: once the retired entry's slot has been reused for a new key, the
  registry no longer remembers the retired vkey and naming it again is an ordinary registration — retired
  ELFs/vkeys stay on the authority's off-chain list and are never re-registered.
- **Client:** `set_registry_entry_ix` builder; `decode_registry_account` decodes every populated entry
  (not just the header), each carrying its own `activation_slot` and a derived `retired` flag; the
  `governance` example's `set-registry-entry` subcommand accepts `--activation-slot now|retire|<u64>`.
- **Tests (real BPF):** a `SetRegistryEntry`-appended layout-1 entry carrying the real programVK
  reaches the on-chain PLONK pairing (~559k CU) against a chain that started with only the
  header-fallback layout-2 entries; a delayed rotation of a different vkey under the same curve/scheme/
  layout never touches the still-live old key (old key before, during the delay, and after the new key
  activates all reach the pairing; the new key is `RegistryEntryNotFound`, ~13k CU, until its own
  activation slot); the old key is refused immediately, by the same name, only once explicitly retired,
  with `count` unchanged; a genesis registry naming the same vkey twice — including under two different
  layouts — is refused by name; a hand-crafted registry account carrying a duplicate vkey is refused
  `InvalidAccountData` rather than acted on; naming a retired vkey again with a real activation slot is
  refused `EntryRetired` (retiring it again stays Ok); a full registry with one retired slot reuses that
  slot for a new vkey with every other byte of the account identical before and after; a full registry
  with none retired is `RegistryFull`; the same vkey under a different layout is `LayoutMismatch`; a
  same-vkey activation update changes only the 8-byte tail; five hand-built-instruction guards (registry
  PDA identity, header `chain_id`, `UnknownLayout`, `UnknownCurveOrScheme`, payer-signer/system-program,
  the last one also proven on a grown v2 registry where no CPI independently enforces it) are each
  mutation-tested; a non-registry-authority signer and a backdated activation slot are refused by name;
  the v1 → v2 realloc is proven to preserve every pre-existing byte.
- This is the production rotation path: a chain's first layout-1 entry can still land once, at genesis,
  via `InitChainV2`, but every rotation and every retirement afterward — including Tiber's — goes through
  `SetRegistryEntry`.

### Chain registration writes the layout-1 verifier entry; Tiber's batch cadence widens to 60 blocks

- **`register_chain` now writes three genesis registry entries, not two.** (This now applies to reserved
  chains only. A permissionless chain registers with an empty registry and refuses the verifier-key flags;
  see "Permissionless chain registration and proved finality".) The layout-1 primary — the
  chain's own ZisK stateless-validator guest vkey, read from a `--layout1-vkey-json` file — comes first,
  ahead of the existing layout-2 header-RLP fallback and the zeroed Groth16 placeholder slot. The example
  refuses to run without `--layout1-vkey-json`, by name: a chain registered without a layout-1 entry can
  never finalize a proved root, so there is no silent fallback to layout-2-only. The entry construction is
  a standalone, unit-tested function (`zk_settlement_client::registry_entries_for_init`) rather than
  inline code in the example, so the ordering and field values are pinned directly rather than only
  through an end-to-end run. The Tiber chain registration script passes
  `fixtures/vkeys/tiber-200101-layout1.json` by default (override with `LAYOUT1_VKEY_JSON`); its dry-run
  test asserts the flag appears in the printed command.
- **The reproducible guest ELF now has a recorded verification key.** `fixtures/vkeys/tiber-200101-layout1.json`
  pins the layout-1 vkey for the fork's `bin/guests/stateless-validator-rome` build (the same reproducible
  build documented in that crate's own README), alongside the ELF's sha256, the ZisK toolchain version and
  the exact build/prove commands used to produce it. This is a different verification key from the one
  first measured — that earlier proof ran against a pre-reproducibility-fix build of the same
  guest, which does not hash-match the ELF this fixture pins, so its vkey was never registered against any
  chain.
- **Tiber's own `blocks_per_batch` moves from 10 to 60** (the crate default, `DEFAULT_BLOCKS_PER_BATCH`, is
  unchanged at 10 — this is a per-profile override): one root post every 60 seconds at the sequencer's default
  cadence instead of every 10, a sixth as many batches while the chain is idle.
  `crates/rome-zk-sequencer/config.example.toml`, the Tiber deploy directory's `derive.toml.template` and the
  example Tiber env file all carry the value explicitly — `rome-zk-derive` does not read the sequencer's
  `profile.json`, so its own config has to agree with the sequencer's by hand, and a comment on each says so. The
  Tiber health check's verifier-lag bound becomes 120 blocks (twice the batch size), about two minutes at the
  chain's one-block-per-second cadence. This is the chain's idle-load upper bound, not a throughput promise: at the
  chain's designed gas rate a 60-block batch's DA already exceeds the batcher's per-batch frame cap well before 60
  blocks accumulate, so under real traffic the batcher closes each group on that cap first.
- **Tests:** `registry_entries_for_init` is unit-tested directly (layout-1 entry first, correct
  curve/scheme/vkey per slot); the sequencer's and derive's own config-loading tests assert 60, not 10, for
  the profile shipped with this repo.
- This reset needs new program ids: `register_chain` refuses a chain whose root account already exists,
  and every prior Tiber reset hit that the same way — a Tiber reset has always rotated ids for this
  reason. The chain id cannot move either: the guest ELF embeds the chain's own genesis at build time.

### Batch guest + prover input (`crates/rome-zk-prover-input`; guest in rome-zk-guest)

- **`rome-zk-layouts::public_values` gains `write`** (the encoder — `read` alone decoded a caller-supplied
  blob and was not its own inverse), pinned by a round-trip test. Needed so the guest can commit its own
  computed `PublicValues` fields as bytes.
- **`crates/rome-zk-prover-input`** (host-only, its own workspace — root `Cargo.toml`'s `exclude`): reads a
  finalized batch's inbox chunk bodies + batch account (`zk-inbox-client`'s own read path, `acc` verified
  against the batch account's own recorded value), decodes the channel to learn the batch's `first..=last`
  block range, loads chain config from a genesis JSON, and fetches each block + its execution witness from
  a reth verifier over **plain JSON-RPC (`ureq` + `serde_json`)** — no `alloy-provider` (its generated
  Multicall3/ArbSys `sol!` bindings never resolved in this crate's own small dependency graph); the crate
  builds and tests clean by default, no feature flag needed.
- **`crates/rome-zk-prover-input-cross-repo-wire`** (new, its own workspace): proves this crate's wire
  types encode byte-for-byte compatibly with the fork's own `guest_rome::input` types, both directions —
  path-depends on a `.fork/` checkout of the fork (never committed; `run.sh` skips loud with the clone
  command when that checkout is absent).
- **New client `guest-rome`** in `rome-protocol/rome-zk-guest` (
  not this repo): proves one finalized batch — DA binding (accelerated keccak via `alloy-primitives`'
  `native-keccak`, passed through `rome-zk-layouts`/`rome-zk-merkle`'s existing `HashV` trait), channel
  decode + block-for-block equality against the witnessed blocks, per-block chaining + a one-sided drift
  bound + stateless validation (reusing that repo's `reth` client's own recovery/validation path
  unmodified), then the public-values-v2 commit. Builds a real ELF for the ZisK target
  (`riscv64ima-zisk-zkvm-elf`); a companion bench harness (`bin/guests/bench-rome-dahash`) measured the
  DA-hash stage alone at 3,838,075 (accelerated) vs 12,754,010 (software) steps on the 10×300 synthetic
  batch, and reproduced the real batch 2043 fixture's on-chain `acc` byte-for-byte under
  `ziskemu`.
- **Real end-to-end run (batch 3930, chain 200101, blocks 39181..=39190):** the real guest ELF,
  run under `ziskemu` against a real batch's input, committed public values matching the host's
  independently-computed expectation on every field, plus `last_block_hash`/`state_root` matching the
  Tiber verifier's own values for the batch's last block — 576,056 steps, 0.0131 s. Found and fixed a real
  bug on this first execution: `ziskos`'s public-output mechanism is a 64-slot, 32-bit register file (256
  raw bytes total), not the 512-byte buffer the guest's commit call assumed — `run()` now commits each
  packed word as 4 raw bytes, not 8. Four mutations on the real input each refused by name under `ziskemu`
  (wrong witness, a flipped chunk-body byte, `max_drift_secs = 0`, a replaced parent header); a fifth —
  lowering `max_drift_secs` below the batch's nominal offset — does **not** refuse on this real, idle
  batch, because every block's timestamp already precedes the batch's own `open_unix_ts` (the check is a
  one-sided upper bound), a finding recorded in the crate's README rather than a gap papered over.
- No proof is generated here; no change to any existing client or program.
- **Closed a soundness gap:** the guest bound only `number`, `timestamp` and
  tx bytes to the DA stream — every other derivation-rule header field, and the whole `chain_config`,
  were host-supplied and only consensus-bounded, so a host could commit a different (still
  consensus-valid) value for any of them. `rome-zk-executor-api` gains `HeaderRule`/
  `canonical_header_rule(chain_id, number, fee_recipient)` — the ONE shared rule the sequencer's block
  env, `rome-zk-derive`'s payload-attributes construction, and the guest's own per-block assertion all
  build from now. The guest asserts every rule-fixed field (`prevRandao`, `beneficiary`, `extraData`,
  `withdrawalsRoot`, `parentBeaconBlockRoot`, `blobGasUsed`, `excessBlobGas`) per block before stateless
  validation of that block, and checks the DA stream's own `gas_limit` against the witnessed header.
  `RomePublicInput` (wire v2) drops `chain_config` entirely — the chain's genesis and fee recipient are
  baked into the guest ELF at compile time instead, asserted against `public.chain_id`
  (`ChainConfigIdMismatch`). Verified against the real `zec-rome` ELF under `ziskemu` on the real,
  migrated batch-3930 input: the honest input still commits identical public values to before the fix;
  seven mutations each refused by name.
- Smaller fixes: the fork's `native_keccak256` stub deleted (`ziskos` already
  exports it for non-`hints` host builds); both bin workspaces' `Cargo.lock` committed, ELF sha256
  recorded; `rome-zk-prover-input::inbox::fetch_and_verify_batch` refuses an unfinalized batch by name;
  the CLI's sidecar gained a `provenance` block.

### Channel / batcher (`crates/rome-zk-channel`, `crates/rome-zk-batcher`)

- **`reassemble` refuses any duplicate `frame_no` by name** (`ChannelError::DuplicateFrame`). It used to keep
  the last write and assert in a comment that duplicate bodies "must be identical", so a differing duplicate
  made the output depend on frame order. No producer emits duplicates (`cut_frames` numbers 0..n once; a
  resubmit re-sends the same transaction against the same chunk PDA).
- The batcher's anchor reader applies derive's rule: the frame stored at chunk idx *i* must carry
  `frame_no == i` (`AnchorError::FrameNoMismatch`), refused before reassembly.

### Prover (`guest/rome-zk-bench-decode`, `crates/rome-zk-channel`, `crates/rome-zk-merkle`)

- **Measurement guest**: a ZisK guest that measures keccak+merkle+acc over chunk bodies, frame reassembly,
  pure-Rust zstd (`ruzstd`) and RLP decode on the real batch 2043 (`fixtures/inbox/txv1-dev-batch-2043.json`)
  and synthetic loaded batches from the real encoder. Decode is ≈ 13 steps per compressed byte; software
  keccak ≈ 49 steps per byte.
- `rome-zk-channel` gains cargo features `zstd-c` (default, unchanged behaviour) and `decode-pure`
  (`decode_stream_pure` via `ruzstd`, the zkVM path) with a pure == C equivalence test run in CI.
- `rome-zk-merkle`'s `solana-program` dependency is target-gated to `cfg(target_os = "solana")` — its own
  code only references it there; the on-chain path is unchanged (`cargo build-sbf` verified).
- Found: `rome-zk-layouts` cannot yet be a zkVM dependency (`solana-program` → `getrandom 0.1.16` has no
  zisk target); a feature gate for it follows. The bench guest carries a host-test-pinned `shim` until then.
- **`rome-zk-layouts` and `rome-zk-channel` build for the ZisK target; the bench guest's shim is
  deleted.** `rome-zk-layouts` gains a default-on `solana` Cargo feature gating
  `solana-program` and every `pda()`/`seeds()` helper needing a real `Pubkey` (11 modules, 31 sites); every
  byte-level constant, `read`/`write`, `acc`, `forced_empty_root`, `frame`, `chunk` header and
  `public_values` stays unconditional and pure. `rome-zk-channel` depends on layouts with
  `default-features = false` (it only ever touched `frame::*`). `guest/rome-zk-bench-decode` now depends on
  both real crates directly (`default-features = false`, channel with `decode-pure`) — the bench `shim`
  module and its two-implementation ("reference" vs "zisk-safe") split are gone; `cargo-zisk build
  --release` builds the guest against the real crates, and `ziskemu -m` on the committed real-batch fixture
  reproduces the on-chain `acc` (`965a19bc…`) in 36,968 steps (previously 40,872 with the shim; the ~9.5 %
  drop is a removed fixed-cost mixing hash, not a decode-cost change — the synthetic-batch measurement
  moved only +0.13 %). Every program (`cargo build-sbf`) and host consumer keeps the default `solana`
  feature and is unchanged; `cargo check -p rome-zk-layouts --no-default-features` and
  `-p rome-zk-channel --no-default-features --features decode-pure` are required CI checks.

### Deploy (shared Solana node)

- **Rome's shared standalone Solana node is now configured from this repo.** The shared node's deploy
  directory holds the image build (Anza release tarball, `solana-test-validator`), the entrypoint (genesis
  with SIMD-0500 deactivated — this repo's programs are SBPF v0) and the compose file for the shared validator
  VM; README carries the upgrade/feature-change steps (a feature change is a ledger wipe).

### Deploy (Tiber)

- **Tiber's co-located Agave is retired**: `docker-compose.solana.yml` and its Solana start and
  tunnel targets are gone; the settlement cluster is the shared standalone Solana node
  (with its own deploy directory), reached through its TLS front by containers and operator alike.

- **The settlement cluster is a runtime parameter.** Every settlement-side tool — the program deploy script,
  the top-up script, the chain registration script, the Tiber health check, the
  rendered batcher/derive configs — reads the Solana RPC from
  `SOLANA_RPC_URL` (set in the Tiber env file; the deploy target renders it from a `SOLANA_RPC_URL=<url>` argument
  and refuses to render without one); no `-u devnet` / public-endpoint literal remains (the
  cluster-parameter test in the Tiber deploy tests). Tiber's settlement now runs on **its own single-node
  Agave** (the Tiber deploy directory's `docker-compose.solana.yml`: `solana-test-validator` 4.2.1 beside the
  sequencer — SIMD-0500 deactivated so this repo's SBPF v0 programs deploy, Transaction V1 and the alt_bn128
  syscalls active, 200 ms slots, own faucet; started and tunnelled through its own Make targets) instead of the
  public devnet cluster; program ids and every pinned PDA are unchanged (same program keypairs, same seeds, new
  cluster). Rome's shared standalone node refuses the deployment (SIMD-0500 active there).

### Sequencer (`rome-zk-sequencer`)

- **Blocks are numbered from 1, coinciding with the EVM header number.**
  A fresh chain's genesis is real height 0 (never sealed by the sequencer); the first block the sequencer
  ever seals is block 1, and `BlockEnv.number` therefore equals the resulting header's real block number
  for every block — no consumer of a committed block env needs to apply an offset any more.
  `rome-zk-executor-reth`'s restart-time read of the persisted head (`last_persisted_block()` now reports
  the real header number directly, no `-1` conversion) and `rome-zk-derive`'s engine equality check and
  settlement-root resume anchor (see the Derivation node entries below) changed in lockstep.
  **Devnet-only consequence:** this changes every already-committed block's real height and
  `prev_randao` — a running devnet must be reset, never migrated in place;
  this is never acceptable on a live chain.
- Single-node sequencer: bounded admission queue (arrival order is sealing order — no fee-ordered
  mempool), per-sender nonce cache with nonce-gap parking, 50 ms signed sub-blocks and 1 s blocks.
- An append-only, fsynced ordered log is the durable source of truth; a pre-confirmation is returned only
  after its sub-block is appended and fsynced.
- Execution runs through an `Executor` trait (`rome-zk-executor-api`); the default implementation is an
  in-process Reth engine (`rome-zk-executor-reth`) that persists chain state asynchronously and replays
  the log's unpersisted tail on restart.
- JSON-RPC + WebSocket server (`eth_sendRawTransaction`, `eth_chainId`, and Rome-specific
  pre-confirmation endpoints).
- No multi-node replication yet — the ordered log has no replication trait implementation beyond the
  single-node case.
- **Chain profile as configuration (`src/profile.rs`, `[profile]` in `config.toml`):** sub-block period,
  sub-blocks per block, blocks per batch, gas limits, admission headroom and the DA/prover throughput
  budgets are one declared-and-validated table instead of independent hardcoded constants — a config that
  omits `[profile]` keeps every prior default. Loading rejects an internally inconsistent profile (a
  sub-block period × count that isn't a whole number of seconds; an explicit `block_gas_limit` that
  disagrees with `sub_block_gas_limit × sub_blocks_per_block`; an implied gas/s or DA rate over the
  profile's own declared budget), naming the bound. `sub_block_gas_limit`/`block_gas_limit` moved out of
  `Config`'s top level into `[profile]`; a config still carrying either at the old top level is refused at
  load, naming the stray key, rather than silently ignored.
- Startup replay checks the ordered log's actual block/sub-block shape against the configured
  `sub_blocks_per_block`: a record whose index is at or past that bound, or a block-number change that
  doesn't land on that boundary, is refused (`RecoveryError::ProfileMismatch`) — a log sealed under one
  profile and replayed under a different one is refused with the configured value named, not silently
  replayed to a divergent head.
- **The chain's profile identity is persisted beside the ordered log (`profile.json`)**: written the
  first time the log is opened, containing `chain_id`, `sub_block_ms`, `sub_blocks_per_block`,
  `sub_block_gas_limit`, the effective `block_gas_limit`, and `blocks_per_batch`. Every later start
  compares the stored identity against the configured one (after every env override) *before* any replay
  work runs; any disagreement — including `chain_id` — is refused, naming the field, the stored value,
  and the configured value (`RecoveryError::ProfileJsonMismatch`). An existing log with no `profile.json`
  (written before this check existed) refuses to start unless `--write-profile-json` performs the
  one-time migration.
- **`blocks_per_batch` joined the persisted identity**: a `profile.json` that predates this field (every
  other field present, this one missing) is refused by name
  (`RecoveryError::ProfileJsonMissingBlocksPerBatch`), distinctly from a wholly-missing file — the same
  `--write-profile-json` flag, passed once more, upgrades it by rewriting the file from the configured
  profile. Never silently defaulted.
- **The numbering-from-1 premise is enforced, not just arranged.**
  `replay_into_executor` refuses unless the log's first record is `(block 1, index 0)`
  (`RecoveryError::LogNumbering`) — valid because the ordered log is never pruned, so whatever the first
  record actually is, is genuinely the chain's first-ever sealed block. `first_block` joined the
  persisted `profile.json` identity (the same migration-flag shape as `blocks_per_batch`): a file
  predating it is refused (`RecoveryError::ProfileJsonMissingFirstBlock`), before any replay work runs —
  but, unlike `blocks_per_batch`, never migratable over a log that already has real (0-based) records;
  only a fresh chain clears it. `rome-zk-executor-reth`'s `open_block`/`seal_block` independently refuse
  a `BlockEnv.number`/`BlockSealInputs.block` that disagrees with the real reth height
  (`ExecutorError::EnvMismatch`) — the equality becomes unconstructable at the executor boundary itself,
  not only asserted by callers that happen to agree with each other.
- **The numbering-origin check is one function, called by every writer of `profile.json` and by replay.**
  `recovery::log_numbering_origin` reads the log's own first record and is called
  before every `profile.json` write over a directory that already has records — the migration path that
  predates `blocks_per_batch`, the one that predates `first_block`, and the no-file case alike — so none
  of them can stamp `first_block: 1` over a genuinely 0-based log; `replay_into_executor` calls the same
  function before replaying a single record. `rome-zk-batcher` calls it directly on the log it reads,
  independent of `profile.json` (see the Batcher section below).
- **The `/metrics` HTTP responder moved into its own crate, `rome-zk-metrics-http`.** `metrics::
  serve_metrics` is now a thin call into it; this crate's own behaviour, `metrics_addr` config field and
  tests are unchanged — the move only lets `rome-zk-batcher` reuse the identical responder without
  pulling this crate's reth dependency in.

### Batcher (`rome-zk-batcher`)

- Reads the sequencer's ordered log tail, groups blocks into a compressed channel stream, and cuts the
  stream into inbox chunks, each posted as **one SIMD-0385 V1 Solana transaction per frame** — `Open` +
  one `Write` of the frame's entire body + `Seal` + `SealLeaf`, no split across several transactions and
  no v0 (legacy, 1,232-B) fallback: the design has always assumed V1's 4,096-byte envelope, and the
  3,681-byte max frame body only fits it, not v0.
- A shadow compressor keeps output within the chunk size limit; a stateless resume/resolve pipeline
  rebuilds posting state from on-chain reads on every start, rather than persisting its own state.
- A startup preflight refuses to run unless the configured inbox program matches what the settlement
  program's registry names for the chain, and unless the payer is the chain's registered authority. A
  second, separate startup check derives the real loaded-accounts-data-size requirement for the
  configured `max_frames_per_batch` (reading each involved account's live length via RPC) and refuses to
  run if the configured limit is below it.
- The per-payer send rate is bounded by Solana's per-block, per-writable-account compute budget: every
  chunk-lane transaction write-locks the fee payer, so its cost is charged against that budget at the
  transaction's *requested* compute-unit limit. The cost model itself — signature (720 CU) + write-lock
  (300 CU per writable account) + instruction-data (Σ instruction-data bytes ÷ 4) + loaded-accounts pages
  (8 CU per 32 KiB) — mirrors `agave`'s own cost calculation, not the transaction's wire size.
- The Solana sender is an RPC-based implementation; the design's staked TPU/QUIC sender is not yet built.
- `BlockSource` groups the ordered log by the chain's `sub_blocks_per_block` and publishes its
  `block_gas_limit`, both read from the sequencer's own `profile.json` (written beside the log —
  `config::read_profile_identity`) rather than from a parallel `config.toml` field: the batcher never
  owns its own copy of the chain's block shape, and there is no built-in default to fall out of sync with
  the sequencer's.
- **`blocks_per_batch` joined `profile.json` too**: `read_profile_identity` now yields it directly, and
  this crate's `Config` no longer has a `blocks_per_batch` field at all — the grouper cap (below) comes
  entirely from the sequencer's own declared value. A `profile.json` still missing the field (written
  before it existed) is refused, naming the file and pointing at the sequencer's `--write-profile-json`
  migration flag, never silently assumed to be the old shared default.
- **`BlockSource` peeks one record ahead before ever handing back a "complete" block**: if the log
  genuinely holds more sub-blocks for a block than this process is configured with **and that sub-block is
  already on disk**, it refuses (`SourceError::ProfileMismatch`) instead of returning a truncated block
  first and only erroring on the *next* record. This is a mitigation for a log whose real shape disagrees
  with its own profile identity, not a live-tail guarantee — at the live tail the next sub-block is not yet
  written, so a completed block is emitted and the mismatch surfaces one record later; the profile-identity
  cross-check (`profile.json`, above) is the actual guard against a genuinely misconfigured
  `sub_blocks_per_block`.
- **One grouping path from the chain anchor, both modes** (`--once` is `--follow` without the tail wait):
  both open the log AT the on-chain anchor and verify the log's own first block matches it exactly (a
  named `Gap` refusal otherwise — a gap in the log, or the log and chain have diverged), then accumulate
  into one `grouping::SizeCappedGrouper` seeded with `anchor.from_block - 1`, posting every group of at
  most `blocks_per_batch` **consecutive** blocks that closes along the way; `--once` additionally posts
  whatever partial group is left once the log is exhausted, then exits, while `--follow` waits for the
  tail instead. There is no from-block-0 regrouping any more — the earlier shape misaligned whenever a
  previous run's tail group was partial (a rerun's fixed 0-anchored windows and the true anchor position
  agreed only when the previous run's block count happened to be an exact multiple of the cap). Continuity
  — a gap in the log (a missing block) is a named grouping error and nothing from that run is posted — is
  enforced *across every group boundary*, restart included (`BlockGrouper::take_group` carries its own
  last block forward, seeded on start from the anchor). These are the same continuity invariants
  `rome-zk-derive`'s `batch_queue::decode_batch` checks on the read side, proven directly by a
  dev-dependency cross-crate test — size-closed groups included. `blocks_per_batch` is read from the
  sequencer's own `profile.json` (below) — this crate owns no config field of its own for it any more.
- **A group also closes early — before the `blocks_per_batch` cap — if the next block would push the
  channel's compressed encoding over `max_frames_per_batch`** (`grouping::SizeCappedGrouper`, sharing
  `ShadowCompressor`'s own size accounting): a full-cap group at the design gas ceiling can exceed the
  frame budget, and without this a rerun of either mode would re-read the same over-budget group and stall
  forever; the carried-over block starts the next group instead.
- **Both `--once` and `--follow` resume from one on-chain chain anchor, not a local file**
  (`anchor::resolve_anchor`, replacing the old `batcher-cursor.json`): it re-derives the last finalized
  inbox batch's own posted blocks straight from its still-sealed chunks (paged reads, up to 100 batch ids
  per read, header parse, `Frame::from_bytes` on exactly `header.len` bytes of body → `reassemble` →
  `verify_acc` against the on-chain `acc` → `decode_stream`); when a finalized batch's chunks have already
  been rent-recycled (`head_final_batch >= batch` — otherwise a hard refusal, since `Close` could not have
  legitimately run yet) or every ever-opened batch is abandoned, it falls back to the settlement root's
  own `number` with the timestamp seed recomputed exactly by replaying the ordered log's own
  monotonic-timestamp recurrence (never guessed at 0, and now a named `LogReplayMisaligned` refusal — not
  a `debug_assert` — if the log's own numbering disagrees with the replay at any step). A not-finalized
  batch found at `batch_cursor.next_batch - 1` at startup is always `AbandonBatch`'d and its chunk PDAs
  closed (packed several per transaction, cutting a real abandoned 900-chunk batch's restart cost roughly
  4x) before the anchor is resolved or anything is posted — a stateless resume never guesses whether a
  half-written batch is "maybe still live". `--from-block <N>` is a **verified** cross-check only (must
  equal the resolved anchor, else a named refusal) — there is no unverified resume override.
- **Exactly one batcher process per chain authority, enforced.** The run reads `batch_cursor.next_batch`
  once, right after the startup abandon, as its own `expected_next_batch`; if the live on-chain cursor ever
  disagrees with it, another writer holding the same chain's authority key posted in between, and this run
  refuses by name (`resolve::ResolveError::CursorAdvanced`) rather than resolving a fresh batch id under a
  moved cursor — a second instance with the same key is destructive, not merely unsupported.
- **The chain anchor is 1-based, matching the sequencer's own numbering.** A fresh
  chain's `from_block` is `1`, not `0` (genesis is never sealed); the settlement-root fallback's
  `from_block` is `root.number + 1`, not `root.number`; and the closed-batch timestamp replay walks
  the ordered log from block `1`, not `0`. Proven against a log sealed through the real sequencer
  (`SealerState` from `ResumePoint::default()`), not only a hand-written fixture — every batcher fixture
  that builds an ordered log now starts it at block 1. The two cross-crate sealer-to-batcher tests assert
  the log's own first record is `(block 1, index 0)`, not only what the log yields at the anchor, so a
  0-based producer is caught by the fixture itself, not only downstream.
- **The chain anchor checks its own log's numbering origin, independent of `profile.json`.**
  `anchor::resolve_anchor` reads the log's first record before any on-chain state and refuses by
  name (`AnchorError::LogNumbering`) unless it is `(block 1, index 0)` — this runs on every anchor path
  (fresh chain, finalized-batch decode, settlement-root fallback), so a hand-edited or migrated
  `profile.json` can never make this batcher accept a 0-based log. `config::read_profile_identity` also
  refuses a `first_block` that is present but wrong by value (`ConfigError::ProfileJsonFirstBlockNotOne`),
  matching the identical rule the sequencer applies to the same file.
- **Bounded posting window: up to `batches_in_flight` batches (config, default 2) post concurrently.**
  `OpenBatch(N+1)` is sent once `OpenBatch(N)` has confirmed, chunk lanes of different batches overlap freely,
  and `FinalizeBatch(N+1)` is sent only once `FinalizeBatch(N)` has confirmed and its hand-off to the
  `PostRootSink` is done (`pipeline::WindowedPoster`, one oneshot gate per batch, chained) — a bounded window
  overlaps the cluster's own inclusion-tail latency with the next batch's work instead of paying it serially,
  without ever reordering finalization (`rome-zk-derive` walks finalized batches in id order). A failure in
  any in-flight batch stops the whole run: a shared flag refuses any further `OpenBatch`, and
  `submit_group`/`finish` both drain to surface the first real error, and a batch whose predecessor failed
  before finalizing refuses its own `FinalizeBatch` by name (`PipelineError::PreviousBatchFailed`) instead of
  finalizing out of order — it stays open for the startup sweep. **Defense-in-depth:** the sweep refuses by
  name (`PipelineError::FinalizedAboveOpenBatch`), abandoning nothing, when a finalized batch sits above an
  open one in the pending window (a third party finalizing ahead of the real poster — now unconstructable at
  the program level, since `FinalizeBatch` requires the batch's own authority signer; this refusal remains
  only for a chain still on an older program version): abandoning the open batch there would strand its
  blocks; the batcher halts on that chain until the open batch is finalized or the program upgrade lands.
  Retry warnings and every RPC-error message are redacted (no request URL). **The startup sweep now abandons
  every open-not-finalized batch in the pending window `[root.head_final_batch, cursor.next_batch)`**, not
  only `next_batch - 1` (`pipeline::abandon_open_batches_in_pending_window`, generalising the single-batch
  case) — a bounded window can leave more than one batch open-not-finalized after a crash.
- **CU sampling moved off the posting path.** The two `getTransaction` reads a finalized batch's own CU
  figures come from now run as a spawned, budget-bounded background task (`pipeline::sample_cu_off_path`)
  whose outcome is a log line only — it can never delay the next `OpenBatch` or a `FinalizeBatch` gate
  signal. The batcher's own read `RpcClient` now carries an explicit 20 s per-request timeout (the same
  value the sender crate already uses), instead of the underlying HTTP client's 30 s library default.
- **New cadence metrics:** `rome_zk_batcher_open_confirm_seconds`/`_finalize_confirm_seconds`/
  `_batch_post_seconds` (histograms, 0.25 s – 60 s buckets), `_batches_in_flight` (gauge), and
  `_lag_blocks` (gauge: newest block visible in the ordered log minus the last finalized batch's own last
  block). Registered as Prometheus metrics (`Metrics::render`).
- **The batcher now serves its own `GET /metrics` and Prometheus scrapes it as a distinct target.**
  `metrics_addr` (config, default `127.0.0.1:9002`, env override `ROME_ZK_BATCHER_METRICS_ADDR`) is
  served by `rome-zk-metrics-http::serve` — the same reth-free responder `rome-zk-sequencer` serves,
  pulled into its own crate so this crate never needs a reth dependency for one HTTP endpoint — spawned
  once at startup, ahead of both `--once` and `--follow`. Tiber's rendered `batcher.settlement.toml` sets
  `metrics_addr = "0.0.0.0:9002"`, `docker-compose.settlement.yml`'s `batcher` service gains `expose:
  ["9002"]` (compose-network only, never a host `ports:` mapping — same rule as reth's own 9001),
  `prometheus.yml` gains `job_name: batcher`, and the Tiber health
  check gains a matching "Prometheus target
  job=batcher is up" check next to the existing `reth` one.
- **CU sampling is now rate-limited: `cu_sample_every` (config, default 50) samples only every Nth
  finalized batch.** Two `getTransaction` reads per sampled batch were previously spent on *every*
  finalized batch — needless load at any real posting rate, on top of the informational-only budget this
  was always meant to stay under. `cu_sample_every = 0` is refused at config load
  by name (`ConfigError::ZeroCuSampleEvery`) rather than left to divide by zero at runtime.
  `cu_samples_triggered_total` (new counter metric) is bumped whenever it is a finalized batch's turn,
  independent of whether an RPC client is actually configured to sample it — the cadence itself is
  directly observable. Both of `sample_cu_off_path`'s own log lines — a timed-out/unresolved lookup, and
  a `getTransaction` that never resolved after its retries — now log at `debug`, never `warn`: a slow or
  absent result on a busy or degraded cluster is routine, expected noise for an informational sample off
  the posting path, not an operational alarm.
- **`--follow`'s idle tail-wait now observes an in-flight sibling failure on its own.** Before this, a
  batch that failed while this process sat idle (no new blocks, nothing to submit) was only noticed inside
  the next `submit_group`/`finish` call — on a quiet chain that could be a whole batch period away, or
  never. `WindowedPoster::poll_failure` — a non-blocking drain, safe to call every idle tick — is now
  polled from the tail-wait's own sleep/refresh arm; a sibling failure there exits the process
  immediately instead of leaving it alive on a dead window.
- **`_lag_blocks` is now read from a live counter at each batch's own hand-off time, not captured at
  submit time.** The gauge used to take `newest_log_block` as a plain argument to `submit_group` — the
  block that happened to close the group — so it was structurally 0 or 1 regardless of how long a batch
  actually took to finalize. The main loop now maintains a shared `Arc<AtomicU64>` of the newest block
  seen from the ordered log (updated on every `source.next_block()`, both modes), and each batch's own
  settle task reads it when it actually hands off — the value the gauge needs.
- **The startup sweep pages its batch-account probes instead of reading one account per id.** Until
  `PostRoot` lands, `head_final_batch` stays 0 on a real chain, so the pending window
  `[root.head_final_batch, cursor.next_batch)` can span every batch the chain has ever opened —
  `pipeline::abandon_open_batches_in_pending_window` now probes it with
  `AccountOps::get_multiple_account_data`, in the same page size the chain-anchor walk already uses (one
  shared home, `resolve::PROBE_PAGE_SIZE`/`resolve::decode_batch_probe`), instead of one sequential
  `get_account` per id.
- **Hygiene:** `submit_group` re-checks the shared `failed` flag a second time, immediately
  after `resolve::resolve_batch_id` returns and before `OpenBatch` — `resolve_batch_id`'s own account
  reads are real await points, a window in which a sibling batch can fail and set the flag after the
  top-of-function check already passed. Every RPC-error-carrying error variant across this crate and
  `rome-zk-solana-sender` (`ResolveError::AccountRead`, `PreflightError::{RootAccountRead,RegistryAccountRead}`,
  `LoadedAccountsError::{ProgramAccountRead,ProgramDataAccountRead}`, `SenderError::Rpc`, the CU-sample
  `print_cu` log line) now carries a `message` field redacted at construction time
  (`rome_zk_solana_sender::describe_rpc_error`) instead of the raw `ClientError` `Display` — reqwest's own
  `Display` appends `" for url (<URL>)"`, which can carry a secret embedded in the URL's query string.
- The Tiber settlement config render script is now fail-closed on `SOLANA_RPC_URL`: a missing env file, or one that
  lacks the key, refuses by name — the previous silent fallback to the public, rate-limited devnet RPC is gone (ops
  values are runtime parameters; an absent one is a named failure, never a default).

### Inbox program (`programs/zk-inbox`)

- Chunk lifecycle: `Open` (requires the batch to exist and the caller to be its authority), `Write`,
  `Seal`, `Close` (requires the covering root to be final).
- `Seal { len, body_hash }` binds the sealed length to a `keccak256` hash of the account's own bytes
  (`body_hash` must equal `keccak256(body[..len])`), so a short-seal — a hole a `Write` never covered —
  is rejected by the program itself rather than only prevented client-side. **A sealed chunk is immutable
  in bytes and length**: `Write` on an already-sealed chunk is rejected outright, and a re-`Seal` that
  would change the stored `len` is rejected with a dedicated error; a re-`Seal` with the *same* `len` (the
  batcher's own idempotent resubmit path) stays an `Ok`.
- Batch accumulator: `OpenBatch` (authority-gated, ids issued from a sequential per-chain cursor so an id
  is never reused), `GrowBatch` (permissionless, reallocates an undersized batch account toward its full
  size), `SealLeaf` (permissionless, order-independent), `FinalizeBatch` (authority-gated, resumable
  Merkle reduction over the batch's chunks), `CloseBatch`, `AbandonBatch`.
- **`FinalizeBatch` now requires the batch's own `authority` signer.** Accounts become `[batch_pda
  (writable), authority (signer, readonly)]` (`batch_pda` stays at index 0, so the settlement watcher's
  decode path is unchanged); refused (`MissingRequiredSignature`) when the account is absent, unsigned, or
  simply not the stored authority — every resumable step call needs the same signer. `SealLeaf` stays
  permissionless. Closes the gap the batcher's startup-sweep `FinalizedAboveOpenBatch` mitigation was
  covering for: with a bounded posting window, a third party could otherwise finalize a later batch id
  ahead of the real poster's own still-open one, stranding it — that shape is now unconstructable at the
  program level; the sweep's refusal stays as defense-in-depth for a chain still on an older program
  version. `zk_inbox_client::finalize_batch_ix` gained an `authority: &Pubkey` parameter; the batcher
  passes its own chain authority (`target.payer`, already established).
- `InitBatchCursor` bootstraps the per-chain sequential batch-id cursor.
- Every PDA this program creates uses create-or-adopt creation, so a pre-funded address cannot block it.
- **Batch account header v2: `open_unix_ts`.** `OpenBatch` now reads the Clock sysvar once and writes
  both `open_slot` and `open_unix_ts` (the committed `Clock::unix_timestamp`) onto the batch account —
  the anchor the derivation node's one-sided timestamp drift bound (below) checks every block against.
  `open_unix_ts` is not part of the accumulator (`acc` still binds only the DA bytes: `chain_id ‖ batch ‖
  open_slot ‖ expected_count ‖ root ‖ forced_root`) — it is authoritative because the program wrote it,
  not because it is Merkle-committed. A negative `Clock::unix_timestamp` is refused at the source, before
  any write, so this reading can never be anything other than a real committed clock value. No migration:
  a v1 account is refused (`BadVersion`, or the equivalent named error at each reader's own boundary) by
  the program itself, `zk-inbox-client`, the derivation node, the batcher's resume-anchor walk, and the
  settlement program's `PostRoot`; a devnet carrying v1 batches is reset, never upgraded in place.

### Settlement program (`programs/zk-settlement`)

- `InitChain` creates a chain's root, verifier-registry and per-chain configuration accounts, under two
  namespaces: a reserved range (requires the registry authority's co-signature and a live allow marker)
  and a permissionless range (id derived from the caller's authority and an internal nonce, requires a
  refundable deposit).
- `PostRoot` (unproved; a batch becomes final once its challenge window elapses with no open dispute and
  its predecessor is already final) and `PostRootProved` (a verified proof accompanies the post; the batch
  is final in the same transaction). Both require continuity against the predecessor batch and check the
  inbox program's own accumulator commitment — `PostRoot` reads the inbox batch account's header directly
  to do so, and refuses a v1-shaped one (`SettleError::WrongInboxAccount`) the same as every other reader
  of that header (see the inbox program's header-v2 entry above).
- `FinalizeBatch`, `ClosePending` (rent recycling for a finalized batch's pending account), `RootView`
  (cross-program-invocable; returns only final roots).
- Registration and revenue: `InitGlobalConfig` (authenticated against the program's own upgrade authority
  — no bootstrap key needed), `AllowReservedId` / `RevokeReservedId`, `SetFee`, `SetTreasury`,
  `MigrateChain` (brings a chain that predates this scheme forward into it; now retired and replaced by
  `MigrateChainV2`, below), `RefundDeposit`,
  `ReclaimChain` (reclaims an id that was never posted to, once its window has elapsed and, for a
  reserved id, once its allow marker is revoked), `SetGlobalConfig`, and a two-step
  `ProposeRegistryAuthority` / `AcceptRegistryAuthority` rotation.
- The challenge state machine (non-exclusive per-block disputes, bonds, the forced-inclusion lane) is
  reserved (`RejectBatch` decodes but is not implemented) — not part of this release.
- **Public values v2 (208 B) and `PostRootProved` layout 1.** `rome_zk_layouts::public_values` now defines
  the batch-level, accumulator- and drift-bound public-values struct (`chain_id`, first/last block,
  `open_unix_ts`, `max_drift_secs`, summed `gas_used`, parent/last-block hashes, state root, and both the
  inbox and forced-outcome commitments — all little-endian, no reserved bytes), plus the packing to and
  from ZisK's 64-word `u32` output ABI. `PostRootProved` now has two live layouts, selected by the
  registry entry's `layout_id`: layout 2 (unchanged, single block, header-bound) and layout 1 (new — a
  whole batch range, bound directly to the poster's claimed args, the inbox batch account's own committed
  clock reading, and the chain's drift bound), before the pairing runs in either case. The batch guest
  (`guest-rome`, in the separate guest repository) commits layout 1; see "Batch guest + prover input".
- **`chain_config` v2: `max_drift_secs`.** A new `InitChainV2` (discriminant 24) takes an explicit
  `max_drift_secs` argument (`0` refused) and creates `chain_config` at v2 directly; the original `InitChain`
  (3) keeps the byte shape recorded on the shared standalone node (slot 2554, committed as a fixture and
  decoded by name in a client test) and is refused as `RetiredInstruction`. A new `MigrateChainV2`
  (discriminant 23) brings a v1 account forward in place (realloc 47→55 bytes, preserving every existing
  field) as well as handling the never-existed-before case; a chain already on v2 refuses a second migration.
  The original `MigrateChain` (15) keeps its recorded byte shape and is refused by name (`RetiredInstruction`)
  — a shipped instruction body never changes at its discriminant, so the settlement watcher still decodes
  Tiber's already-recorded `MigrateChain` call and the shared node's `InitChain`. A new `SetDriftBound`
  instruction (registry-authority-only) changes the bound afterward, the same shape as `SetFee`. Reading
  `chain_config` still accepts a v1 account (`max_drift_secs: None`) so every chain's fee-charging path keeps
  working across the upgrade, before `MigrateChainV2` has run for it; only `PostRootProved`'s layout-1 path
  actually requires v2. Every layout-1 refusal carries its own name: `unpack_zisk_outputs` returns
  `LayoutError::BadZiskWord { index }` / `BadZiskTail { index }` (never a borrowed `BadMagic`/`BadVersion`),
  and the program maps them to `BadPublicValuesPacking` / `BadPublicValues` so a refusal log can never confuse
  a malformed publics blob with the pairing's own `InvalidInstructionData`.

### Proof verifier (earlier version, replaced by Veritas; see the Veritas entry above)

- Verifies a ZisK-produced PLONK proof over BN254 using Solana's `alt_bn128` syscalls. Measured on devnet
  at 543,000 compute units per verification.
- The settlement program's proved-post path binds the verified proof to a specific RLP-encoded block
  header (`keccak(header) == the proof's committed hash`) and reads that header's number, parent hash,
  state root and gas used directly. This does **not** yet bind the proof to the inbox program's own
  commitment or to the forced-inclusion lane — see the trust model in
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for exactly what this does and does not prove today.

### Derivation node (`rome-zk-derive`)

- A pull-based, kona-shaped pipeline (`SolanaTraversal` → `InboxRetrieval` → `FrameQueue`/`ChannelBank` →
  `BatchQueue` → `AttributesQueue` → `EngineController`) that reconstructs the EVM chain purely from
  Solana's finalized inbox and settlement state, driving a real, unmodified reth over its Engine API — no
  dependence on the sequencer or any other Rome-operated service.
- Strict validity throughout: a chunk that fails the accumulator's own recomputed commitment, a channel
  that does not reassemble, a transaction that does not decode or names the wrong chain id, or a
  built/consolidated block whose full committed identity (parent hash, timestamp, `prevRandao`, gas
  limit, and the ordered transaction list) diverges from this batch's frame, is a named
  strict-validity failure — the pipeline stops rather than skipping.
- A batch attempt is atomic with respect to the engine controller's own position: a retryable failure
  partway through a multi-block batch rewinds the controller so the retry re-derives from the same
  expected height, consolidating every block that already succeeded instead of rebuilding it.
- Resumes from the settlement program's own finalized root when the engine itself can confirm it (no
  re-derivation of already-settled history — including at the settlement's own genesis sentinel, where a
  fresh chain derives its first batch starting at design block 1, the same real height 1 the sequencer's
  first sealed block always is), falling back to genesis-first consolidation only when no root account
  exists yet; a root the engine cannot confirm (a block it lacks, or a hash mismatch) is a named refusal
  instead of a silent re-derivation. `--from-batch <N>` is an explicit, engine-verified full re-derivation
  override.
- **`EngineController::advance`'s design-premise check is now an equality, not a `+1` offset:**
  `target_height == attrs.env.number` — the sequencer numbers its first sealed block
  1, so a committed block env's design number and the real EVM height it must build coincide directly.
  The settlement-root resume anchor's `last_design_block` follows the same change: `Some(root.number)`
  (no offset) once anything has been derived, `None` at the genesis sentinel (unchanged).
- Enforces the published block environment's one-sided timestamp drift bound (a block's timestamp must
  not run ahead of its batch's committed `open_unix_ts` anchor by more than a configured margin — honest
  posting latency never trips it, including a batch whose DA lands long after it was sealed), reading the
  anchor straight off the batch account (header v2) via `SolanaTraversal`/`BatchRef::open_unix_ts`. A
  negative committed `open_unix_ts` is refused by name ("not a real Clock reading") rather than folded
  into an anchor of 0 — defense in depth alongside the inbox program's own refusal of the same condition
  at the source. Reads a batch's chunks in `getMultipleAccounts`-paged round trips rather than one RPC
  call per chunk — measured read-only against Tiber devnet (batch 4005, 899 chunks): 337.0 s per-chunk
  vs. 2.0 s paged.
- **The drift bound's `max_drift_secs` now comes from the chain's own `chain_config` account, read at
  startup, not this node's own TOML.** `chain_bound::chain_drift_bound` reads
  `chain_config_pda(settlement_program_id, chain_id)` over the same `AccountReader` the settlement-root
  resume anchor uses, at FINALIZED, and refuses by name — never falls back to any default — on: the
  account missing (the chain is not registered under this settlement program), undecodable, a chain-id
  mismatch, a v1 account (names `MigrateChainV2` as the required fix), or `max_drift_secs = 0`
  (unconstructable on chain already; refused here too as defense in depth). `Config::max_drift_secs` is
  now `Option<u64>` with no default: omitted, the chain's value is used; given, it must equal the chain's
  value exactly or startup refuses by name (`reconcile_drift_bound`, naming both values) — so derive, the
  guest (once it commits this input) and the settlement program all enforce the same number, never three
  independently-set copies. Proved against the real, `cargo build-sbf`-compiled `zk-settlement` program:
  `InitChainV2` with `max_drift_secs = 45` → `chain_drift_bound` over a real `BanksClient`-backed
  `AccountReader` returns 45.
- Drives `testing_buildBlockV1` on a stock reth's plain RPC port (forcing this batch's *exact* ordered
  transaction list into the built block) alongside the real, JWT-authenticated Engine API on its
  authenticated port — see [`crates/rome-zk-derive/README.md`](crates/rome-zk-derive/README.md) for the
  two-endpoint config shape and the `--http.api eth,testing` operational requirement this implies.
- **The pipeline-level resume/continuity test fixture builds its settlement-root account
  bytes through `rome_zk_layouts::root::write`**, the real on-chain layout's own encoder, instead of
  hand-writing four byte offsets into a zeroed buffer — every field the fixture doesn't exercise is
  zeroed explicitly through the real `RootFields` struct rather than left implicit in a raw `vec![0u8;
  ..]`.

### Settlement watcher (`rome-zk-settlement-watcher`)

- **Two passes, not one:** `ingest::run_once` pages `getSignaturesForAddress` for the
  inbox and settlement programs (per-request timeout, two-endpoint failover) and writes only tx-level
  facts — `settlement_tx` (sig/slot/status) and `settlement_tx_program` (one row per `(signature,
  program)` an ingester observed it under: `kind`/`ix_kind`/`chain_id`/`batch_id`, nullable when not
  attributable — a transaction touching two programs gets one row from each rather than one
  clobbering the other). `lifecycle::derive_once` is a *separate* pass that reads already-ingested rows
  back in true `(slot, id)` order and applies their lifecycle effects into `inbox_chunk`/`batch` — never
  inline during ingest, whose own arrival order (newest-first across pages during a cold backward walk)
  does not match on-chain chronological order. Both passes decode transactions via
  `zk_inbox_client`/`zk_settlement_client::decode_instruction` (the inverse of every existing `*_ix`
  builder); account positions (which `AccountMeta` slot carries the chunk/batch PDA) come from the new
  `zk_inbox_client::{chunk_account_index, batch_account_index}`, never hard-coded in the watcher.
- **Two-phase cold-start cursor** (`cursor.rs`): the first walk against a multi-million-signature backlog
  commits every fetched page as soon as it is decoded (never buffering more than one page), recording a
  durable `backfill_before` marker after every commit; a crash mid-walk resumes from that marker rather
  than the tip. Rows and the cursor advance share one DB transaction, so a fault partway through a chunk
  commits neither.
- `inbox_chunk` is keyed by `chunk_pda` (the chunk PDA account's own address): `Open` inserts a row; `Seal`
  resolves purely by that account and only ever `UPDATE`s an existing row — a `Seal`-only transaction with no
  prior `Open` is a genuine no-op, never a fabricated row. `batch.finalize_cursor` mirrors
  `programs/zk-inbox/src/batch.rs::finalize_batch_inner` exactly: `FinalizeBatch` (authority-gated) may be
  called with a partial `step`; `status` only becomes `'finalized'` once the cursor reaches `expected_count`.
- `finality::track_finality` upgrades a row's `status` from `confirmed` toward one of two terminal
  states: `finalized` (Solana now reports that confirmation status) or `dropped` (Solana reports no
  status at all for it, and it has been behind the node's own finalized slot for
  `DROPPED_HORIZON_SLOTS`, 150, slots) — a status never moves backward, and a terminal row is never
  re-checked again.
- `block_status` (`migrations/explorer/0001_settlement.sql`): a SQL view deriving `sequenced | data posted
  (confirmed) | data posted | root posted | final | abandoned` at read time from
  `batch`/`settlement_tx`/`root_post` — nothing is stored on a batch row that has to be rewritten at each
  transition. The `(confirmed)` suffix reflects the finalizing transaction's own status; it drops once that
  transaction itself reaches `finalized`. Per-L2-block status is a follow-on once a
  `block_batch`/`l2_block_settlement` table exists; this schema's own grain is `(chain_id, batch_id)`.
- `root`/`acc` on the `batch` table and the whole `root_post`/`proof`/`challenge` tables are reserved,
  populated by later work (root posting, prover, challenger) — they are computed on-chain and stored
  in account state, not carried in any instruction this watcher decodes from transaction history alone.
- `migrate::migrate` applies `migrations/explorer/*.sql`; the crate also ships a runnable binary
  (`cargo run -p rome-zk-settlement-watcher --bin rome-zk-settlement-watcher`) that runs migrations then
  loops ingest + derive + finality on a timer, configured entirely from named, fail-closed environment
  variables.
- Replay-tested against 2,794 real signatures recorded from Tiber devnet's own inbox program (batches 4003,
  4004, 4005 — finalized/abandoned/finalized) and 11 from the settlement program
  (`fixtures/settlement-watcher/tiber-devnet-batches-4003-4005.json`); a watcher killed mid-backlog
  (`FailAfter` in the test harness) resumes from its cursor with no gap or duplicate, and a DB fault mid-
  chunk (`tests/replay.rs`) leaves the committed row count a whole number of commit chunks with the cursor
  never naming an uncommitted signature. Also noted: Tiber's *live* inbox program still predates this repo's
  `Seal { len, body_hash }` shape — every real `Seal` instruction in the fixture is the older 5-byte
  `{ len: u32 }` payload, which this crate correctly fails to decode (and so never sets `sealed`/`body_hash`
  from) rather than misreading; see the crate's README for the redeploy this implies.
- Test Postgres containers self-destruct (`--label rome-zk-test=1`, `exec timeout 600` in the entrypoint,
  ≤ 10 min) and CI's test job force-removes any leaked ones (`if: always()`) regardless of why the job
  ended (a superseded push cancelling the run, a failed assertion) — `make test-containers-clean` does the
  same by hand.
- **Derive reads the feed, never Solana again.** `ingest::write_page` now persists
  each row's already-decoded `ChunkEvent`/`BatchEvent` list as JSON on `settlement_tx_program.events`;
  `lifecycle::derive_once` reads that column instead of a second `get_transaction` call per row — halving
  RPC load and removing the exposure where Agave mapping a transient Bigtable read error to a bare `null`
  could silently lose a row's lifecycle event forever. Ingest itself now retries a transient `null` body
  (bounded, `NULL_BODY_MAX_RETRIES`) rather than skipping the signature.
- **Derive gated on feed completeness; pages never split a slot.** The ingest
  walk-in-progress marker (`backfill_head_sig`/`backfill_head_slot`) is now written in the same DB
  transaction as the first row a walk ever commits; `derive_once`'s row-fetch query joins
  `settlement_cursor` and returns nothing (`DeriveOutcome::IngestWalkInProgress`) while that marker is set
  — an ingest error partway through a backfill can no longer let the derive cursor jump past rows an
  older, not-yet-ingested page will still supply. `ingest::run_once` also buffers a trailing run of
  signatures sharing the oldest slot seen so far across as many page fetches as it takes to observe a
  strictly older one, so a page boundary can never split one Solana slot across two committed units
  (previously possible with a small `rpc_page_size`, inverting `(slot, id)` order for exactly that pair).
- **Batch terminal state is orthogonal to recycling.** `batch.status` stays in
  `{open, finalized, abandoned}` forever; `CloseBatch` sets the new `closed_tx` column (never a fourth
  status value the `block_status` view would need an arm for) and `AbandonBatch` sets the new
  `abandoned_tx` column (never `finalized_tx`, which it used to overwrite). `block_status` gained a
  `data posted (dropped)` state for a finalizing transaction that itself becomes `dropped`. Chunk `Close`
  (rent reclaimed) is now recorded on `inbox_chunk.closed_tx` (a new `ChunkEventKind::Closed`) rather than
  only as an instruction name. `FinalizeBatch { step }` application is idempotent per
  `(batch_pda, settlement_tx_id)` via the new `batch_finalize_step` table, so `derive_cursor` may be reset
  for repair without double-counting a step already applied.
- **Persisted events are versioned and never panic.**
  `settlement_tx_program.events_version SMALLINT NOT NULL DEFAULT 1` sits beside the `events` JSONB; the
  compatibility rule is additive-only (a new field on an existing variant is `#[serde(default)]`, anything
  else bumps the version). `lifecycle::derive_once` decodes with a fallible helper instead of an `expect`
  — an undecodable row (the exact case the batch-terminal-state repair path can produce, re-deriving the oldest rows
  with the newest decoder) is logged by name (`IngestError::UndecodableEvents`) and skipped for that row
  only; the pass keeps making progress rather than crash-looping. Repair: re-ingest the row, then reset
  `derive_cursor`.
- **`block_status`'s provisional suffix now applies uniformly.** The abandoned arm
  reads `abandoned (confirmed)` / `abandoned (dropped)` / `abandoned` exactly the way the finalized arm
  reads `data posted (confirmed)` / `data posted (dropped)` / `data posted` — a `dropped` `AbandonBatch` no
  longer reads identically to a finalized one.
- **RPC failover no longer stops on the first null.** A null `getTransaction` body
  from one endpoint now means "try the next", not "this signature does not exist" — `Ok(None)` is the
  verdict only once every configured endpoint agrees; an endpoint that errors instead of agreeing null
  propagates that error.
- **Account resolution never shifts.** `decode.rs` resolves an instruction's accounts
  into one slot per index, never compacted — an index that does not resolve (an ALT-loaded key, or a
  malformed message) yields `None` for that slot only, never a left-shift that mis-attributes a later,
  real account to an earlier instruction slot. ALT-bearing and CPI-nested inbox instructions stay out of
  scope for this watcher for now.
- `settlement_tx (slot, id)` and `settlement_tx_program (kind, settlement_tx_id)` composite indexes land
  in `migrations/explorer/0001_settlement.sql` ahead of Tiber's real inbox history reaching multi-million
  rows.

### Shared crates

- `rome-zk-executor-api`: `rome-zk-executor-api::BLOCKS_PER_BATCH` renamed to `PREV_RANDAO_EPOCH_BLOCKS` —
  `prev_randao(chain_id, number)` is a pure function of the epoch this fixed constant (10) divides `number`
  into; it takes no chain-profile parameter, so a chain's `[profile].blocks_per_batch` (the batcher's own
  posting cadence — a different quantity) can never change an already-committed block's `prev_randao`.
- `rome-zk-layouts`: byte-exact account layouts, every account's PDA seeds and derivation
  (`seeds(...)`/`pda(...)` next to each layout module — the single definition both on-chain programs and
  both clients now resolve to, directly or through a thin same-name wrapper), the inbox chunk header
  (`chunk`) and the channel/frame header (`frame`), and the accumulator's commitment formula, consumed
  identically on-chain and off-chain. `solana-program` (pinned) is now a dependency, needed for the
  `Pubkey` type `pda` returns; every other function stays free of it.
- **`rome-zk-layouts` pins the five PDAs already deployed on Tiber** (`tests/pda_pins.rs`: root, batch
  cursor, global config, reserved-allow marker and chain config, all for chain id 200101) as string
  literals against the live addresses on that chain. With every account's PDA
  derivation now living in this one crate, the programs' and clients' own delegation tests compare their
  derivation against this crate's — which agrees with a wrong seed exactly as readily as a right one.
  These five tests are the check that catches a seed change that would strand an already-deployed
  account, independent of whether every other consumer still agrees with it.
- `rome-zk-merkle`: this workspace's one `keccak256` (dispatches to the syscall on-chain, `sha3`
  off-chain — the four independent hand-rolled copies this replaces are gone), plus the hash-agnostic
  binary Merkle reduction built on it, shared by both programs and every off-chain reader.
- `rome-zk-pda`: the create-or-adopt PDA-creation helper used at every account-creation site in both
  programs.
- `zk-inbox-client`, `zk-settlement-client`: instruction builders, PDA derivation, account decoders and
  (top-level) instruction decoders for the two programs.
- `rome-zk-testkit` (new, dev-dependency only): the `solana-program-test` fixtures — building a
  `ProgramTest` that loads the real, `cargo build-sbf`-compiled program binary; a rent-exempt lamport
  helper; hand-built `batch_cursor`/root account bytes; a funded keypair; the attacker-prefunds-a-PDA
  griefing primitive — that were previously pasted into eleven separate test files across
  `programs/zk-inbox`, `programs/zk-settlement`, `rome-zk-derive` and `rome-zk-batcher`. Every one of
  those test files now imports this crate instead of redefining its own copy.
- **One CU-read helper, one fixed program id per program.** `rome-zk-testkit::send_measuring_cu` (sends
  a transaction, returns its real, BPF-measured compute units and log messages) replaces the near-identical
  `send`/`send_capturing_cu`/`send_measuring_cu` copies every CU-pinning test file used to define for
  itself; `fixed_inbox_program_id()`/`fixed_settlement_program_id()` replace `Pubkey::new_unique()` as the
  loaded program id in every CU-gate test, so a PDA's bump-seed search depth — and therefore the measured
  CU — no longer varies run to run.
- **`rome-zk-layouts::{batch, cursor, root}` gained write helpers** (`batch::write_header`,
  `cursor::write`, `root::write`), each with a byte-golden test, matching the shape `chunk::write_header`
  already had — so a test fixture builds an account through the layout's own encoder rather than poking
  its offset constants by hand. `rome-zk-derive`'s own test fixtures (`traversal.rs`, `resume.rs`) use them
  now instead of writing `OFF_*` offsets directly.
- **`rome-zk-layouts::batch` header v2**: `HEADER_LEN` 202 → 210, `VERSION` 1 → 2, a new `open_unix_ts i64`
  field (`OFF_OPEN_UNIX_TS` = 202) — the committed Clock reading `OpenBatch` writes alongside `open_slot`.
  `read`/`decode_batch_account` refuse a v1 account by name (`BadVersion`); every caller of
  `account_len`/`leaves_offset`/`bitmap_len` (both programs, `zk-inbox-client`, the batcher's sizing, the
  derivation node) follows the new `HEADER_LEN` automatically, since none of them hardcode the old value.
- `rome-zk-profile` (new): one home for a chain's cadence/cap profile (`Profile`), the identity persisted
  beside its ordered log (`ProfileIdentity`), every design-default constant (50 ms sub-blocks, 20/block,
  10 blocks/batch, 5,000,000 sub-block gas, 70% admission headroom, 60 s timestamp drift bound), and the
  pure `profile.json` read/write
  file layer (`read_profile_json` classifying `Missing`/`V0`/`V1`/`Current`, `write_profile_identity`) —
  no Solana, reth, alloy or tokio dependency. Replaces three previously independent copies:
  `rome-zk-sequencer`'s own `sealer`/`executor` constants (which its `profile` module used to re-read, the
  reverse of the new ownership direction), `rome-zk-batcher`'s inlined legacy-format classification, and
  `rome-zk-derive`'s separately-pinned `DEFAULT_BLOCKS_PER_BATCH`. `rome-zk-sequencer::profile` is now a
  thin re-export of this crate under its historical module path, so no external caller's import changed;
  `rome_zk_sequencer::recovery::reconcile_profile_identity` is the sequencer-specific orchestration
  (migration flag, ordered-log numbering-origin check) built on this crate's read/write primitives.
  `rome-zk-batcher::grouping::grouper_from_profile` is the one library call that turns a `ProfileIdentity`
  into a `SizeCappedGrouper`, shared by both the `--once` and `--follow` binary paths.
- `rome-zk-channel` (new): the channel/frame stream codec (`RLP([blocks]) -> zstd -> frames`, and the
  inverse) and the shadow compressor, moved out of `rome-zk-batcher`'s own `src/channel.rs`.
  `rome-zk-batcher::channel` is now a one-line re-export of this crate under its historical module path,
  so no existing call site changed; `rome-zk-derive` depends on it directly (rather than on the whole
  batcher crate) to decode the same bytes it decoded before.
- `rome-zk-log` (new): the sub-block header (`SubBlockHeader`, moved from `rome-zk-sequencer::header`)
  and the ordered-log record format, segments, torn-tail recovery and tail-follow reader (moved from
  `rome-zk-sequencer::log`). `rome-zk-sequencer::log`/`::header` are now one-line re-exports of this
  crate under their historical module paths. `log_numbering_origin`'s own check moved with the record
  format; `rome_zk_sequencer::recovery::log_numbering_origin` is now a thin wrapper mapping this crate's
  error onto the sequencer's own `RecoveryError::LogNumbering`, for the sequencer's own two call sites (a
  `profile.json` write, replay) — the batcher's anchor resolution calls `rome_zk_log::log_numbering_origin`
  directly now (see the Batcher entry below), and the stored-`first_block` value check reads
  `rome_zk_profile::FIRST_BLOCK` directly. `rome_zk_sequencer::log::RecoveryError` is now
  `rome_zk_log::LogError` (re-exported under that name); variants and messages unchanged.
- **`LogReader` refuses mid-segment or non-newest-segment corruption instead of waiting on it forever.**
  A record whose length prefix decodes outside the sane range, or whose body/CRC bytes are not fully
  present, is only ever a legitimate in-progress append at the tail of the newest segment on disk — an
  older, already rolled-past segment can never grow again, and real data sitting past a bogus length
  prefix proves a writer kept appending after it, which an honest torn write can never do. `read_next` now
  reports that shape as a hard `io::Error` (`InvalidData`) rather than `Ok(None)`, everywhere except the
  newest segment's own tail, where it is unchanged (`replay`'s own classification, which already drew this
  same line via `is_last_segment`, is untouched). The batcher's `source.rs` surfaces this the same way it
  already surfaced a bit-flipped record: `SourceError::Io`.
- **The batcher no longer depends on `rome-zk-sequencer` in production.** `rome-zk-batcher` now depends on
  `rome-zk-log` (the ordered-log reader, the numbering-origin check) and `rome-zk-profile` (the chain's
  cadence defaults, `ProfileIdentity`, `FIRST_BLOCK`) directly; `rome-zk-sequencer` is a dev-dependency
  only, for the cross-crate tests that seal a real `SealerState` end to end. `cargo tree -p rome-zk-batcher
  -e normal` no longer names `rome-zk-sequencer`.
- **`rome-zk-derive`'s chunk header decode goes through `rome-zk-layouts::chunk::read` now**, the same
  decoder the inbox program and both clients use, instead of a field-by-field re-decode of `zk_inbox::OFF_*`
  offsets in `InboxRetrieval::chunks` — one owner for the chunk header's shape, consumed identically by
  every reader.
- **`rome-zk-channel`'s `shadow_compressor_never_exceeds_the_frame_budget` fuzz test runs 5 trials by
  default**; the original 50-trial sweep is `shadow_compressor_never_exceeds_the_frame_budget_wide_sweep`,
  `#[ignore]`d — run it explicitly with `cargo test -p rome-zk-channel -- --ignored`.
- `rome-zk-metrics-http` (new): the minimal, reth-free HTTP/1.1 `GET /metrics` responder — one function,
  `serve(addr, render)` — moved out of `rome-zk-sequencer::metrics`'s own hand-rolled listener, unchanged,
  so `rome-zk-batcher` (which has no reth dependency and should never grow one just to serve a Prometheus
  endpoint) can reuse the identical responder. `rome_zk_sequencer::metrics::serve_metrics` is now a thin
  call into it.
- `rome-zk-solana-sender` (new): the Solana send/confirm machinery (V1 transaction build, the
  crate-generation `compat` conversion boundary, the in-flight bound, batched `getSignatureStatuses`
  polling, block-height-gated resubmit, the retry counter), moved out of `rome-zk-batcher`'s own
  `src/sender.rs`. `rome-zk-batcher::sender` is now a one-line re-export of this crate under its
  historical module path. Its production code depends on neither `rome-zk-batcher` nor `rome-zk-channel`
  — `FramePlan`/`Stage` are built from the 2.1.6-generation `Instruction` type `zk-inbox-client`/
  `zk-settlement-client` already produce, never `channel::Frame` — so the prover orchestrator's
  `PostRootProved` poster, the challenger, and governance tooling can depend on just this crate.

### Not yet built

The following crates exist as scaffolding only — a short module doc saying what they will do, and no
behavior: `rome-zk-challenger` (the challenge-flow client), `rome-zk-indexer`, and
`rome-zk-explorer-api` (the explorer's read API).

### Operations

- A persistent Solana-devnet-settled chain (Tiber, chain id 200101) that every merged change deploys to. The
  render-config Make target (a script in the Tiber deploy directory) renders the
  sequencer's `sequencer-config.toml` (gitignored, no secret) from `crates/rome-zk-sequencer/config.example.toml` —
  the sequencer start target runs it automatically before scp'ing; the signing key stays provisioned separately in
  the secret store.
- A dedicated build machine for the workspace's heavier build and test jobs.
- The `fmt` job's `Cargo.lock is current` step (`cargo metadata --locked`) is now a required check on
  every push, so a `Cargo.lock` that doesn't resolve with `--locked` can no longer merge — the only other
  consumer of `--locked` is the image build (`Dockerfile` `cargo build --release --locked`), which runs
  only on push to main and would otherwise be the sole place a stale lock surfaces.
- **Lib-only builds are gated in CI.** `cargo test --workspace`/`cargo clippy --workspace --all-targets`
  unify every crate's `[dev-dependencies]` features into the resolved graph, which can mask a production
  dependency missing a feature its own lib path needs — `rome-zk-derive`'s `alloy-consensus` was one
  normal dependency short of `k256`, so `recover_signer` compiled fine under those two gates but failed
  standalone (`cargo check -p rome-zk-derive`), exactly the shape a lib-only image build like
  `Dockerfile`'s hits. The `clippy` job now runs `cargo check --locked -p rome-zk-derive -p
  rome-zk-batcher -p rome-zk-sequencer` (no dev-dependencies in the graph) before its usual clippy pass,
  one crate per invocation -- selecting several packages together unifies their normal-dependency
  features too, which would hand a crate a feature it does not declare.
- **One image, three binaries; Tiber's settlement services, scripted registration, extended check.** The root
  `Dockerfile` now builds `rome-zk-sequencer`, `rome-zk-batcher` and `rome-zk-derive` in one `cargo build --release
  --locked --bins` invocation and copies all three into the runtime image (renamed to a single repository-wide
  image, same tags — sha + `main` — from the same CI `docker` job); the sequencer stays the image's default
  `ENTRYPOINT`, and a compose override selects the other two. The Tiber deploy directory's
  `docker-compose.settlement.yml` layers three services on top of the sequencer: a second, independent
  `reth-verifier` (stock reth, following via the Engine API, never `--dev`-mining its own blocks), `derive` (the
  derivation client, driving `reth-verifier` purely from the Solana inbox) and `batcher` (`--follow`, posting the
  sequencer's ordered log). Their configs are rendered from `programs.json` by the settlement config render script
  (through the Tiber render-settlement-config, settlement-up and settlement-down Make targets) — no literal program
  id in any newly-committed file. The Tiber chain registration script (`--dry-run` / `--confirm`, fails
  closed given neither or both) drives `zk-settlement-client`'s `register_chain` example and prints the five chain
  PDAs in both the README table's Markdown shape and `pda_pins.rs`'s Rust-literal shape (`rome-zk-layouts`'s new
  `print_pdas` example) so a chain reset moves both together. The Tiber
  health check gained two checks: batches advancing
  (`batch_cursor.next_batch` increasing across two 60 s-apart samples) and verifier head == sequencer head
  (`reth-verifier`'s `eth_blockNumber`, reached over a tunnelled ssh connection since its RPC is loopback-only,
  within one `blocks_per_batch` of the sequencer's own).

### Executor

- **A torn persist across Reth's two storage layers heals at open instead of refusing to start.** Reth writes
  static data (fsync) → static config (rename) → MDBX (commit); a kill between the first two steps leaves one
  header row on disk that MDBX never committed, and NippyJar's cursor then decodes the previous row's hash
  column wrongly — the "canonical head header missing" refusal of a Tiber incident. `RethExecutor::new` now
  inspects any dangling Headers row before Reth's `ProviderFactory::check_consistency` truncates it: a row
  MDBX never committed is refused by name (`HeaderNumbersMismatch`) and Reth's truncation repairs the segment;
  a static config ahead of MDBX is pruned by Reth's own invariant check; a row MDBX did commit (not a crash
  shape — defence in depth) is verified four ways (`static_heal`) and re-committed forward. In every case the
  executor opens at the block every layer agrees on, the ordered log replays the torn block, and the sequencer
  seals on — proven for both shapes, with and without transactions, against a never-torn control
  (`reth_torn_persist_recovery.rs`, `reth_torn_persist_resume.rs`). Residual gap: several dangling rows, or a
  row refused for another reason, leave Reth's state un-unwound and a later replay refuses loudly
  (`ReplayDiverged`, or `UnexpectedStaticFileBlockNumber` at the first live persist); repair = drop the Reth
  datadir, the log rebuilds it. `healed_to <= best_block_number()` is asserted on every path. Operator tool:
  `examples/inspect_datadir` (read-only on an existing datadir; refuses to create one).

### Operations

- The Tiber genesis render script renders the genesis gas limit from the sequencer profile; the deploy target
  exports it as `BLOCK_GAS_LIMIT` for compose; the verifier reth runs `--builder.gaslimit` at that figure and serves
  reth's `testing` namespace on its loopback-only port (the derivation node's block-build call).
- **The settlement-up target no longer depends on the full deploy target.** A new push-files target holds just
  the config-render-and-scp half of what the deploy target used to do in one step; the deploy target (which
  recreates the support services caddy/otterscan/prometheus/grafana and the database proxy) now runs the
  push-files target plus its own recreate step, and the settlement-up target runs the push-files target
  directly instead — so bringing up the settlement services never again force-recreates the live sequencer as
  a side effect. The settlement services themselves now come up with `--no-deps`, so compose never implicitly
  starts/recreates `reth` just because `batcher`'s own `depends_on: reth` names it. The Makefile target
  test in the Tiber deploy tests proves both, via `make -n`, against the real Makefile and a mutated copy.

### `rome-zk-prover` — Postgres job history

- **A `Store` seam, the follower's fifth dependency alongside fetch/prover/sender/verifier, records
  one event at every job transition** (Queued, InputBuilt, Proving, Proved, Verified, Posted,
  AlreadyPosted, Superseded, Finalized, or the loop's own halting Failed) into Postgres. The chain
  stays the loop's only cursor: `Store` has no `get`/`query` method at all, so nothing in the follower
  can read history back even by accident — proven by a test that preloads the store with a stale claim
  ("batch 7 already posted") while the real chain's own head is 3, and shows the follower still proves
  batch 4 with zero reads against the store.
- **Two implementations.** `NoopStore` (no `database_url` configured, the default) drops every event.
  `PgStore` writes to Postgres via `sqlx`, applying a new migration (`proof_jobs`, keyed
  `(chain_id, batch, attempt)`; `proofs`, the winning attempt's own 768-byte proof + 512-byte publics)
  once at connect time. Connecting or migrating failing at start is a named, fail-closed refusal
  (`HistoryUnreachable`) — an operator who configured `database_url` never runs silently with no
  history. Once running, a `record` failure is logged and counted
  (`rome_zk_prover_store_errors_total`) but never allowed to stop a batch from proving, proven by a
  store whose every call fails while the same five-batch scenario still posts all five. The database
  URL is never logged unredacted.
- **A job is `Finalized` in history the moment it is actually final on chain**, whichever instruction
  made it so: `PostRootProved` itself advances `head_final_batch` in the same instruction whenever a
  batch posts in strict order ("posted+proved (final immediately)"), the only path this crate's
  product shape takes today, so the common case attributes `Finalized` to the post's own signature
  rather than waiting on a separate `FinalizeBatch` send that never happens.
- The CLI builds `PgStore`/`NoopStore` from `Config.database_url` at start; the deploy template
  already rendered that field with nothing to change.

### `rome-zk-prover` — history records the real attempt, failed attempts say so, restarts never strand a row

- **A resumed job's history follows its own real winning attempt, never a placeholder.** The prover-input
  sidecar's provenance now carries the attempt it was built under; a resumed job reads that attempt back
  and records every event (`Queued`, `Verified`, `Posted`, `Finalized`) against it, so a restart's history
  lands on the SAME `(chain_id, batch, attempt)` row the original run already used instead of stranding
  the real winning attempt's history at `verified` while a fabricated row for a different attempt ends up
  `finalized`. The resumed job's own `Verified` event also carries the resume gate's real re-verify wall
  time (the same pairing check it already ran), never a fabricated zero that would silently overwrite an
  already-recorded measurement.
- **A freshly produced attempt that fails records `Failed` before retrying**, at that same attempt —
  previously the attempt's own row simply froze at whatever transition last succeeded (`Proving`/
  `Proved`), never showing that it actually failed.
- **A restart onto a candidate that is already posted AND already final** — a preemption between the
  on-chain send confirming and this crate's own finalize-history bookkeeping — records `Finalized` with no
  signature rather than leaving the row stuck at `posted` forever: the chain is the source of the fact,
  the signature that made it so is not always locally known. `finalize_sig` is nullable to carry this.
- **The documented "latest job" query now orders by `updated_at DESC`, never `attempt DESC`**: a batch's
  real prove attempts restart from 1 on every process restart, so a stale, higher-numbered failed attempt
  from a previous life can otherwise outrank a genuinely later, lower-numbered success.
- The devnet Postgres database's actual name is now documented alongside the design's generic name,
  `rome_zk_prover`, everywhere the two previously disagreed. The prover's own Postgres test fixture password is a
  labeled non-secret (`fixture-not-a-secret`), never a real-looking credential.

### `rome-zk-sequencer` — an idle chain seals nothing (`empty_block_interval_secs`)

- **A tick with no ready transactions writes nothing at all, by default.** `[profile]` gains
  `empty_block_interval_secs` (0, the default: never seal a block with no transactions; N: seal one at
  most every N seconds instead, for a chain that wants a regular timestamp even while idle). The idle rule
  is checked before any state mutation — no log record, no call into the execution engine, no change to
  the sealer's own internal trackers — so a quiet chain sits idle indefinitely rather than sealing an
  empty block every 50 ms tick, which is what every chain on this sequencer used to do unconditionally.
  Once a block has actually opened (its first sub-block sealed, empty or not), every later sub-block in it
  still seals regardless of content — the idle gate only ever decides whether a NEW block opens.
- **The first real transaction after any idle gap opens its block at the wall clock**, never at a value
  derived from how long the chain sat idle — the existing monotonicity trackers (`prev_sub_block_ts_us`,
  `prev_block_timestamp_secs`) are simply left untouched by every idle tick, so they are exactly as stale
  as they were before the gap started, and the wall clock wins the very same `max(first, prev + 1)`
  comparison every block's timestamp already went through.
- **New metrics**: `rome_zk_sequencer_idle_ticks_total` (a quiet chain's liveness signal), `block_gas_used`
  and `block_timestamp_ahead_seconds` (per sealed block — the gas/s and drift-bound measurements a later
  load programme needs a source for).
- The knob lives on `Profile` only, never on the persisted `ProfileIdentity` beside the ordered log — it
  is the sequencer's own cadence choice, not a value the batcher or derivation node need to agree with it
  on. Loading refuses a nonzero interval below the profile's own block time, naming the bound. An empty
  block, when one is sealed at a nonzero interval, is built through the exact same block-open path as any
  other block — same committed environment, same header rule — so it is still a full-length block whose
  header the guest and derivation already accept unchanged; nothing about DA, derivation, or the guest
  changed in this release.

### `rome-zk-batcher` — a partial batch closes by age (`batch_close_after_secs`)

- **A group that never fills now posts on a bounded cadence instead of waiting forever.** A new config
  knob, `batch_close_after_secs` (default 60 s, refused at 0 and, once the sequencer's `profile.json` is
  read, below this chain's own block time), closes a partial group once it has held its first block that
  long — through the exact same posting call a full or size-closed group uses, not a separate path. An
  empty group never closes this way, however long it has been since the last one did.
- **The clock is this process's own receipt instant, never a block's own on-chain timestamp.** Comparing
  against a block's own timestamp would close a *loaded* batch's group before it ever filled (its last
  block is already old enough by the time this process gets around to reading it), and a batcher host clock
  running ahead of the chain's own would shrink every group toward a single block — both are now
  unconstructable rather than merely unlikely, since the age check has no access to any block's timestamp
  at all.
- **`--follow` checks this on every loop tick, block or not; `--once` never checks it.** A trickle of
  blocks arriving slower than the block-count cap still closes on schedule, because the check runs whether
  a new block just arrived or the log went quiet, not only while idle. `--once` reads the whole log and
  posts its own trailing partial group once, at the end, exactly as it always has — nothing about that
  path changed.
- **New metrics**: `groups_closed_total{reason=cap|size|age}` (a quiet chain shows no increments at all),
  `oldest_unposted_block_age_seconds` (gauge, 0 whenever nothing is unposted), and
  `block_age_on_arrival_seconds` (histogram: this process's wall clock minus a block's own timestamp at
  read time — observability only, never a close decision; a restart resuming into a backlog legitimately
  shows a large value here).
- Tiber's own rendered config and its committed doc-example both carry the new knob at its 60 s default.
  This change does not touch the rendered check script, the Prometheus job wiring, or the sequencer
  container's own log level.

### `rome-zk-derive` serves metrics; the drift bound is proved to hold across an idle gap; the batcher measures Solana clock skew

- **`rome-zk-derive` gains a Prometheus registry served on `GET /metrics`** (`metrics_addr`, default
  `127.0.0.1:9003`, env override `ROME_ZK_DERIVE_METRICS_ADDR`) — the same reth-free HTTP responder the
  batcher and sequencer already serve. Five names: `rome_zk_derive_batches_derived_total` (counter),
  `rome_zk_derive_critical_total` (counter — every `PipelineError::Critical` this node raises, from
  whichever stage), `rome_zk_derive_last_batch` and `rome_zk_derive_head_block` (gauges), and
  `rome_zk_derive_batch_seconds` (histogram, wall-clock time to derive one batch).
- **The one-sided timestamp drift bound is proved to hold across an idle gap of any length**, not merely
  asserted: a block sealed a full hour after the one before it, with its own batch's `open_unix_ts`
  written fresh after the gap (never against the previous block), derives cleanly — and a block that
  genuinely exceeds the bound is still refused, exactly as before. No change to the bound's own logic or
  value. A companion test against a real, unmodified reth v2.5.2 node proves the same one-hour gap
  executes and seals identically to any other block — `block.timestamp > parent.timestamp` places no
  upper bound on the gap, only a lower one.
- **`rome-zk-batcher` gains `rome_zk_batcher_solana_clock_skew_seconds`**, observed once per `OpenBatch`:
  the just-opened batch account's own committed `open_unix_ts` minus this process's own unix wall clock
  captured immediately before the send. Signed, buckets past ±60 s both sides. Observability only — never
  a decision input; a read or decode failure here is logged and skipped, never allowed to affect whether
  the `OpenBatch` itself is considered successful.
- This release only adds the config field and the endpoint. Derive's compose-network `expose` entry, the
  rendered `derive.toml`'s `metrics_addr`, and the Prometheus `derive` scrape job (target `derive:9003`) came
  in a separate deploy change (see "Tiber ops — the idle knobs and derive's `/metrics` reach the deploy
  surface" below).

### Tiber ops — the idle knobs and derive's `/metrics` reach the deploy surface

- **`EMPTY_BLOCK_INTERVAL_SECS` and `BATCH_CLOSE_AFTER_SECS` move from a hard-coded template literal to the Tiber
  env file**, rendered in by the config render script (into `sequencer-config.toml`'s `[profile]`) and the
  settlement config render script (into `batcher.settlement.toml`). Both refuse by name
  (`EmptyBlockIntervalMissing`, `BatchCloseAfterMissing`) when the env file is missing or lacks the key — never a
  silent fall back to the source template's own literal. Ops values are runtime parameters, the same rule
  `SOLANA_RPC_URL`'s own fail-closed render already follows.
- **`derive.toml` gains a fixed `metrics_addr = "0.0.0.0:9003"`** (the container binds every interface on
  its own compose network; nothing needs to vary this per render), `docker-compose.settlement.yml`'s
  `derive` service gains a compose-network-only `expose: "9003"` (same rule as the batcher's own 9002 —
  never a host port mapping), and `prometheus.yml` gains an unconditional `derive` job on `derive:9003`
  (the service always exists, unlike the prover job). The sequencer's own compose service
  (`docker-compose.sequencer.yml`) gains `RUST_LOG=info` — it emitted nothing on stdout before this.
- **Tiber health check's check 8 ("batches advancing") is retired**
  — an idle chain under the knobs above
  correctly posts nothing, so that check would fail forever on a healthy chain. Replaced by **8a**
  (`rome_zk_batcher_oldest_unposted_block_age_seconds`, via Prometheus, bounded at
  `BATCH_CLOSE_AFTER_SECS + 60` s — 0 on an idle chain, FAIL by name if the metric is absent) and **8b**
  (`cursor.next_batch − 1 − root.head_final_batch`, decoded directly from the two on-chain accounts the
  README's own PDA table pins, bounded at 2). The retired check's line and number stay reserved, never reused.
- Two crate tests pin the render output against real config parsing, so a shape drift breaks CI rather than only a
  shell-level check: `rome-zk-derive`'s deploy-fixture test now reads back the rendered `metrics_addr`, and a new
  `rome-zk-profile` test proves the exact sequencer `config.example.toml` `[profile]` table validates through
  `Profile::validate` after the config render script's own substitution.
- **No live Tiber resource changes with this release** — recreating the sequencer/batcher/derive
  containers on an image built from it is the operator's own step.

### Tiber ops — the recreate steps and the check script are true against the live chain

The previous release's recreate steps, walked against the real `make -n` recipes and the
live chain's own accounts read-only, had every check and every step below either red on
arrival or describing a step that does not run as written. This release closes that gap; **no live
Tiber resource changes with this release** — the recreate itself stays the operator's own step.

- **Tiber health check's check 2 no longer requires
  `eth_blockNumber` to advance.** An idle chain
  (`EMPTY_BLOCK_INTERVAL_SECS=0`) never advances the head — that used to fail this check forever. It now
  also accepts the sequencer's own `rome_zk_sequencer_idle_ticks_total` counter having increased over the
  last minute, and only fails by name (`SequencerStalled`) when neither signal moves.
- **Tiber health check's check 2 reads the idle signal as one
  Prometheus range query, `increase(...[1m])`.** The
  first cut sampled the raw counter twice, 3 s apart, through Prometheus — whose 15 s scrape returns the
  same sample to both reads four times in five, so the live recreate walk read a sequencer
  ticking ~20 times a second as `SequencerStalled: idle_ticks 1615 -> 1615`. The test stub now answers
  the raw counter with one fixed value on both reads to model exactly that aliasing.
- **Tiber health check's check 8b is gated behind the same "does a prover
  instance exist" guard as checks 10-11.** With no
  prover host in dev outside the end-of-build soak, nothing ever finalizes a batch, so the settlement lag only grows
  — that is structural, not a check failure. 8b now SKIPs without an instance and evaluates for real once one
  exists, instead of failing on every run in the meantime.
- **Tiber health check's check 8b reads its two PDAs from
  `programs.json` via `cargo run -p rome-zk-layouts
  --example print_pdas`** — the same derivation the README's own "PDAs" table is generated from — rather
  than grepping that Markdown table, which drifts silently the moment the table is not repasted at a
  chain reset. A lookup that finds neither PDA now fails by name instead of the script exiting early
  under `set -euo pipefail` before it could report anything.
- **Tiber health check's check 8a refuses by name
  (`BatchCloseAfterUnset`) when `BATCH_CLOSE_AFTER_SECS` is unset**
  in the sourced env, rather than silently assuming a 60-second bound — an operator who forgot to set the
  knob and one who deliberately chose 60 must not read identically. The PASS line now also states the
  bound and where it came from.
- **New check 4c**: the Prometheus target `job=derive` is up, mirroring 4b. This is what actually proves
  the `derive` scrape job added in the previous release is being scraped, not just rendered — Prometheus
  bind-mounts `prometheus.yml` read-only, so a config change needs Prometheus itself recreated to load it.
- **The recreate steps are rewritten against the real recipes**, in three `make` invocations rather than five
  loosely-described steps: a new step 0 covers a box whose Tiber env file predates the idle knobs; `SOLANA_RPC_URL`
  is now shown on every push-files, deploy and settlement-up line, with its own footgun stated plainly (an omitted
  value does not refuse, it silently renders the secret store's public-devnet endpoint — a parked gap, tracked, not
  fixed here); the recreate step now force-recreates Prometheus alongside the settlement trio, which the previous
  wording omitted; the expected check-script output is written out check by check for this chain's actual state (8b
  and 10-11 SKIP, check 2 PASSes via the idle-tick branch, 4c PASSes) rather than a blanket "ALL PASS".
- **The push-files target's remote tar extract carries `--no-same-owner --no-same-permissions`** (the same
  flags and rationale the prover deploy's own push already uses) and re-applies `chown 999:999`/`chmod 0400`
  to `jwt.hex` after the extract, so a bare push with no settlement-up run after it can no longer
  silently leave `jwt.hex` at the operator's own local mode and owner.
- **Two crate tests read the real render surface instead of a hand-mirrored fixture**: `rome-zk-derive` gains a test
  that reads the Tiber deploy directory's `derive.toml.template` directly, applies the settlement config render
  script's own placeholder substitution, and loads the result through `Config::load`; `rome-zk-profile` gains a test
  that applies the config render script's own sed substitution to the real `config.example.toml` with a non-default
  value and asserts it landed, plus a below-block-time value refusing by name (`EmptyBlockIntervalBelowBlockTime`).
  The previous release's fixture tests stay (they still prove the deploy-time fixture parses); these are additional
  coverage of the substitution itself, which a hand-mirrored fixture cannot catch drifting.

### `contracts/exit-portal` — the L2 exit portal, fixtures, and the greenfield predeploy

New Solidity project (`contracts/exit-portal`, foundry, `solc 0.8.28` / `evm_version cancun` / optimizer
200 runs / no bytecode metadata — a byte-reproducible runtime): `RomeExitPortal.initiateExit(bytes32
solRecipient)` refuses a zero amount, an amount over `type(uint128).max`, and a zero recipient (cheapest
check first, custom errors), then records `sentMessages[messageHash] = true` at storage slot 0 and emits
`ExitInitiated` with the message's own fields — a 160-byte preimage this document's "Exit message" row
pins. ETH stays on L2 (no `withdraw`, no `receive`/`fallback`); a later `ProveExit` instruction proves the
message's inclusion against a finalized `state_root` via an Ethereum Merkle-Patricia proof and releases
the twin asset from an off-chain Solana vault.

Fixtures (`fixtures/exit/*` + `contracts/exit-portal/RomeExitPortal.runtime.hex`) come from a single named producer
— `ghcr.io/foundry-rs/foundry:v1.3.6` — never the operator's own forge/anvil; regenerate with `make exit-fixtures`.
A greenfield chain's `genesis.json` (via the Tiber genesis render script) predeploys the portal's committed runtime
bytecode at `0x4200000000000000000000000000000000000016`, refusing by name (`ExitPortalRuntimeMissing`) if that file
is missing or not valid hex. The Tiber exit-portal deploy script (`--dry-run` / `--confirm`) deploys the same
contract to Tiber without a chain reset — the deployer key never touches argv or disk, passed to the container only
as an environment variable the forge script reads via `vm.envUint("PRIVATE_KEY")`.

CI gained a `contracts` job (self-hosted, the pinned image): build + test + a runtime-hex
reproducibility check, then a fixture regeneration + drift check (`git diff --exit-code`) — every byte
reproduces except `anvil_state_root.json`'s own `block_hash` (anvil's per-block `mixHash`/`prevRandao`
has no CLI pin in this version; `state_root`/`number`, the fields the exit-proof pipeline actually uses,
are checked explicitly instead — see `fixtures/exit/README.md`).

**Deployer key and genesis guard:**

- **The deployer key now survives `sudo` and forge's own parser.** The
  exit-portal deploy script with `--confirm` ran
  the container as `--user root` under plain `sudo`; plain `sudo`'s `env_reset` default silently dropped
  `PRIVATE_KEY` before docker ever saw it (verified: `<UNSET>` inside the container). It now runs as the
  invoking user (`--user "$(id -u):$(id -g)" -e HOME=/tmp` — the CI `contracts` job's own shape) and wraps
  with `sudo --preserve-env=PRIVATE_KEY` only when the invoking user cannot reach the docker socket
  directly (the Docker invocation helper in the Tiber deploy directory).
  Inside the container, the exit portal deploy script now
  normalises the key to a `0x`-prefixed form before invoking `forge` — the secret-store value is
  `openssl rand -hex 32` (no prefix), and `vm.envUint` refuses that shape with `missing hex prefix
  ("0x")`.
- **A routine push can no longer replace Tiber's live genesis.** The genesis render script refuses by name
  (`GenesisDrift`) when its output path already exists and the render would change it, printing which top-level
  fields / alloc keys differ rather than the whole file, and leaves the existing file untouched; an identical
  re-render is a no-op (exit 0). (The push path then needed an explicit keep mode, below — the live genesis
  legitimately differs from a fresh render now.)
- `make exit-fixtures` and `contracts/exit-portal/README.md`'s "Build and test" now use the same
  invoking-user shape as the CI job (`--user "$(id -u):$(id -g)" -e HOME=/tmp`, no `--user root`, no
  `chown` cleanup after) — plain `sudo` is fine there since nothing it needs is read back out of the
  caller's own environment.

**Genesis keep mode and test fixes:**

- **The push-files Make target keeps the live genesis and continues; the reset target renders fresh.** The earlier
  `GenesisDrift` guard was correct about never rewriting the file but wrong about the push: the tracked
  `genesis.json` in the Tiber deploy directory (Tiber: dev account only) now differs from a fresh render (the portal
  predeploy), so every routine push — and the reset, which never deleted the file — aborted on the recipe's first
  line. The genesis render script's `--keep-existing` (what the push-files target passes) keeps the file
  byte-for-byte, still writes the gas-limit sidecar, prints one line (`differs in: alloc[0x42…16]`) and exits 0; a
  bare run still refuses by name; an unknown flag is refused by name (`UnknownArgument`). The reset target removes
  `genesis.json` before the deploy target. Proved with the tracked file
  itself in the genesis render test (Tiber deploy
  tests) and `make -n` in the Makefile target
  test, each guard mutated red.
- **The exit-portal deploy script builds ONE docker argv** for the
  dry-run text and the `--confirm` exec (the
  two were hand-typed copies; mutating the real line alone was invisible to every test). The env test now
  runs `--confirm` end to end through stub cloud CLI/`sudo`/`docker` and turns red when the confirm line
  loses `--preserve-env`. The empty-prefix expansion is bash-3.2-safe (`${arr[@]+"${arr[@]}"}`).
- **`docker_needs_sudo` is tested without its override** (stub `id`: member → no sudo; non-member and a
  `dockerd`-prefixed group → sudo) and matches the group list as whole words with no external pipeline.
- The genesis render test now also renders against the real, committed
  `contracts/exit-portal/RomeExitPortal.runtime.hex` (not only its own fast stub hex) and asserts the
  portal's alloc entry equals that file byte-for-byte; the stub-hex assertion's label no longer
  misdescribes the stub as "the committed hex".
- `fixtures/exit/README.md` cites the fixture image's commit id (`d2415887096b10226d13af9240b5bef5e6b0d815`)
  as read directly off the image's own `forge --version` output, not an unsourced release-notes guess.

### `rome-zk-mpt` + `rome-zk-layouts::exit` — bounded Merkle-Patricia proof verifier, exit layouts

New crate `rome-zk-mpt`: `verify_account`/`verify_storage` prove an account's fields (or a storage slot's
value, or its provable absence) against a proof-bound `state_root`/`storage_root` via an Ethereum
Merkle-Patricia proof. Bounds — `MAX_NODES = 64`, `MAX_NODE_BYTES = 532` — are checked before any hashing or
RLP decoding; every hash goes through `rome-zk-merkle::keccak256` (the one keccak owner), never a
locally-linked dependency. `verify_account` has no exclusion outcome (its only caller always asks about a
portal address it already trusts to exist); `verify_storage`'s `StorageValue::Absent` is a distinct, explicit
type-level outcome from `Present([0u8; 32])`. Its own bounded RLP decoder (`src/rlp.rs`), not `alloy-rlp` —
the crate's contract is bound-first and purpose-built for the four fixed trie-node shapes, so a small decoder
audits more directly than wrapping a general one and re-imposing the same bounds afterward. Embedded
("inline") children — a node whose own encoding is under 32 bytes, embedded directly in its parent rather than
referenced by hash — are followed with no separate hash check, proved against a small, deterministic,
hand-built trie (`inline_child_is_followed`) rather than relying on any real fixture to happen to contain one.
`ExitProof` (borsh: `account_nodes`/`storage_nodes`) is the wire shape `ProveExit` carries (see the
`ProveExit` entry); `byte_len()` returns its exact serialized size for the V1 envelope budget. No
`solana-program` dependency at all — this crate needs no `Pubkey` — so unlike `rome-zk-layouts` it has no
`solana` Cargo feature to gate; `cargo build-sbf` proves the SBF target compiles with the syscall keccak path
active via `rome-zk-merkle`'s own target gating.

`rome-zk-layouts` gains an `exit` module: `ExitMessage`'s 160-byte ABI preimage/hash/storage-slot, pinned
byte-for-byte against the Solidity fixture (`fixtures/exit/message_hash.json`); four new account layouts —
`exit_config` (142 B), `exit_record` (169 B), `exit_window` (36 B), `exit_nullifier` (1,044 B, a 1,024-byte
persistent replay bitmap covering 8,192 exits/page); nullifier page/bit math
(`nullifier_page(nonce) = nonce >> 13`) with a `NullifierPageMismatch` refusal that keeps a caller from
touching the wrong page's copy of a bit; `cap_units`, converting a wei amount to whole gwei units, rounding
up. `root.rs`'s doc comment now records what its existing `exit_cap_per_window`/`poster_bond` fields count in
(gwei of the native asset per window; lamports) — no byte offset changed. `tests/pda_pins.rs` gains four
exit-account PDAs derived under Tiber's live settlement program id but not yet sent anywhere — pinned so a
seed-order regression is caught here rather than only when `ProposeExitConfig`/`ProveExit` run on Tiber.

Fixtures: `fixtures/exit/tiber_eoa_getProof.json` + `tiber_eoa_block.json`, copied verbatim from a
read-only capture of the real Tiber verifier (an EOA, one account node, 120 B — the
whole trie is a single account). `contracts/exit-portal/script/fixtures.sh` gained a second scenario, on a
second, independent anvil instance (so the first scenario's own files stay byte-identical): the same
portal contract after 20 `initiateExit` calls, producing `anvil_getProof_multi.json` (an inclusion proof
of exit 7's storage slot and an exclusion proof of a never-sent nonce, in one `eth_getProof` call),
`anvil_state_root_multi.json`, `message_hash_multi.json` (all 20 messages' hashes/slots). The 20-key
trie's actual node-kind composition is measured, not assumed
(`multi_fixture_node_kinds_are_measured_not_assumed`): branch and leaf nodes only — no two of these 20
sparse `keccak256`-derived keys share a long enough common prefix to need an extension node, and no node
here is small enough to embed inline. CI's `contracts` job drift check now also accounts for the second
anvil instance's own non-deterministic `block_hash` the same way it already did for the first.

### `rome-zk-mpt` — committed regression coverage for the extension-node walk and value-shape guards, a canonical-form guard

The extension-node arm of the trie walk (the classic forge vector — a forged proof whose path skips an
extension's shared nibbles) now has committed coverage: `contracts/exit-portal/script/fixtures.sh` gained
a third scenario, on a third anvil instance, 500 `initiateExit` calls deep — enough to reliably contain
real Extension nodes, unlike the 20-key `multi` scenario. `anvil_getProof_ext.json` (+
`anvil_state_root_ext.json` + `message_hash_ext.json`) carries an inclusion proof that walks *through* a
real Extension node to a `Present(1)` leaf and an exclusion proof that *diverges at* one, both asserted
against measured node kinds, not assumed (`ext_fixture_has_an_extension_node`,
`inclusion_through_extension_verifies`, `exclusion_diverging_inside_extension_is_absent`). A hand-built,
hash-consistent 3-nibble extension node additionally covers divergence strictly inside the path, a key
shorter than the path (proving the length guard runs before any out-of-bounds slice), and a full match
into a child the proof does not carry (`hand_built_extension_divergence_and_missing_child`).

The defensive value-shape guards in `decode_storage_value`/`decode_account_rlp` — an over-length storage
value, and every adversarial account RLP shape (oversized nonce/balance, a wrong-length storage root or
code hash, a 3-item list, a nested scalar) — now have hand-built, hash-consistent single-leaf trie tests
(`storage_value_over_32_bytes_is_refused`, `account_rlp_adversarial_refused`), plus a positive case
confirming a non-canonical leading-zero storage value still decodes to the correct left-padded `Present(1)`
(`leading_zero_storage_value_decodes_to_one`).

`nibbles::decode_compact_path` now refuses a non-canonical even-path hex-prefix byte whose padding nibble
is nonzero, rather than silently discarding it — belt-and-suspenders canonical-form enforcement (every
node stays keccak-bound to a trusted root regardless, so this closes a laxity, not a soundness gap).

### `ProposeExitConfig` (26) + `ActivateExitConfig` (27) — exit-config governance

Two new settlement instructions govern a chain's `exit_config` PDA (defined earlier, first written here) and,
through it, the root's exit cap and poster bond. `ProposeExitConfig`, signed by the chain authority
(`root.authority`), proposes any subset of `exit_portal`/`bridge_program`/`exit_cap_per_window`/
`poster_bond` together with a required `activation_slot` — the delay must be at least one full challenge
window (`activation_slot >= Clock::slot + root.challenge_window_slots`, no bootstrap exception), and a
chain whose `challenge_window_slots == 0` cannot propose at all (fail-closed: a chain with no window can
never configure exits). Only one proposal may be in flight per chain; a supplied portal or bridge program
may never be the zero sentinel. The instruction writes only `exit_config`'s PENDING fields and the mask —
the CURRENT `exit_portal`/`bridge_program` stay untouched, so a proposal is invisible to a later `ProveExit`
until it is activated. `ActivateExitConfig` is permissionless: once `Clock::slot` reaches the activation
slot, anyone may call it to copy the pending portal/bridge-program into `exit_config`'s current fields and
the pending cap/bond into the ROOT account's own `exit_cap_per_window`/`poster_bond` (the root stays the
one source of truth for both numbers; neither account's byte layout changes), then clear every pending
field and the mask.

`require_chain_authority` factors `refund_deposit`'s pre-existing `root.authority` check
(`WrongChainAuthority`) into a shared helper both instructions now call — `refund_deposit`'s own behaviour
is unchanged. `ProposeExitConfig` creates its PDA via `create_or_adopt_pda` (a pre-funded `exit_config`
account is adopted, never refused) **only the first time** — once the account already exists (any later
governance cycle, after the chain's first proposal has been activated), the create-or-adopt step is
skipped entirely and only the new pending fields are written, so a chain authority may propose again and
again with no cap on how many cycles it may run (an earlier version of this instruction called
`create_or_adopt_pda` unconditionally, which made every proposal after the first on a chain revert
`InvalidAccountData` — the portal/bridge/cap/bond could only ever be set once, permanently). A proposal
whose four optional fields are all `None` is now refused (`InvalidArgument`) rather than accepted as an
inert no-op that `ActivateExitConfig` could only ever refuse. Errors 72–73 and 75–79 cover the new refusals
(`ExitPortalZero`, `BridgeProgramZero`, `ActivationTooSoon`, `ActivationNotReached`, `NoPendingExitConfig`,
`PendingExitConfigExists`, `ChallengeWindowZero`); 74/80/81 stay reserved for later exit work.
`zk-settlement-client` gained `propose_exit_config_ix`/`activate_exit_config_ix`/
`decode_exit_config_account`/`exit_config_pda`; the settlement watcher's instruction decoder names both by
discriminant. Measured on real BPF against the fixed settlement-program test id (bit-exact run to run):
`ProposeExitConfig` 9,618 CU, `ActivateExitConfig` 5,244 CU — both well inside the 30k budget; both
instructions' signed V1 transactions (468 B / 257 B) measure well inside the 4,096-byte envelope.

### `ProveExit` (28) — prove an L2 exit against a Final `state_root`, bind the portal on chain, cap per window, burn a replay nullifier

`settle.rs`'s `root_view` gained `final_root_tuple` — its own head-vs-pending finality read, factored into
a pure function (`root::RootFields`, an already-seed-checked pending `AccountInfo`, `batch` in; the FINAL
`{number, parent_hash, state_root, block_hash}` tuple or `NotFinal` out), so `RootView` and the new
`ProveExit` share one finality predicate rather than two independently-maintained copies. `RootView` itself
now just reads `root` and calls it — behaviour is byte-identical (its existing test suite is unchanged and
stays green).

New `exit` module: `ProveExitArgs { chain_id, batch, message: ExitMessageArg, proof: rome_zk_mpt::ExitProof }`
at discriminant 28, permissionless. Accounts `[payer (signer, writable), root, pending(batch), exit_config,
exit_record (writable, new), exit_window (writable, new-or-existing), exit_nullifier_page (writable,
new-or-existing), system_program]`. `ExitMessageArg` mirrors `rome_zk_layouts::exit::ExitMessage`
field-for-field with a borsh derive — that crate stays borsh-free by design (it must build for a ZisK guest
target), so the wire-carrying shape lives next to the instruction instead. Check order (cheapest first): every
account's PDA seeds (the nullifier page's seed is `nullifier_page(message.nonce)`, derived on chain — never a
caller-supplied page, so a wrong-page account fails here, before any bit is read — which means the layouts
crate's `NullifierPageMismatch` guard is unreachable on this call path by construction, and the real "wrong
page" refusal is `InvalidSeeds`); `final_root_tuple`; `exit_config.exit_portal == 0` → `ExitConfigUnset` (an
absent `exit_config` reads the same way); `root.exit_cap_per_window == 0` → `ExitCapUnset`;
`root.challenge_window_slots == 0` → `ChallengeWindowZero`; `message.asset != 0` → `UnsupportedAsset` (v1
native only); the MPT proof — `verify_account` against **`exit_config.exit_portal`, never the message or an
instruction argument**, then `verify_storage` for `message.storage_slot()` — `Absent` → `ExitNotSent`,
`Present(v != 1)` → `ExitProofInvalid`, a bounds/hash/shape error → `ExitProofTooLarge`/`ExitProofInvalid` by
name; the nullifier's `bit_is_set` → `ExitAlreadyProved` (read only — not yet written); the window's
`spent_cap_units + cap_units(amount) > cap` → `ExitCapExceeded` (computed only — not yet written). Every one
of those checks runs to completion, with zero account writes, before the commit step sets the nullifier bit,
writes the window's new spent total and exit count, and writes the `exit_record` (`STATUS_PROVED`) together —
a call refused `ExitCapExceeded` therefore leaves the message's nullifier bit clear, so the exact same exit
can be re-proved once the client re-queues it into a later window, rather than being permanently lost to a
check-ordering bug. The window and nullifier-page PDAs are **shared across exits by design** (many exits land
in the same window or the same 8,192-exit page) and are created with the same `!already_exists`-guarded
`create_or_adopt_pda` pattern already used for `exit_config` — an unconditional call would revert the second
exit sharing either account. Errors 63–69 (`ExitConfigUnset`/`ExitCapUnset`/`ExitAlreadyProved`/
`ExitProofInvalid`/`ExitNotSent`/`ExitCapExceeded`/`ExitAmountOverflow`), 74 (`ExitProofTooLarge`) and 81
(`UnsupportedAsset`) are now used; 70–71 (`ExitNotProved`/`NotBridgeProgram`) are allocated to `ConsumeExit`
(below).

`window_index = Clock::slot / root.challenge_window_slots` — a fixed-length, non-overlapping bucket of
`challenge_window_slots` slots; every exit proved in the same bucket shares one `exit_window` account and
sums its `cap_units` together.

`zk-settlement-client` gained `prove_exit_ix`, `exit_record_pda`/`exit_window_pda`/`exit_nullifier_pda`, and
`decode_exit_record_account`/`decode_exit_window_account`; the settlement watcher's instruction decoder
names `ProveExit` by discriminant and chain id/batch (full `exit` lifecycle decoding — record/window/
nullifier events — arrives with `ConsumeExit`, below).

Measured on real BPF against the committed anvil fixture (3 account nodes + 2 storage nodes — a floor, not
the design's ≤ 120k-CU ceiling assumed for a much larger 16+8-node proof; the real Tiber-scale figure is
a separate, later measurement): **`ProveExit` 41,342 CU**; its signed V1 transaction is **1,179 bytes**, well
inside the 4,096-byte envelope. `tests/exit_prove.rs` (real BPF, `solana-program-test`) covers the valid
path, every named refusal above, two duplicated-state REDs proving two distinct real exits share one
window and one nullifier page without either exit reverting the other (each with its own
`!already_exists`-guard mutation), an exact-cap-boundary success (`>` vs `>=` mutation-tested), an
over-cap-then-next-window-admitted flow (the nullifier-not-yet-burned invariant asserted directly), and a
pre-funded-PDA adoption test for all three new accounts at once.

### `ConsumeExit` (29) — the registered bridge releases a PROVED exit, refunds the record's payer, closes the record; the nullifier bit persists

New `exit::ConsumeExitArgs { chain_id, message_hash }` at discriminant 29, gated on the registered bridge
program's own PDA signer. Accounts `[bridge_signer (signer — the `exit_consumer` PDA `["exit_consumer",
chain_id]` under `exit_config.bridge_program`), exit_config (read-only), exit_record (writable),
payer_refund (writable)]` — no `system_program`; the close path only drains lamports, reallocs to zero and
reassigns, the same three-call shape `ClosePending` already uses. Check order (cheapest first): seeds/owner
of `exit_config`/`exit_record`; `exit_record.status == STATUS_PROVED` else `ExitNotProved` (70) — a
non-existent record fails the owner check first, same read either way; `bridge_signer.key ==
exit_consumer_pda(chain_id, exit_config.bridge_program)` else `NotBridgeProgram` (71), AND
`bridge_signer.is_signer` — a PDA has no private key, so only that program's own `invoke_signed` CPI can
ever satisfy this pair, which is the ENTIRE authorisation; `payer_refund.key == record.payer` else
`InvalidArgument` — the refund destination is read off the record itself, never an instruction argument.
Effect: drain the record's lamports to `payer_refund`, close the account. **The `exit_nullifier` bit is
never touched** — it is the persistent replay guard; a re-`ProveExit` of the same message
after release still hits `ExitAlreadyProved` at `prove_exit`'s own nullifier check, which reads the bit,
never the now-recycled record.

`rome-zk-layouts::exit` gained `exit_consumer_seeds`/`exit_consumer_pda` — `["exit_consumer", chain_id]`
derived UNDER the caller-supplied bridge program, never under the settlement program itself; the one
derivation both `ConsumeExit` and the later `zk-bridge` program's release CPI must agree on byte-for-byte.
`zk-settlement-client` gained `consume_exit_ix`/`exit_consumer_pda` (`decode_exit_record_account` already
existed from the `ProveExit` change). The settlement watcher's instruction decoder names `ConsumeExit` by
discriminant and chain id, and gained real `exit` lifecycle decoding: a new `ExitEvent` enum
(`Proved`/`Released`) alongside `ChunkEvent`/`BatchEvent` in `DerivedEvents` (`#[serde(default)]` on the new
field — an already-ingested row's JSON predates it and must still decode), and a new derive pass,
`lifecycle::derive_exit_once` (structurally identical to `derive_once`, its own `derive_cursor` row under the
settlement program's existing `'root'` ingest kind — `ProveExit`/`ConsumeExit` are instructions of that one
program, never a separate ingest source). `migrations/explorer/0001_settlement.sql` gained a new `exit` table
(keyed by `message_hash`, globally unique) and `'exit'` as a reserved `settlement_tx_program.kind` value
(unused so far — `ProveExit`/`ConsumeExit` route through the existing `'root'` kind — kept open the same way
`'proof'`/`'challenge'` already were, for a possible future dedicated ingest source).

Tested against a minimal, TEST-ONLY stub bridge program (`programs/zk-settlement/tests/fixtures/stub-bridge`,
its own single-crate workspace outside the `programs/*` member glob, built by explicit manifest path in CI's
`test` job only — never picked up by the `build-sbf` job's release-artifact upload, never shipped as a
deployed program) that CPI-signs its own `exit_consumer` PDA via `invoke_signed`, the same mechanism the real
`zk-bridge` program uses. `programs/zk-settlement/tests/exit_consume.rs` (real BPF): a bridge-signed
`ConsumeExit` against a PROVED record releases and closes it, refunding the record's exact rent to
`record.payer`; a different program's own signed `exit_consumer` PDA (when `exit_config.bridge_program` names
someone else) is refused `NotBridgeProgram`; a `payer_refund` that is not `record.payer` is refused
`InvalidArgument` with nothing moved; consuming with no record, or consuming twice, both fail the same owner
check a recycled/absent account always would; and `prove_exit_after_release_is_refused` drives a genuine
`ProveExit` → `ConsumeExit` → re-`ProveExit` of the same message end to end, proving the nullifier bit — not
the record — is what makes the replay guard persistent. Every test uses
`rome_zk_testkit::fixed_settlement_program_id`/the new `fixed_stub_bridge_program_id` (never
`Pubkey::new_unique`) so the measured CU is reproducible run to run, independent of PDA bump-seed search depth
— measured with them, `ConsumeExit`'s own execution (isolated via `sol_log_compute_units` markers either side
of the CPI) is **17,168 CU**; the whole transaction including the stub's own CPI-dispatch overhead is **20,480
CU**. Both exceed the planned `ConsumeExit ≤ 15k` figure — which was an ASSUMPTION, never previously measured
— reported here as the real number rather than forced to fit it; no hard CU ceiling is asserted in the test,
matching `ProveExit`'s own CU test (only the real 4,096-byte tx-size envelope is a hard assertion).
`crates/rome-zk-settlement-watcher/tests/exit_lifecycle.rs` (postgres-container): the watcher decodes both
instructions by name, and a `ProveExit` then `ConsumeExit` pair drives one `exit` row from `proved` to
`released` with both signatures recorded.

## Migration notes

These are the one-time or per-chain operational steps run against the settlement and
inbox programs. None of them require exposing a private key beyond what a normal signed transaction
already needs; every key referenced below is read from wherever the deployment's key management already
keeps it, never printed or logged.

### Upgrading the programs

The four on-chain programs (zk-inbox, zk-settlement, veritas and zk-bridge) are deployed under upgradeable
program keys. To ship a new build:

1. Build the programs (`cargo build-sbf --arch v3` for each of `programs/*/`, per the top-level
   [`README.md`](README.md)) and confirm the workspace test suite is green against the new build.
2. Deploy the upgrade using the standard Solana upgrade path (`solana program deploy` against the
   existing program id, signed by the program's current upgrade authority).
3. Check that every chain on those programs still posts and that its services are healthy
   (`./rollup check` for a chain run from `deploy/rollup`).

A change to an account layout, a PDA seed, or the accumulator's commitment formula is **not** a plain upgrade
— those are baked into every existing account, and changing them means every existing account of that shape is
now misread by the new program. Such a change needs a fresh chain (new program ids and a clean chain state)
rather than an in-place upgrade, until a migration path for that specific layout exists.

### Bootstrapping a new chain's batch cursor (`InitBatchCursor`)

Every chain the inbox program serves needs its own sequential batch-id cursor before its first
`OpenBatch` call. Run `InitBatchCursor` once per chain, signed by the chain's registered authority, naming
the batch id the cursor should start counting from. Batch ids are 1-based: for a brand-new chain this is
`1` — settlement's own continuity check requires the first postable batch to be `head_pending_batch + 1 =
1`, so a cursor bootstrapped at 0 can never post its first batch. `crates/zk-inbox-client/examples/
init_cursor.rs` defaults to 1 and refuses `--next-batch 0` unless `--allow-zero` is explicitly passed. For
a chain that already has batch history elsewhere (a chain being migrated onto this inbox program), it
must be initialized at **`head_pending_batch + 1` of the chain's settlement root** (the id settlement
accepts next) — initializing it too low would let a future `OpenBatch` reuse an id whose data availability
may already have been reclaimed, and initializing it above that id halts the chain. A second
`InitBatchCursor` call for the same chain is rejected; there is no re-run path.

### Bringing a pre-existing chain forward (`MigrateChainV2`)

A chain whose root and registry accounts were created before the settlement program's registration and
fee bookkeeping existed (Tiber's own chain, for example) has no `chain_config` account, and every
`PostRoot` / `PostRootProved` call for it requires one. A chain migrated before `chain_config` version 2
existed has a version 1 account with no drift bound, and layout 1 cannot post for it. `MigrateChainV2`
(discriminant 23), signed by the registry authority, handles both: it creates a missing `chain_config` at
version 2 with the current global defaults (fee schedule, deposit bookkeeping), or grows a version 1
account to version 2 in place and keeps every existing field. A chain already on version 2 is refused
(`ChainAlreadyMigrated`). The drift bound is an explicit argument with no default, and `0` is refused. A
`chain_config` it creates records that no deposit was ever locked, so the deposit counts as already
refunded. The original `MigrateChain` (15) is retired and refused by name (`RetiredInstruction`).

Run it with `rome-zk-ops migrate ... --max-drift-secs N` (a dry run unless `--confirm`). Run
`InitGlobalConfig` once per deployment first: the global configuration account (`treasury`,
`registry_authority`, default fee schedule) must exist for the migration to read its defaults from.
