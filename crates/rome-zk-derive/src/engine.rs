//! `EngineController`: drives a stock reth purely through JSON-RPC — the real,
//! authenticated Engine API (`forkchoiceUpdated`/`newPayload`) plus, for forcing this batch's *exact*
//! ordered tx list into the built block (no mempool reordering/omission — `noTxPool`
//! semantics), reth's `testing_buildBlockV1` namespace.
//!
//! ## Why `testing_buildBlockV1`, not a normal `getPayload` build
//!
//! Standard (non-OP) Ethereum Engine API has **no forced-transaction-list mechanism** — `getPayload`
//! always builds from whatever the node's own mempool/builder decided, and a plain
//! `reth_node_ethereum::EthereumNode` (the node type this design's chain actually runs — see
//! `rome-zk-executor-reth`) does not understand OP's payload-attributes extension
//! (`transactions`/`noTxPool`) at all. `rome-zk-executor-reth/tests/derivation_equivalence.rs` hit
//! exactly this and used reth's own `testing_buildBlockV1`/`commitBlockV1` namespace (a real,
//! spec-documented mechanism — see `alloy_rpc_types_engine::testing`/reth's
//! `crates/rpc/rpc-api/src/testing.rs`: "Raw signed transactions to force-include in order") instead.
//! This module follows the same precedent for the derivation node's *production* Engine API path, not
//! only its tests.
//!
//! **Open question this repo should resolve (flagged, not silently assumed):** reth gates
//! `testing_buildBlockV1` behind an explicit `--http.api testing` flag and calls it "Highly sensitive:
//! testing-only, powerful enough to include arbitrary transactions" — running it on a derivation node's
//! reth is a real operational dependency the docs do not currently name. Because
//! `testing_buildBlockV1` is served on the plain RPC surface (not the authenticated engine port —
//! verified against reth v2.5.2, `crates/ethereum/node/src/node.rs`: it merges into `container.modules`,
//! not the auth module), [`JsonRpcEngineApi`] takes **two** endpoints: `rpc_url` (plain, for
//! `testing_buildBlockV1`) and `engine_url`+`jwt_secret_path` (authenticated, for the real
//! `engine_forkchoiceUpdatedV3`/`engine_newPayloadV4`/`eth_getBlockByNumber` consolidation read).
//!
//! ## `env.number == target_height`
//!
//! `BlockEnv::number` is a Rome-protocol-level, batch-relative numbering fed into `prev_randao` and the
//! channel stream's own RLP; a real Ethereum EL always assigns the next block **parent height + 1**, and
//! genesis itself is always real height 0. Numbering **coincides at the source**: the sequencer numbers its
//! first sealed block 1 (`rome-zk-sequencer`), so `BlockEnv::number` equals the real height for every block a
//! healthy chain ever produces — `RethExecutor` still never reads `BlockEnv.number` as the height it builds
//! at (reth assigns real height by its own parent+1 rule regardless), the two are simply arranged to agree.
//! This module still asserts the equality explicitly rather than baking it in silently — a batch whose
//! numbering has drifted from the engine's real height is a real bug in this node's own bookkeeping (or a
//! malformed batch), not something a downstream tx-set mismatch should be left to surface:
//! [`EngineController::advance`] rejects a batch whose `target_height != attrs.env.number` with a named
//! [`PipelineError::Critical`] before doing anything else with it.
//!
//! ## Consolidation and the engine's own head
//!
//! `crate::traversal::SolanaTraversal` always resumes at batch 0 unless a settlement-root anchor
//! confirms a later one (`crate::resume`), so [`EngineController::from_engine_head`] (the
//! genesis-first fallback) seeds `parent_hash`/the next real height to build from **the engine's own
//! real genesis** (`block_at(0)` — never a hardcoded `B256::ZERO`), not from
//! wherever the chain currently is: re-deriving batch 0 must ask for real height 1, whatever the
//! engine's actual head height turns out to be. Every already-built block along the way — real heights
//! `1..=head_height` — then **consolidates** rather than rebuilds (a cheap read + compare, one
//! [`EngineApi::block_at`] call, no `build_forced_block`/`new_payload`/`forkchoice_updated`) until
//! [`Self::advance`] reaches a height the engine does not have yet, where it starts building for real —
//! this is what makes "the engine's own chain is the durable truth" concrete: the transition from
//! consolidating to building is discovered per-block, not computed upfront.
//!
//! **`PipelineError::Reset` is deleted.** An earlier change deleted `crate::traversal::SolanaTraversal`'s
//! `open_slot`-gap heuristic — its one production trigger — and reassigned `Reset` to mean only "the engine's head
//! disagrees with the derived head"; but that case was already, and always had been, [`PipelineError::Critical`] via
//! [`Self::advance`]'s consolidation and build-identity checks (below) — a genuine equivocation or operator error,
//! never a recoverable drift. With no live trigger left, the now-provably-dead variant and its one caller
//! ([`crate::pipeline::DerivePipeline::step`]'s former `Reset` arm) were removed — a restart re-seeds via a fresh
//! [`Self::from_engine_head`] call in the binary instead. The in-process re-seed primitive that removal left with no
//! production caller (`sync_from_engine`) is itself deleted: [`Self::from_engine_head`] is the one seeding path this
//! crate needs, exercised directly by this module's own
//! `restart_resumes_after_the_engines_existing_blocks_without_rebuilding_them` test.
//!
//! Consolidation compares the **full committed identity** — `number` (via the height check above), `timestamp`,
//! `prev_randao`, `gas_limit`, and the **ordered** tx list — never tx sets alone (the identical check applies to the
//! *build* branch, not only consolidation): `prev_randao` is unique per design block by construction
//! (`rome_zk_executor_api::prev_randao`), so a real content mismatch at the correct height is a genuine equivocation,
//! not a height-tracking artifact — [`PipelineError::Critical`], never a retry.

use alloy_eips::eip7685::RequestsOrHash;
use alloy_primitives::{keccak256, Address, Bytes, B256};
use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadStatus,
    PayloadStatusEnum, TestingBuildBlockRequestV1,
};
use jsonrpsee::core::client::ClientT;

use crate::attributes::Attributes;
use crate::PipelineError;

/// Builds this block's `PayloadAttributes` (the published block-environment fields) from the ONE shared
/// [`rome_zk_executor_api::canonical_header_rule`] — every rule-fixed field
/// (`prev_randao`, `suggested_fee_recipient`, `parent_beacon_block_root`) comes from that call, never an
/// independently-pinned literal, so this build can never silently diverge from the sequencer's own
/// `rome-zk-executor-reth::executor::attrs_from_env` or the guest's per-block assertion. Factored out
/// of [`EngineController::advance`] as its own pure function so it is unit-testable without a mock engine.
fn payload_attrs_from(attrs: &Attributes) -> PayloadAttributes {
    let rule = rome_zk_executor_api::canonical_header_rule(
        attrs.chain_id,
        attrs.env.number,
        attrs.env.coinbase,
    );
    PayloadAttributes {
        timestamp: attrs.env.timestamp_secs,
        prev_randao: rule.prev_randao,
        suggested_fee_recipient: rule.beneficiary,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(rule.parent_beacon_block_root),
        slot_number: None,
        target_gas_limit: Some(attrs.env.gas_limit),
    }
}

