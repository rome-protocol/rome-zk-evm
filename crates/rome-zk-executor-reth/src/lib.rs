//! `RethExecutor` — an in-process reth v2.5.2 implementation of
//! `rome_zk_executor_api::Executor`, plus (`src/node.rs`) a standalone reth `eth_*`/`net`/
//! `web3`/`debug`/`trace`/`ots` JSON-RPC surface reading directly off this executor's own MDBX.
//!
//! Sources read at the pinned tag (`v2.5.2`) before writing this file, cited at each call site
//! below:
//!   - `crates/evm/evm/src/lib.rs` — `ConfigureEvm`, `NextBlockEnvAttributes`,
//!     `builder_for_next_block`, `evm_with_env`.
//!   - `crates/evm/evm/src/execute.rs` — `Executor`/`BlockBuilder` traits, `BlockBuilderOutcome`
//!     (`execution_result`/`hashed_state`/`trie_updates`/`block` — the assembled, hashed, rooted
//!     block; `execution_result: BlockExecutionResult<Receipt>` re-exported from `alloy_evm`, see
//!     `~/.cargo/registry/src/*/alloy-evm-0.38.0/src/block/mod.rs`: `receipts`/`requests`/`gas_used`).
//!   - `crates/ethereum/evm/src/{lib.rs,receipt.rs}` — `EthEvmConfig`, `RethReceiptBuilder`'s exact
//!     receipt shape (`tx_type`, `success`, `cumulative_gas_used`, `logs`).
//!   - `crates/ethereum/evm/tests/execute.rs` — the `BasicBlockExecutor::new(evm_config, db)` /
//!     `provider.batch_executor(db)` pattern this module's `seal_block` re-execution follows.
//!   - `crates/storage/provider/src/test_utils/mod.rs` — the exact `ProviderFactory::new(db,
//!     chain_spec, static_file_provider, rocksdb_provider, runtime)` wiring `open_provider_factory`
//!     below mirrors, pointed at a real (non-temp) datadir instead of `tempdir_path()`.
//!   - `crates/storage/db-common/src/init.rs` — `init_genesis`.
//!   - `crates/storage/storage-api/src/block_writer.rs` — `BlockWriter::append_blocks_with_state`
//!     (this crate's block persistence — see `executor.rs::seal_block`'s comment for
//!     why this method, not the full engine, and reth's own doc comment on it,
//!     `crates/storage/provider/src/providers/database/provider.rs` ~line 3822: "This function is
//!     only used in tests").
//!   - `crates/storage/provider/src/providers/database/provider.rs` — `BlockNumReader::
//!     best_block_number` (reads the `StageId::Finish` checkpoint `append_blocks_with_state`
//!     advances) vs `last_block_number` (reads only the static-file frontier, never advanced by this
//!     crate — the wrong method for "real persisted head" here); `DatabaseProvider::latest()`
//!     (`LatestStateProvider` — reads the plain state tables directly, no separate "canonical
//!     pointer" to advance beyond the write transaction's own commit).
//!   - `crates/revm/src/database.rs` — `StateProviderDatabase`.
//!   - `examples/rpc-db/src/main.rs`, `examples/node-custom-rpc/src/main.rs` — `RpcModuleBuilder` +
//!     `EthApiBuilder` standalone-over-a-provider wiring, and `TransportRpcModules::
//!     replace_configured`/`merge_configured` for overriding/extending namespaces (`src/node.rs`).
//!
//! ## Design mapping
//!
//! - **Block env is committed before execution**: `open_block(BlockEnv)` fixes the
//!   block's number, timestamp (the sealer's rule: max(first sub-block's second, parent + 1)),
//!   gas limit, coinbase and `prev_randao` = keccak(chain_id ‖ number ‖ channel_id) BEFORE the first
//!   sub-block runs; `seal_block` asserts it was given the identical env (`EnvMismatch` otherwise).
//!   A pre-confirmed sub-block therefore never diverges from the sealed block.
//! - **Sub-block execution** (`execute_sub_block`): txs run against a `revm::State` held in
//!   `PendingBlock` for the block's 20 sub-blocks (sub-block N+1 sees N's changes), layered over
//!   `ProviderFactory::latest()` PLUS a value overlay (`CacheState`, built from the executed bundle
//!   of every sealed-but-not-yet-persisted block — normally exactly one). No state root per sub-block;
//!   `receipts_root` = ordered trie root of this sub-block's receipts. `nonce()` reads the overlay
//!   first, then the DB.
//! - **Block sealing** (`seal_block`): re-executes the included, ordered, already-recovered txs once
//!   (no second ECDSA pass) through reth's `builder_for_next_block` + `BlockBuilder::finish` over a
//!   `MemoryOverlayStateProviderRef` of the same overlay; `finish()` computes the state root. The
//!   header (hash, state root) is returned immediately.
//! - **Persistence is asynchronous and ordered** ("execute N+1 while merkleizing N"): the executed block is handed to a
//!   `spawn_blocking` task that waits for the previous block's write, then `BlockWriter::append_blocks_with_state`s it
//!   into MDBX and commits. `open_block(N+1)` NEVER awaits that task: block N stays in the overlay until its write is
//!   confirmed (`reap_finished_write`), then the overlay entry is dropped. Layering a block's bundle over a DB that
//!   already contains it is idempotent (absolute values). After the write, the shared `BlockchainProvider`'s
//!   canonical/safe/finalized head is set so `eth_blockNumber` and every "latest" read follow the chain.
//! - **What a crash loses and how it is repaired**: at most the unpersisted block(s) in the overlay.
//!   The ordered log is the truth; on restart `RethExecutor::new` reads the durable head
//!   from MDBX and `replay_into_executor` re-executes only the log tail beyond it (tests:
//!   `reth_tail_replay.rs`, the kill -9 test). MDBX stays in durable-commit mode: measured log fsync
//!   ≤ 10 ms and tick lateness ≈ 1 ms, so relaxing it buys nothing.
//! - **Measured (Apple silicon, release, idle host)**: seal foreground 0.3 ms @100 txs and
//!   7 ms @5,000 txs; pre-confirmation p99 51–58 ms across 10 runs on both RPC paths (was 84–92 ms
//!   before persistence, 121–138 ms while `open_block` still awaited the write); RSS 189 MB after
//!   1,000 blocks; state-root time flat to 50k touched accounts, so trie-node persistence
//!   (`TrieWriter`) is not wired yet.

#![forbid(unsafe_code)]

mod chain;
mod executor;
mod static_heal;

pub use chain::{genesis_from_path, open_provider_factory, RethTypes};
pub use executor::{RethConfig, RethExecutor};
pub use static_heal::{ForwardHealOutcome, ForwardHealRefusal, VerifiedForwardHeal};
