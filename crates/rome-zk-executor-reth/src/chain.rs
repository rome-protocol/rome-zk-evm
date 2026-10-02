//! Genesis loading and `ProviderFactory` bring-up.
//!
//! Mirrors `reth_provider::test_utils::create_test_provider_factory_with_chain_spec_and_db_args`
//! (source: `crates/storage/provider/src/test_utils/mod.rs`, this crate's module doc for the exact
//! path) — same real `reth_db::init_db` MDBX, the same `StaticFileProviderBuilder` and
//! `RocksDBBuilder` wiring — pointed at a real, persistent datadir instead of `tempdir_path()` (that
//! helper wraps the database in `TempDatabase`, which deletes the directory on drop; production
//! needs the opposite).

use std::path::Path;
use std::sync::Arc;

use alloy_genesis::Genesis;
use reth_chainspec::ChainSpec;
use reth_db::mdbx::DatabaseArguments;
use reth_db::DatabaseEnv;
use reth_node_ethereum::EthereumNode;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::providers::{RocksDBBuilder, StaticFileProviderBuilder};
use reth_provider::ProviderFactory;

/// The concrete node-types adapter this crate builds every `ProviderFactory` against: reth's own
/// `EthereumNode` primitives (real `TransactionSigned`/`Block`/`Receipt` shapes — the same ones a
/// stock reth binary uses) over a real, on-disk MDBX `DatabaseEnv`.
pub type RethTypes = NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>;

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("read genesis {path:?}: {source}")]
    ReadGenesis {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("parse genesis {path:?}: {source}")]
    ParseGenesis {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
    #[error("open db at {path:?}: {source}")]
    OpenDb {
        path: std::path::PathBuf,
        source: eyre::Error,
    },
    #[error("create static file provider: {0}")]
    StaticFiles(String),
    #[error("create rocksdb provider: {0}")]
    RocksDb(String),
    #[error("create provider factory: {0}")]
    ProviderFactory(String),
    #[error("init genesis: {0}")]
    InitGenesis(String),
}

/// Read and parse a genesis JSON file (the shape the devnet genesis template produces
/// once its dev account address placeholder is substituted) into `alloy_genesis::Genesis`.
pub fn genesis_from_path(path: &Path) -> Result<Genesis, ChainError> {
    let text = std::fs::read_to_string(path).map_err(|source| ChainError::ReadGenesis {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| ChainError::ParseGenesis {
        path: path.to_path_buf(),
        source,
    })
}

/// Open (creating on first run) a real MDBX-backed `ProviderFactory` at `datadir`, and write the
/// genesis block if it is not already there (`reth_db_common::init::init_genesis` is idempotent —
/// "write the genesis block if it has not already been written").
pub fn open_provider_factory(
    datadir: &Path,
    chain_spec: Arc<ChainSpec>,
) -> Result<ProviderFactory<RethTypes>, ChainError> {
    let db_path = datadir.join("db");
    let static_files_path = datadir.join("static_files");
    let rocksdb_path = datadir.join("rocksdb");
    std::fs::create_dir_all(&static_files_path).ok();

    let db = reth_db::init_db(&db_path, DatabaseArguments::default()).map_err(|source| {
        ChainError::OpenDb {
            path: db_path.clone(),
            source,
        }
    })?;
    let db: Arc<DatabaseEnv> = Arc::new(db);

    let static_file_provider = StaticFileProviderBuilder::read_write(static_files_path)
        .build()
        .map_err(|e| ChainError::StaticFiles(e.to_string()))?;
    let rocksdb_provider = RocksDBBuilder::new(&rocksdb_path)
        .with_default_tables()
        .build()
        .map_err(|e| ChainError::RocksDb(e.to_string()))?;

    let factory = ProviderFactory::<RethTypes>::new(
        db,
        chain_spec,
        static_file_provider,
        rocksdb_provider,
        reth_tasks::Runtime::test(),
    )
    .map_err(|e| ChainError::ProviderFactory(e.to_string()))?;

    reth_db_common::init::init_genesis(&factory)
        .map_err(|e| ChainError::InitGenesis(e.to_string()))?;

    Ok(factory)
}