/// The Engine API surface [`EngineController`] drives. One seam: a profile that talks
/// to a different execution client (or a different forced-inclusion mechanism) implements this trait
/// instead of touching [`EngineController`]'s own logic.
pub trait EngineApi: Send {
    /// Builds a block on top of `parent_block_hash` containing exactly `transactions`, in order —
    /// reth's `testing_buildBlockV1` (this module's doc explains why). Returns the built
    /// `(payload, execution_requests)`.
    fn build_forced_block(
        &mut self,
        parent_block_hash: B256,
        attrs: PayloadAttributes,
        transactions: Vec<Bytes>,
    ) -> impl std::future::Future<Output = Result<(ExecutionPayloadV3, RequestsOrHash), PipelineError>>
           + Send;

    /// The real `engine_newPayloadV4` (or the version this chain's fork needs) — submits a built
    /// payload for validation.
    fn new_payload(
        &mut self,
        payload: ExecutionPayloadV3,
        requests: RequestsOrHash,
    ) -> impl std::future::Future<Output = Result<PayloadStatus, PipelineError>> + Send;

    /// The real `engine_forkchoiceUpdatedV3`, with no payload attributes — used only to canonicalize a
    /// block already accepted by [`Self::new_payload`].
    fn forkchoice_updated(
        &mut self,
        state: ForkchoiceState,
    ) -> impl std::future::Future<Output = Result<ForkchoiceUpdated, PipelineError>> + Send;

    /// The block this engine's own chain already has at `number`, if any — its full committed identity.
    /// Used only by [`EngineController::advance`]'s consolidation check ("consolidate
    /// instead of re-execute when the unsafe block already matches").
    fn block_at(
        &mut self,
        number: u64,
    ) -> impl std::future::Future<Output = Result<Option<ExistingBlock>, PipelineError>> + Send;
}

/// A block's full committed identity, as read back from the engine's own chain — everything
/// [`EngineController::advance`]'s consolidation check compares against `attrs.env` (tx-set-only
/// equality let a genuine content mismatch consolidate silently as long as the
/// same tx hashes happened to appear).
///
/// The original six fields caught a mismatched tx set,
/// timestamp, prev_randao or gas_limit — but not a consolidated block whose OTHER derivation-rule-fixed
/// header fields (beneficiary, extra_data, the withdrawals/parent-beacon roots, the blob-gas fields)
/// disagreed with `rome_zk_executor_api::canonical_header_rule`: a resumed node could silently accept a
/// prior run's block built under a different (still consensus-valid) rule for any of those. Every field
/// here that this rule fixes is now part of the committed identity, read back the same way the six
/// original fields already were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingBlock {
    pub block_hash: B256,
    pub timestamp: u64,
    pub prev_randao: B256,
    pub gas_limit: u64,
    /// The challenger's own deliverable (derive exposes
    /// per-batch `(last_block_hash, state_root)`) must not go missing just because this block was
    /// consolidated rather than built — `eth_getBlockByNumber` reports it (`stateRoot`) same as any
    /// other committed field.
    pub state_root: B256,
    /// In block order — consolidation requires exact ordered equality with the frame's tx list, not set
    /// equality (`noTxPool`: arrival order is execution order).
    pub tx_hashes: Vec<B256>,
    /// `header.miner`/`beneficiary` — must equal `HeaderRule::beneficiary`.
    pub beneficiary: Address,
    /// `header.extraData` — must equal `HeaderRule::extra_data` (always empty on this chain).
    pub extra_data: Bytes,
    /// `header.withdrawalsRoot` — must equal `HeaderRule::withdrawals_root`.
    pub withdrawals_root: B256,
    /// `header.parentBeaconBlockRoot` — must equal `HeaderRule::parent_beacon_block_root` (always ZERO,
    /// no beacon chain behind this rollup).
    pub parent_beacon_block_root: B256,
    /// `header.blobGasUsed` — must equal `HeaderRule::blob_gas_used` (always 0, no blobs).
    pub blob_gas_used: u64,
    /// `header.excessBlobGas` — must equal `HeaderRule::excess_blob_gas` (always 0, no blobs).
    pub excess_blob_gas: u64,
}

/// One derived block's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockOutcome {
    pub block_number: u64,
    pub block_hash: B256,
    pub state_root: B256,
    /// `true` if this block was already present on the engine's chain and merely verified
    /// (consolidated), `false` if this call actually built/submitted/canonicalized it.
    pub consolidated: bool,
}

/// Drives one [`EngineApi`] block by block, tracking the parent hash and the **real** chain height it
/// should build on next.
///
/// **Real chain height is tracked independently of `Attributes::env`'s `number` field, even though the
/// two now coincide by construction (bug found by `tests/engine_equivalence.rs` against a real node,
/// not asserted from architecture; the sequencer side was fixed so they agree).**
/// `BlockEnv::number` is a sequencer-assigned design number, numbered from 1 (the old
/// fixed `batch * BLOCKS_PER_BATCH + i` arithmetic was withdrawn — a batch's blocks may start at any
/// number, only internally consecutive and continuing from the previous batch) fed into `prev_randao`
/// and the channel stream's own RLP — it is a *parameter* `PayloadAttributes` carries in, never something
/// the Engine API lets a caller dictate. A real Ethereum EL always assigns the next block **parent
/// height + 1**, and genesis itself is always real height 0 — so a fresh chain's first produced block is
/// real height 1, which is also its `BlockEnv::number` now. This struct still tracks `next_height`
/// on its own, seeded from the real parent height at construction, rather than reading `attrs.env.number`
/// as the height to query/build at — [`Self::advance`] asserts the two agree (module doc) and is
/// Critical, not silently self-correcting, the moment they don't.
pub struct EngineController<E> {
    engine: E,
    parent_hash: B256,
    next_height: u64,
}

impl<E: EngineApi> EngineController<E> {
    /// `parent_hash`/`parent_height` are the engine's REAL current head and its REAL height (0 for a
    /// fresh chain at genesis) — [`Self::advance`]'s first call will build real height
    /// `parent_height + 1` on top of `parent_hash`.
    pub fn new(engine: E, parent_hash: B256, parent_height: u64) -> Self {
        Self {
            engine,
            parent_hash,
            next_height: parent_height + 1,
        }
    }

    /// Seeds `parent_hash`/the next real height to build directly from the engine's own **real
    /// genesis** — `block_at(0)` (the previous hardcoded `(B256::ZERO, 0)` used
    /// the right height but the wrong, fake parent hash, which fails as `HeaderNotFound` the moment a
    /// real build is attempted on top of it). Genesis, not "wherever the chain currently is": this
    /// crate's traversal always resumes at batch 0 (module doc), so [`Self::advance`]'s very first call
    /// must target real height 1 regardless of how far the engine's chain has actually advanced —
    /// [`Self::advance`]'s own consolidation check (module doc) is what lets it walk forward from there
    /// without rebuilding anything already present.
    pub async fn from_engine_head(mut engine: E) -> Result<Self, PipelineError> {
        let genesis = engine.block_at(0).await?.ok_or_else(|| {
            PipelineError::Temporary("genesis (real height 0) not found on the engine".into())
        })?;
        Ok(Self::new(engine, genesis.block_hash, 0))
    }

    pub fn parent_hash(&self) -> B256 {
        self.parent_hash
    }

    /// The real chain height [`Self::advance`]'s next call will build (or consolidate).
    pub fn next_height(&self) -> u64 {
        self.next_height
    }

    /// This controller's own bookkeeping — `(parent_hash, next_height)` — never a read of the engine's
    /// actual chain. A caller snapshots this before attempting a multi-block
    /// batch and [`Self::rewind_to`]s it back on a non-Critical failure, so a retry's first block sees
    /// the same expected height again instead of `next_height` having already advanced past whatever
    /// blocks succeeded before the failure.
    pub fn position(&self) -> (B256, u64) {
        (self.parent_hash, self.next_height)
    }

    /// Restores a position [`Self::position`] previously returned — see that method's doc. Never
    /// touches the engine's own chain; this only rewinds this controller's bookkeeping of where it
    /// believes it is.
    pub fn rewind_to(&mut self, position: (B256, u64)) {
        self.parent_hash = position.0;
        self.next_height = position.1;
    }

    /// The underlying [`EngineApi`] — mainly for tests to inspect a [`mock::MockEngineApi`]'s call log.
    pub fn engine(&self) -> &E {
        &self.engine
    }

    /// Derives one block from `attrs` (FCU/getPayload/newPayload per L2 block, then
    /// "block tx set == frame tx set?" — a mismatch is [`PipelineError::Critical`], the strict policy a
    /// sequencer-lane tx failing execution triggers, never a skip).
    pub async fn advance(&mut self, attrs: &Attributes) -> Result<BlockOutcome, PipelineError> {
        let target_height = self.next_height;

        // The design premise this whole module leans on, asserted explicitly
        // rather than baked in silently (see this module's doc) — a drift here is a real bug in this
        // node's own height bookkeeping (or a malformed batch), not something a downstream tx-set
        // mismatch should be left to surface.
        if target_height != attrs.env.number {
            return Err(PipelineError::Critical(format!(
                "design premise violated: real height to build ({target_height}) must equal \
                 attrs.env.number ({}) — see engine.rs's module doc",
                attrs.env.number
            )));
        }

        let expected_hashes: Vec<B256> = attrs.txs.iter().map(keccak256).collect();

        // Consolidation: if the engine's chain already has this exact block (a resumed
        // node re-deriving a batch it already processed before a restart), verify instead of rebuilding.
        // Queried by the REAL height (`target_height`), never `attrs.env.number` (see this struct's doc).
        // The full committed identity must match — number (via the height
        // check above), timestamp, prev_randao, gas_limit, and the ORDERED tx list — not tx sets alone.
        if let Some(existing) = self.engine.block_at(target_height).await? {
            // The six original fields (below) never covered
            // the OTHER derivation-rule-fixed header fields — a stale/foreign block whose beneficiary,
            // extra_data, withdrawals/parent-beacon roots or blob-gas fields disagreed with the ONE
            // shared rule used to consolidate silently as long as timestamp/prev_randao/gas_limit/tx set
            // happened to match. `rule` is the same call `payload_attrs_from` makes for the BUILD path
            // (module doc), so a resumed node checks a consolidated block against the identical rule a
            // freshly-built one is held to.
            let rule = rome_zk_executor_api::canonical_header_rule(
                attrs.chain_id,
                target_height,
                attrs.env.coinbase,
            );
            let identity_matches = existing.timestamp == attrs.env.timestamp_secs
                && existing.prev_randao == attrs.env.prev_randao
                && existing.gas_limit == attrs.env.gas_limit
                && existing.tx_hashes == expected_hashes
                && existing.beneficiary == rule.beneficiary
                && existing.extra_data == rule.extra_data
                && existing.withdrawals_root == rule.withdrawals_root
                && existing.parent_beacon_block_root == rule.parent_beacon_block_root
                && existing.blob_gas_used == rule.blob_gas_used
                && existing.excess_blob_gas == rule.excess_blob_gas;
            if identity_matches {
                self.parent_hash = existing.block_hash;
                self.next_height += 1;
                return Ok(BlockOutcome {
                    block_number: target_height,
                    block_hash: existing.block_hash,
                    // The state root IS observable from block_at() — `eth_getBlockByNumber`
                    // reports `stateRoot` same as any other committed field (see `ExistingBlock`'s doc).
                    state_root: existing.state_root,
                    consolidated: true,
                });
            }
            return Err(PipelineError::Critical(format!(
                "real height {target_height} already exists on the engine's chain with a different \
                 committed identity (timestamp/prev_randao/gas_limit/ordered tx list, or a \
                 derivation-rule-fixed header field: beneficiary/extra_data/withdrawals_root/\
                 parent_beacon_block_root/blob_gas_used/excess_blob_gas) than this batch's frames — \
                 cannot consolidate"
            )));
        }

        let payload_attrs = payload_attrs_from(attrs);
        let (payload, requests) = self
            .engine
            .build_forced_block(self.parent_hash, payload_attrs, attrs.txs.clone())
            .await?;

        let status = self.engine.new_payload(payload.clone(), requests).await?;
        match &status.status {
            PayloadStatusEnum::Valid => {}
            // SYNCING/ACCEPTED are ordinary "not yet, ask again later" engine
            // states (an active sync, or a side-chain payload not yet processed) — not a strict-validity
            // fault. Only INVALID is this batch's own content being rejected.
            PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted => {
                return Err(PipelineError::Temporary(format!(
                    "block {}: engine newPayload returned {:?} (not yet valid) — retry",
                    attrs.env.number, status.status
                )));
            }
            PayloadStatusEnum::Invalid { .. } => {
                return Err(PipelineError::Critical(format!(
                    "block {}: engine rejected newPayload: {:?}",
                    attrs.env.number, status.status
                )));
            }
        }

        let built = &payload.payload_inner.payload_inner;
        let block_hash = built.block_hash;
        let state_root = built.state_root;
        let block_number = built.block_number;
        if block_number != target_height {
            return Err(PipelineError::Critical(format!(
                "engine built real height {block_number}, expected {target_height} (parent hash tracking has drifted from the engine's real chain)"
            )));
        }

        // The built payload's full committed identity — parent_hash,
        // timestamp, prev_randao, gas_limit, and the ORDERED tx list — must match this batch's own
        // frame, exactly the same check [`Self::advance`]'s consolidation branch above already applies.
        // The previous build-branch check compared only `block_number` and an UNORDERED tx
        // set (`tx_set_eq`) — weaker than consolidation's, and silently accepting a builder that clamped
        // `target_gas_limit` against a mismatched parent, or reordered the forced txs, as this batch's
        // canonical derivation ("arrival order is execution order").
        let built_hashes: Vec<B256> = built.transactions.iter().map(keccak256).collect();
        if built.parent_hash != self.parent_hash
            || built.timestamp != attrs.env.timestamp_secs
            || built.prev_randao != attrs.env.prev_randao
            || built.gas_limit != attrs.env.gas_limit
            || built_hashes != expected_hashes
        {
            return Err(PipelineError::Critical(format!(
                "block {block_number}: built payload's committed identity (parent_hash/timestamp/prev_randao/gas_limit/ordered tx list) diverges from this batch's frame — strict policy"
            )));
        }

        self.engine
            .forkchoice_updated(ForkchoiceState {
                head_block_hash: block_hash,
                safe_block_hash: block_hash,
                finalized_block_hash: block_hash,
            })
            .await?;

        self.parent_hash = block_hash;
        self.next_height += 1;
        Ok(BlockOutcome {
            block_number,
            block_hash,
            state_root,
            consolidated: false,
        })
    }
}

/// Production implementation: two real jsonrpsee HTTP clients against a stock reth v2.5.2 —
/// `rpc_client` (plain, for `testing_buildBlockV1`) and `auth_client` (JWT-authed via
/// `reth_rpc_layer::AuthClientLayer`, matching `reth_rpc_builder::auth::AuthServerHandle::http_client`'s
/// own construction, for the real Engine API). Not itself directly unit-tested (it is a thin
/// serialization shim over two real network clients); its behavior is exercised end-to-end by
/// `tests/engine_equivalence.rs` against a real in-process reth node.
pub struct JsonRpcEngineApi<C, A> {
    rpc_client: C,
    auth_client: A,
}

/// Connects to a stock reth's plain RPC endpoint (for `testing_buildBlockV1`) and its authenticated
/// engine endpoint (JWT read from `jwt_secret_path`, hex-encoded — the same file format
/// `tools/derive/src/main.rs`'s `--jwt` flag reads). Returns `impl EngineApi` so callers never need to
/// name the concrete (tower-middleware-wrapped) client type.
pub fn connect(
    rpc_url: &str,
    engine_url: &str,
    jwt_secret_path: &std::path::Path,
) -> Result<impl EngineApi, ConnectError> {
    let jwt_hex = std::fs::read_to_string(jwt_secret_path)
        .map_err(|e| ConnectError::ReadJwt(jwt_secret_path.to_path_buf(), e))?;
    let secret = alloy_rpc_types_engine::JwtSecret::from_hex(jwt_hex.trim())
        .map_err(|e| ConnectError::ParseJwt(e.to_string()))?;

    let rpc_client = jsonrpsee::http_client::HttpClientBuilder::default()
        .build(rpc_url)
        .map_err(|e| ConnectError::BuildClient(format!("rpc client: {e}")))?;

    let secret_layer = reth_rpc_layer::AuthClientLayer::new(secret);
    let middleware = tower::ServiceBuilder::default().layer(secret_layer);
    let auth_client = jsonrpsee::http_client::HttpClientBuilder::default()
        .set_http_middleware(middleware)
        .build(engine_url)
        .map_err(|e| ConnectError::BuildClient(format!("auth client: {e}")))?;

    Ok(JsonRpcEngineApi {
        rpc_client,
        auth_client,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("read JWT secret at {0:?}: {1}")]
    ReadJwt(std::path::PathBuf, std::io::Error),
    #[error("parse JWT secret: {0}")]
    ParseJwt(String),
    #[error("build RPC client: {0}")]
    BuildClient(String),
}

impl<C, A> EngineApi for JsonRpcEngineApi<C, A>
where
    C: ClientT + Send + Sync,
    A: ClientT + Send + Sync,
{
    async fn build_forced_block(
        &mut self,
        parent_block_hash: B256,
        attrs: PayloadAttributes,
        transactions: Vec<Bytes>,
    ) -> Result<(ExecutionPayloadV3, RequestsOrHash), PipelineError> {
        let req = TestingBuildBlockRequestV1 {
            parent_block_hash,
            payload_attributes: attrs,
            transactions,
            extra_data: None,
        };
        let envelope: alloy_rpc_types_engine::ExecutionPayloadEnvelopeV5 = self
            .rpc_client
            .request("testing_buildBlockV1", req.into_params())
            .await
            .map_err(|e| map_build_error("testing_buildBlockV1", e))?;
        Ok((
            envelope.execution_payload,
            RequestsOrHash::Requests(envelope.execution_requests),
        ))
    }

    async fn new_payload(
        &mut self,
        payload: ExecutionPayloadV3,
        requests: RequestsOrHash,
    ) -> Result<PayloadStatus, PipelineError> {
        let versioned_hashes: Vec<B256> = Vec::new();
        self.auth_client
            .request(
                "engine_newPayloadV4",
                (payload, versioned_hashes, B256::ZERO, requests),
            )
            .await
            .map_err(|e| map_build_error("engine_newPayloadV4", e))
    }

    async fn forkchoice_updated(
        &mut self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, PipelineError> {
        self.auth_client
            .request::<ForkchoiceUpdated, _>(
                "engine_forkchoiceUpdatedV3",
                (state, None::<PayloadAttributes>),
            )
            .await
            .map_err(|e| map_build_error("engine_forkchoiceUpdatedV3", e))
    }

    async fn block_at(&mut self, number: u64) -> Result<Option<ExistingBlock>, PipelineError> {
        let block: Option<serde_json::Value> = self
            .auth_client
            .request("eth_getBlockByNumber", (format!("0x{number:x}"), false))
            .await
            .map_err(|e| map_build_error("eth_getBlockByNumber", e))?;
        let Some(block) = block else { return Ok(None) };
        let block_hash = parse_hash_field(&block, "hash")?;
        let timestamp = parse_hex_u64_field(&block, "timestamp")?;
        // Post-merge, `mixHash` carries prevRandao (EIP-4399) — this is not a real mix-hash.
        let prev_randao = parse_hash_field(&block, "mixHash")?;
        let gas_limit = parse_hex_u64_field(&block, "gasLimit")?;
        // A consolidated block's state_root is not a build-time artifact —
        // it is a field of the block itself, reported the same as `hash`/`gasLimit`/etc.
        let state_root = parse_hash_field(&block, "stateRoot")?;
        let tx_hashes = parse_tx_hashes(&block)?;
        // Every OTHER derivation-rule-fixed header field, read back the
        // same way as the six original fields above — `identity_matches` checks each against
        // `canonical_header_rule`.
        let beneficiary = parse_address_field(&block, "miner")?;
        let extra_data = parse_bytes_field(&block, "extraData")?;
        let withdrawals_root = parse_hash_field(&block, "withdrawalsRoot")?;
        let parent_beacon_block_root = parse_hash_field(&block, "parentBeaconBlockRoot")?;
        let blob_gas_used = parse_hex_u64_field(&block, "blobGasUsed")?;
        let excess_blob_gas = parse_hex_u64_field(&block, "excessBlobGas")?;
        Ok(Some(ExistingBlock {
            block_hash,
            timestamp,
            prev_randao,
            gas_limit,
            state_root,
            tx_hashes,
            beneficiary,
            extra_data,
            withdrawals_root,
            parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
        }))
    }
}

/// A JSON-RPC `Call` error (the server's own rejection, e.g. -38005
/// `UnsupportedFork`, -32602 invalid params, or `testing_buildBlockV1` rejecting the forced tx list
/// itself) is a permanent, strict-validity fault — [`PipelineError::Critical`] — never the blanket
/// `Temporary` that would retry it forever.
/// `engine_newPayloadV4`/`engine_forkchoiceUpdatedV3`/`eth_getBlockByNumber` route through this same
/// split, not just `testing_buildBlockV1` — a permanent server-side rejection on any of the four calls
/// must not retry at 1 Hz forever. A transport/timeout/connection error is still ordinary and
/// [`PipelineError::Temporary`] on every one of them.
fn map_build_error(method: &str, e: jsonrpsee::core::ClientError) -> PipelineError {
    match e {
        jsonrpsee::core::ClientError::Call(obj) => {
            PipelineError::Critical(format!("{method}: server rejected the call: {obj}"))
        }
        other => PipelineError::Temporary(format!("{method}: {other}")),
    }
}

fn parse_hash_field(block: &serde_json::Value, field: &str) -> Result<B256, PipelineError> {
    block[field]
        .as_str()
        .ok_or_else(|| PipelineError::Temporary(format!("eth_getBlockByNumber: missing {field}")))?
        .parse()
        .map_err(|e| PipelineError::Temporary(format!("eth_getBlockByNumber: bad {field}: {e}")))
}

fn parse_hex_u64_field(block: &serde_json::Value, field: &str) -> Result<u64, PipelineError> {
    let s = block[field].as_str().ok_or_else(|| {
        PipelineError::Temporary(format!("eth_getBlockByNumber: missing {field}"))
    })?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| PipelineError::Temporary(format!("eth_getBlockByNumber: bad {field}: {e}")))
}

/// `miner` — the same field a real Ethereum EL reports as the block's `beneficiary`.
fn parse_address_field(block: &serde_json::Value, field: &str) -> Result<Address, PipelineError> {
    block[field]
        .as_str()
        .ok_or_else(|| PipelineError::Temporary(format!("eth_getBlockByNumber: missing {field}")))?
        .parse()
        .map_err(|e| PipelineError::Temporary(format!("eth_getBlockByNumber: bad {field}: {e}")))
}

/// `extraData` — hex-encoded, possibly `"0x"` (empty), never absent on a real block.
fn parse_bytes_field(block: &serde_json::Value, field: &str) -> Result<Bytes, PipelineError> {
    block[field]
        .as_str()
        .ok_or_else(|| PipelineError::Temporary(format!("eth_getBlockByNumber: missing {field}")))?
        .parse()
        .map_err(|e| PipelineError::Temporary(format!("eth_getBlockByNumber: bad {field}: {e}")))
}

/// A missing/non-array `transactions` field, or any element
/// that is not a parsable hash, is [`PipelineError::Temporary`] — the same "ask again later" outcome
/// every other malformed field in this response gets. The previous code (`.as_array().map(...).
/// unwrap_or_default()`) silently turned any of those into an EMPTY `tx_hashes`, which is fail-closed
/// only by accident: [`EngineController::advance`]'s consolidation check does reject a length mismatch
/// against a non-empty `expected_hashes`, but a block whose forced tx list was *also* empty would
/// consolidate against a block this response never actually described.
fn parse_tx_hashes(block: &serde_json::Value) -> Result<Vec<B256>, PipelineError> {
    let arr = block["transactions"].as_array().ok_or_else(|| {
        PipelineError::Temporary("eth_getBlockByNumber: bad transactions".to_string())
    })?;
    arr.iter()
        .map(|t| {
            t.as_str()
                .ok_or_else(|| {
                    PipelineError::Temporary("eth_getBlockByNumber: bad transactions".to_string())
                })?
                .parse::<B256>()
                .map_err(|_| {
                    PipelineError::Temporary("eth_getBlockByNumber: bad transactions".to_string())
                })
        })
        .collect()
}

/// [`MockEngineApi`]: real payload shapes (`alloy_rpc_types_engine`), no network — used by
/// [`EngineController`]'s own unit tests below and by this crate's `tests/` integration tests (not
/// gated behind `#[cfg(test)]`: an integration test binary links only this crate's public API, so this
/// mirrors `rome_zk_sequencer::testutil`'s own always-public pattern).
pub mod mock {
    use super::*;
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};
    use std::collections::HashMap;

    #[derive(Debug, Clone, Default)]
    pub struct Call {
        pub method: &'static str,
    }

    /// Scripted [`EngineApi::new_payload`] responses — every real `PayloadStatusEnum` case a test needs
    /// to exercise (SYNCING/ACCEPTED must be `Temporary`, only INVALID is
    /// `Critical`).
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub enum NewPayloadResponse {
        #[default]
        Valid,
        Syncing,
        Accepted,
        Invalid(String),
    }

    /// A scripted, in-memory [`EngineApi`]: builds a payload deterministically from `(parent_block_hash, timestamp,
    /// transactions)` so two calls with identical inputs produce identical hashes (letting tests assert on hash
    /// equality without a real EVM). `drop_last_tx` lets a test simulate the divergence the strict policy exists to
    /// catch: the engine "executing" fewer txs than the forced list named (as if one had failed at execution and been
    /// dropped, never a decode failure). `reorder_txs` simulates the same-set-different-order divergence the
    /// build-branch identity check closes: a builder that executes this batch's exact tx set but in a different order.
    #[derive(Default)]
    pub struct MockEngineApi {
        pub calls: Vec<Call>,
        pub existing_blocks: HashMap<u64, ExistingBlock>,
        pub drop_last_tx: bool,
        pub reorder_txs: bool,
        pub new_payload_response: NewPayloadResponse,
        /// `Some(msg)` makes [`EngineApi::build_forced_block`] return the same `PipelineError` the real
        /// [`JsonRpcEngineApi`] would for a `testing_buildBlockV1` JSON-RPC `Call` error — routed through
        /// [`map_build_error`] itself (a prior version of this mock
        /// hard-coded `PipelineError::Critical(msg)` here, bypassing `map_build_error` entirely, so a
        /// test asserting "this path is Critical" was actually asserting the mock's own hard-coded
        /// behavior, not the real mapping function's).
        pub build_forced_block_err: Option<String>,
    }

    impl MockEngineApi {
        /// Mirrors a real EL's "next height = parent height + 1" (see [`EngineController`]'s own
        /// doc) — by looking up `parent_block_hash` among the blocks this mock already knows about
        /// (built earlier in this same run, or pre-seeded directly into `existing_blocks` by a test),
        /// rather than a raw call counter. A counter desyncs the moment a caller seeds
        /// `existing_blocks` directly at a non-zero height (the resume-anchor tests: the controller
        /// starts at real height 6 against a mock that has never itself built anything) or retries a
        /// batch attempt (a rewound controller re-asks for a height this mock already built once
        /// — the lookup naturally consolidates instead of double-counting). An unknown parent hash is
        /// genesis (height 0), matching every test that starts a fresh mock from `B256::ZERO`.
        fn height_after(&self, parent_block_hash: B256) -> u64 {
            self.existing_blocks
                .iter()
                .find(|(_, b)| b.block_hash == parent_block_hash)
                .map(|(height, _)| height + 1)
                .unwrap_or(1)
        }
    }

    fn fake_hash(parent: B256, timestamp: u64, txs: &[Bytes]) -> B256 {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(parent.as_slice());
        preimage.extend_from_slice(&timestamp.to_le_bytes());
        for t in txs {
            preimage.extend_from_slice(t);
        }
        keccak256(preimage)
    }

    impl EngineApi for MockEngineApi {
        async fn build_forced_block(
            &mut self,
            parent_block_hash: B256,
            attrs: PayloadAttributes,
            mut transactions: Vec<Bytes>,
        ) -> Result<(ExecutionPayloadV3, RequestsOrHash), PipelineError> {
            self.calls.push(Call {
                method: "build_forced_block",
            });
            if let Some(msg) = &self.build_forced_block_err {
                let call_err = jsonrpsee::core::ClientError::Call(
                    jsonrpsee::types::ErrorObject::owned::<()>(-32000, msg.clone(), None),
                );
                return Err(map_build_error("testing_buildBlockV1", call_err));
            }
            if self.drop_last_tx {
                transactions.pop();
            }
            if self.reorder_txs && transactions.len() >= 2 {
                transactions.swap(0, 1);
            }
            let height = self.height_after(parent_block_hash);
            let block_hash = fake_hash(parent_block_hash, attrs.timestamp, &transactions);
            // Mirrors a real engine's persistent chain state: once built, a later `block_at` for this
            // same height finds it — this is what lets a second derivation pass consolidate instead of
            // rebuilding, exactly as the real `JsonRpcEngineApi` (backed by an actual persistent chain)
            // would behave.
            self.existing_blocks.insert(
                height,
                ExistingBlock {
                    block_hash,
                    timestamp: attrs.timestamp,
                    prev_randao: attrs.prev_randao,
                    gas_limit: attrs.target_gas_limit.unwrap_or_default(),
                    // Same stand-in this mock's built payload reports below — a later consolidation
                    // must see the identical value a first build would have reported.
                    state_root: block_hash,
                    tx_hashes: transactions.iter().map(keccak256).collect(),
                    // Mirrors what a real engine would actually have built, since
                    // `payload_attrs_from` (the ONLY caller of `build_forced_block`) already set these
                    // from the same `canonical_header_rule` a later consolidation checks against.
                    beneficiary: attrs.suggested_fee_recipient,
                    extra_data: Bytes::new(),
                    withdrawals_root: rome_zk_executor_api::EMPTY_WITHDRAWALS,
                    parent_beacon_block_root: attrs.parent_beacon_block_root.unwrap_or(B256::ZERO),
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                },
            );
            let payload = ExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 {
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: parent_block_hash,
                        fee_recipient: attrs.suggested_fee_recipient,
                        state_root: block_hash, // stand-in; MockEngineApi does no real execution.
                        receipts_root: B256::ZERO,
                        logs_bloom: Default::default(),
                        prev_randao: attrs.prev_randao,
                        block_number: height,
                        gas_limit: attrs.target_gas_limit.unwrap_or_default(),
                        gas_used: 0,
                        timestamp: attrs.timestamp,
                        extra_data: Bytes::new(),
                        base_fee_per_gas: alloy_primitives::U256::from(1_000_000_000u64),
                        block_hash,
                        transactions,
                    },
                    withdrawals: vec![],
                },
                blob_gas_used: 0,
                excess_blob_gas: 0,
            };
            Ok((
                payload,
                RequestsOrHash::Requests(alloy_eips::eip7685::Requests::default()),
            ))
        }

        async fn new_payload(
            &mut self,
            payload: ExecutionPayloadV3,
            _requests: RequestsOrHash,
        ) -> Result<PayloadStatus, PipelineError> {
            self.calls.push(Call {
                method: "new_payload",
            });
            let status = match &self.new_payload_response {
                NewPayloadResponse::Valid => PayloadStatusEnum::Valid,
                NewPayloadResponse::Syncing => PayloadStatusEnum::Syncing,
                NewPayloadResponse::Accepted => PayloadStatusEnum::Accepted,
                NewPayloadResponse::Invalid(msg) => PayloadStatusEnum::Invalid {
                    validation_error: msg.clone(),
                },
            };
            let latest_valid_hash = matches!(status, PayloadStatusEnum::Valid)
                .then_some(payload.payload_inner.payload_inner.block_hash);
            Ok(PayloadStatus {
                status,
                latest_valid_hash,
            })
        }

        async fn forkchoice_updated(
            &mut self,
            _state: ForkchoiceState,
        ) -> Result<ForkchoiceUpdated, PipelineError> {
            self.calls.push(Call {
                method: "forkchoice_updated",
            });
            Ok(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid))
        }

        async fn block_at(&mut self, number: u64) -> Result<Option<ExistingBlock>, PipelineError> {
            self.calls.push(Call { method: "block_at" });
            Ok(self.existing_blocks.get(&number).cloned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{MockEngineApi, NewPayloadResponse};
    use super::*;
    use alloy_primitives::{Address, Bytes};
    use rome_zk_executor_api::BlockEnv;

    fn attrs(number: u64, txs: Vec<Bytes>) -> Attributes {
        Attributes {
            chain_id: 200_101,
            env: BlockEnv {
                number,
                timestamp_secs: 1_757_000_000 + number,
                gas_limit: 100_000_000,
                coinbase: Address::ZERO,
                prev_randao: rome_zk_executor_api::prev_randao(200_101, number),
                base_fee: None,
            },
            txs,
        }
    }

    /// An `ExistingBlock` whose rule-fixed fields are exactly what `canonical_header_rule(a.chain_id,
    /// a.env.number, a.env.coinbase)` requires — the "honest" baseline every consolidation test starts
    /// from (mirrors `guest-rome::chain`'s own `honest_header` in the fork), so a test that wants to
    /// break exactly one field can do so with a struct-update over this, attributing a failure to that
    /// one field alone.
    fn honest_existing_block(a: &Attributes, block_hash: B256, state_root: B256) -> ExistingBlock {
        let rule =
            rome_zk_executor_api::canonical_header_rule(a.chain_id, a.env.number, a.env.coinbase);
        ExistingBlock {
            block_hash,
            timestamp: a.env.timestamp_secs,
            prev_randao: a.env.prev_randao,
            gas_limit: a.env.gas_limit,
            state_root,
            tx_hashes: a.txs.iter().map(keccak256).collect(),
            beneficiary: rule.beneficiary,
            extra_data: rule.extra_data,
            withdrawals_root: rule.withdrawals_root,
            parent_beacon_block_root: rule.parent_beacon_block_root,
            blob_gas_used: rule.blob_gas_used,
            excess_blob_gas: rule.excess_blob_gas,
        }
    }

    /// `payload_attrs_from`'s rule-fixed fields must equal the real,
    /// independently known values (pinned as literals here, NOT re-derived via a second
    /// `canonical_header_rule` call in the test — that would stay green even if the rule's own constants
    /// changed, proving nothing; see `rome-zk-executor-api::canonical_header_rule_matches_the_real_tiber_header_fixture`
    /// for where these literals come from). Mutation: change
    /// `canonical_header_rule`'s `parent_beacon_block_root` from `B256::ZERO` to anything else and this
    /// test goes red.
    #[test]
    fn payload_attrs_from_reads_every_rule_fixed_field_from_the_shared_rule() {
        let a = attrs(11, vec![Bytes::from_static(b"tx")]);
        let built = payload_attrs_from(&a);
        assert_eq!(
            built.prev_randao,
            rome_zk_executor_api::prev_randao(200_101, 11)
        );
        assert_eq!(built.suggested_fee_recipient, Address::ZERO);
        assert_eq!(built.parent_beacon_block_root, Some(B256::ZERO));
        assert_eq!(built.withdrawals, Some(vec![]));
        assert_eq!(built.timestamp, a.env.timestamp_secs);
        assert_eq!(built.target_gas_limit, Some(a.env.gas_limit));
    }

    /// A different chain-config fee recipient (`attrs.env.coinbase`) must change
    /// `suggested_fee_recipient` identically.
    #[test]
    fn payload_attrs_from_beneficiary_follows_env_coinbase() {
        let mut a = attrs(11, vec![]);
        a.env.coinbase = Address::repeat_byte(0xCD);
        let built = payload_attrs_from(&a);
        assert_eq!(built.suggested_fee_recipient, Address::repeat_byte(0xCD));
    }

    #[tokio::test]
    async fn advance_builds_submits_and_canonicalizes_a_block() {
        let mock = MockEngineApi::default();
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(
            1,
            vec![Bytes::from_static(b"tx-a"), Bytes::from_static(b"tx-b")],
        );
        let outcome = ctrl.advance(&a).await.unwrap();
        assert!(!outcome.consolidated);
        assert_eq!(
            outcome.block_number, 1,
            "real height is parent_height+1, decoupled from attrs.env.number"
        );
        assert_ne!(outcome.block_hash, B256::ZERO);
        assert_eq!(ctrl.parent_hash(), outcome.block_hash);
    }

    #[tokio::test]
    async fn a_rejected_new_payload_is_critical() {
        let mock = MockEngineApi {
            new_payload_response: NewPayloadResponse::Invalid("scripted rejection".into()),
            ..Default::default()
        };
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// SYNCING/ACCEPTED are ordinary engine states, not a strict-validity
    /// fault — must retry (`Temporary`), never stop the pipeline (`Critical`). Every
    /// non-`Valid` status (including these) used to be `Critical`.
    #[tokio::test]
    async fn new_payload_syncing_is_temporary_not_critical() {
        let mock = MockEngineApi {
            new_payload_response: NewPayloadResponse::Syncing,
            ..Default::default()
        };
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Temporary(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn new_payload_accepted_is_temporary_not_critical() {
        let mock = MockEngineApi {
            new_payload_response: NewPayloadResponse::Accepted,
            ..Default::default()
        };
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Temporary(_)), "got {err:?}");
    }

    /// A `testing_buildBlockV1` JSON-RPC `Call` error (the server rejecting the
    /// forced tx list itself) is `Critical`, not the blanket `Temporary` that let it retry forever.
    #[tokio::test]
    async fn a_build_forced_block_call_error_is_critical_not_temporary() {
        let mock = MockEngineApi {
            build_forced_block_err: Some("tx failed to execute".into()),
            ..Default::default()
        };
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// The executed block's tx set diverging from the frame's tx set (as if the
    /// engine had silently dropped a tx during execution) is Critical, never a silent skip.
    #[tokio::test]
    async fn a_dropped_tx_at_execution_is_critical_strict_policy() {
        let mock = MockEngineApi {
            drop_last_tx: true,
            ..Default::default()
        };
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(
            1,
            vec![Bytes::from_static(b"tx-a"), Bytes::from_static(b"tx-b")],
        );
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// The build branch's post-build identity check must be as strong as
    /// consolidation's — the SAME tx set, reordered, is Critical, not a silent pass. The
    /// old build-branch check was `block_number == target_height` plus an UNORDERED
    /// `tx_set_eq`, so a builder that executed this batch's exact txs out of order went undetected.
    #[tokio::test]
    async fn a_reordered_tx_set_at_build_time_is_critical_not_silently_accepted() {
        let mock = MockEngineApi {
            reorder_txs: true,
            ..Default::default()
        };
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let a = attrs(
            1,
            vec![Bytes::from_static(b"tx-a"), Bytes::from_static(b"tx-b")],
        );
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// A [`MockEngineApi`] pre-seeded with a genesis entry at real height 0 — mirrors the real engine
    /// (which always has a real genesis at height 0), needed because
    /// [`EngineController::from_engine_head`] reads `block_at(0)` (seed from the
    /// engine's real genesis, never a hardcoded `B256::ZERO`).
    fn mock_with_genesis(genesis_hash: B256) -> MockEngineApi {
        let mut mock = MockEngineApi::default();
        mock.existing_blocks.insert(
            0,
            ExistingBlock {
                block_hash: genesis_hash,
                timestamp: 0,
                prev_randao: B256::ZERO,
                gas_limit: 0,
                state_root: B256::ZERO,
                tx_hashes: vec![],
                // Real height 0 is never checked against `canonical_header_rule` — `advance`'s
                // consolidation only ever queries `target_height >= 1` (module doc) — so these are
                // harmless placeholders, not asserted-against values.
                beneficiary: Address::ZERO,
                extra_data: Bytes::new(),
                withdrawals_root: rome_zk_executor_api::EMPTY_WITHDRAWALS,
                parent_beacon_block_root: B256::ZERO,
                blob_gas_used: 0,
                excess_blob_gas: 0,
            },
        );
        mock
    }

    /// A controller synced from an engine that already holds blocks 1..=3 (real height)
    /// walks forward from genesis and consolidates each one — it must never attempt to rebuild 1..=3, and
    /// ends up ready to build real height 4 for real.
    ///
    /// Each consolidated outcome's `state_root` must be the real, non-zero
    /// value `block_at` reported for that height — `advance`'s consolidation
    /// branch once hardcoded `B256::ZERO` regardless of what the engine actually had.
    #[tokio::test]
    async fn restart_resumes_after_the_engines_existing_blocks_without_rebuilding_them() {
        let mut mock = mock_with_genesis(B256::ZERO);
        let state_roots: Vec<B256> = (1..=3u64)
            .map(|h| B256::repeat_byte(0x50 + h as u8))
            .collect();
        for h in 1..=3u64 {
            let a = attrs(h, vec![]);
            mock.existing_blocks.insert(
                h,
                honest_existing_block(
                    &a,
                    B256::repeat_byte(h as u8),
                    state_roots[(h - 1) as usize],
                ),
            );
        }

        let mut ctrl = EngineController::from_engine_head(mock).await.unwrap();
        assert_eq!(
            ctrl.next_height(),
            1,
            "seeded from real genesis, not wherever the chain has advanced to"
        );

        for h in 1..=3u64 {
            let a = attrs(h, vec![]);
            let outcome = ctrl.advance(&a).await.unwrap();
            assert!(
                outcome.consolidated,
                "block {h} must consolidate, not build"
            );
            assert_ne!(
                outcome.state_root,
                B256::ZERO,
                "block {h}: a consolidated block's state_root must not go missing"
            );
            assert_eq!(
                outcome.state_root,
                state_roots[(h - 1) as usize],
                "block {h}: must be the exact value the engine reported, not a stand-in"
            );
        }
        assert_eq!(ctrl.next_height(), 4, "ready to build real height 4 next");
        assert!(
            ctrl.engine
                .calls
                .iter()
                .all(|c| c.method != "build_forced_block"),
            "never rebuilds a block the engine already has"
        );
    }

    #[tokio::test]
    async fn consolidation_skips_rebuilding_a_block_already_on_the_engines_chain() {
        let mut mock = MockEngineApi::default();
        let existing_hash = B256::repeat_byte(0x42);
        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        mock.existing_blocks.insert(
            1,
            honest_existing_block(&a, existing_hash, B256::repeat_byte(0x43)),
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let outcome = ctrl.advance(&a).await.unwrap();
        assert!(outcome.consolidated);
        assert_eq!(outcome.block_hash, existing_hash);
        assert_eq!(outcome.state_root, B256::repeat_byte(0x43));
        // Consolidation must not have called build/new_payload/forkchoice_updated.
        assert!(ctrl.engine.calls.iter().all(|c| c.method == "block_at"));
    }

    #[tokio::test]
    async fn a_mismatched_existing_block_is_critical_not_silently_reused() {
        let mut mock = MockEngineApi::default();
        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                tx_hashes: vec![keccak256(Bytes::from_static(b"different-tx"))],
                ..honest_existing_block(&a, B256::repeat_byte(0x99), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// The same ordered tx list at a DIFFERENT timestamp must
    /// still be Critical — consolidation compares the full committed identity, not tx hashes alone. Only
    /// `tx_set_eq` used to be checked, so this consolidated silently.
    #[tokio::test]
    async fn same_txs_but_different_timestamp_is_critical_not_consolidated() {
        let mut mock = MockEngineApi::default();
        let tx = Bytes::from_static(b"tx-a");
        let a = attrs(1, vec![tx.clone()]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                timestamp: a.env.timestamp_secs + 1, // the one field that differs
                ..honest_existing_block(&a, B256::repeat_byte(0x77), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// A resumed node must not silently accept a
    /// consolidated block whose `beneficiary` disagrees with `canonical_header_rule` — `identity_matches`
    /// used to look no further than timestamp/prev_randao/gas_limit/tx set, so a stale block built under a
    /// different (still consensus-valid) fee recipient consolidated without complaint.
    /// Against the old six-field `identity_matches` this consolidated successfully; the test requires
    /// `Critical`.
    #[tokio::test]
    async fn a_consolidated_block_with_a_wrong_beneficiary_is_critical() {
        let mut mock = MockEngineApi::default();
        let tx = Bytes::from_static(b"tx-a");
        let a = attrs(1, vec![tx.clone()]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                beneficiary: Address::repeat_byte(0xAB), // wrong — rule says ZERO for this attrs()
                ..honest_existing_block(&a, B256::repeat_byte(0x11), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// The same check, on `extra_data` — the rule requires it empty on this chain.
    #[tokio::test]
    async fn a_consolidated_block_with_a_wrong_extra_data_is_critical() {
        let mut mock = MockEngineApi::default();
        let tx = Bytes::from_static(b"tx-a");
        let a = attrs(1, vec![tx.clone()]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                extra_data: Bytes::from_static(b"evil"),
                ..honest_existing_block(&a, B256::repeat_byte(0x12), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// The same check, on `withdrawals_root` — must equal `EMPTY_WITHDRAWALS`.
    #[tokio::test]
    async fn a_consolidated_block_with_a_wrong_withdrawals_root_is_critical() {
        let mut mock = MockEngineApi::default();
        let tx = Bytes::from_static(b"tx-a");
        let a = attrs(1, vec![tx.clone()]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                withdrawals_root: B256::repeat_byte(0xEF),
                ..honest_existing_block(&a, B256::repeat_byte(0x13), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// The same check, on `parent_beacon_block_root` — must be ZERO (no beacon chain behind this
    /// rollup).
    #[tokio::test]
    async fn a_consolidated_block_with_a_wrong_parent_beacon_block_root_is_critical() {
        let mut mock = MockEngineApi::default();
        let tx = Bytes::from_static(b"tx-a");
        let a = attrs(1, vec![tx.clone()]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                parent_beacon_block_root: B256::repeat_byte(0x99),
                ..honest_existing_block(&a, B256::repeat_byte(0x14), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// The same check, on `blob_gas_used`/`excess_blob_gas` — must both be 0 (no blobs on this chain).
    #[tokio::test]
    async fn a_consolidated_block_with_nonzero_blob_gas_fields_is_critical() {
        let mut mock = MockEngineApi::default();
        let tx = Bytes::from_static(b"tx-a");
        let a = attrs(1, vec![tx.clone()]);
        mock.existing_blocks.insert(
            1,
            ExistingBlock {
                blob_gas_used: 1,
                excess_blob_gas: 1,
                ..honest_existing_block(&a, B256::repeat_byte(0x15), B256::ZERO)
            },
        );
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let err = ctrl.advance(&a).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)), "got {err:?}");
    }

    /// On the exact mapping function [`JsonRpcEngineApi`] uses (not just the
    /// mock's simulated knob above): a real JSON-RPC `Call` error is Critical; a transport/timeout error
    /// stays Temporary.
    #[test]
    fn map_build_error_splits_call_errors_critical_from_transport_errors_temporary() {
        let call_err = jsonrpsee::core::ClientError::Call(
            jsonrpsee::types::ErrorObject::owned::<()>(-32000, "tx failed to execute", None),
        );
        assert!(matches!(
            map_build_error("testing_buildBlockV1", call_err),
            PipelineError::Critical(_)
        ));

        let timeout_err = jsonrpsee::core::ClientError::RequestTimeout;
        assert!(matches!(
            map_build_error("testing_buildBlockV1", timeout_err),
            PipelineError::Temporary(_)
        ));
    }

    /// The auth-port calls — `engine_newPayloadV4`,
    /// `engine_forkchoiceUpdatedV3`, `eth_getBlockByNumber` — must route through the SAME split as
    /// `testing_buildBlockV1`, not wrap every error (including a permanent server-side rejection like
    /// -38005 `UnsupportedFork`) in `Temporary` and retry it forever at 1 Hz.
    #[test]
    fn map_build_error_splits_call_errors_for_every_auth_port_method_by_name() {
        for method in [
            "engine_newPayloadV4",
            "engine_forkchoiceUpdatedV3",
            "eth_getBlockByNumber",
        ] {
            let call_err = jsonrpsee::core::ClientError::Call(
                jsonrpsee::types::ErrorObject::owned::<()>(-38005, "unsupported fork", None),
            );
            assert!(
                matches!(
                    map_build_error(method, call_err),
                    PipelineError::Critical(_)
                ),
                "{method}: a Call error must be Critical, not an eternal retry"
            );

            let timeout_err = jsonrpsee::core::ClientError::RequestTimeout;
            assert!(
                matches!(
                    map_build_error(method, timeout_err),
                    PipelineError::Temporary(_)
                ),
                "{method}: a transport error must stay Temporary"
            );
        }
    }

    /// A well-formed `transactions` array of real hashes
    /// parses in order.
    #[test]
    fn parse_tx_hashes_reads_a_well_formed_array_in_order() {
        let a = B256::repeat_byte(0xaa);
        let b = B256::repeat_byte(0xbb);
        let block = serde_json::json!({ "transactions": [format!("{a}"), format!("{b}")] });
        assert_eq!(parse_tx_hashes(&block).unwrap(), vec![a, b]);
    }

    /// A missing `transactions` field is Temporary, not a silent empty `Vec` (the old code's
    /// `.unwrap_or_default()` made this `Ok(vec![])`).
    #[test]
    fn parse_tx_hashes_missing_field_is_temporary() {
        let block = serde_json::json!({});
        assert!(matches!(
            parse_tx_hashes(&block),
            Err(PipelineError::Temporary(_))
        ));
    }

    /// A non-array `transactions` field is Temporary, not a silent empty `Vec`.
    #[test]
    fn parse_tx_hashes_non_array_field_is_temporary() {
        let block = serde_json::json!({ "transactions": "not-an-array" });
        assert!(matches!(
            parse_tx_hashes(&block),
            Err(PipelineError::Temporary(_))
        ));
    }

    /// One unparsable hash in an otherwise well-formed array is Temporary, not silently dropped
    /// (the old code's `filter_map` silently skipped it, shortening `tx_hashes`
    /// instead of surfacing the malformed response).
    #[test]
    fn parse_tx_hashes_one_bad_hash_is_temporary_not_silently_dropped() {
        let good = B256::repeat_byte(0xaa);
        let block = serde_json::json!({ "transactions": [format!("{good}"), "not-a-hash"] });
        assert!(matches!(
            parse_tx_hashes(&block),
            Err(PipelineError::Temporary(_))
        ));
    }

    /// The `header == env.number` premise is asserted explicitly, by name, not
    /// just relied upon — a caller that (by construction bug) hands `advance` an env whose number does
    /// not correspond to the real height this controller is about to build must get a named Critical,
    /// not a confusing downstream failure.
    #[tokio::test]
    async fn a_mismatched_env_number_is_critical_by_name() {
        let mock = MockEngineApi::default();
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0); // next_height == 1
        let a = attrs(5, vec![]); // should be 1, not 5
        let err = ctrl.advance(&a).await.unwrap_err();
        match err {
            PipelineError::Critical(msg) => {
                assert!(msg.contains("env.number"))
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    /// The primitive `crate::pipeline::DerivePipeline::step` builds its
    /// atomic-batch-attempt fix on — a snapshotted position round-trips exactly.
    #[tokio::test]
    async fn rewind_to_restores_a_previously_snapshotted_position() {
        let mock = MockEngineApi::default();
        let mut ctrl = EngineController::new(mock, B256::ZERO, 0);
        let pos_before = ctrl.position();
        assert_eq!(pos_before, (B256::ZERO, 1));

        let a = attrs(1, vec![Bytes::from_static(b"tx-a")]);
        ctrl.advance(&a).await.unwrap();
        assert_ne!(
            ctrl.position(),
            pos_before,
            "advance must have moved the position"
        );

        ctrl.rewind_to(pos_before);
        assert_eq!(ctrl.position(), pos_before);
    }
}
