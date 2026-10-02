//! Operator confirmation tool: opens a datadir the
//! SAME way `RethExecutor::new` does (`open_provider_factory`, the shared bring-up in
//! `crate::chain`) and prints what every storage layer actually believes, without exercising the
//! healer.
//!
//! **This tool never writes to an existing datadir's chain data.** It refuses outright (before
//! `open_provider_factory` is ever called) unless `<datadir>/db/mdbx.dat` already exists — reth's
//! own `reth_db::init_db` CREATES that file (and the genesis block behind it) on a datadir that does
//! not have one yet, so without this check a typo'd path would silently bring a brand-new, empty
//! chain into existence rather than reporting "no such datadir". Once past that check,
//! `open_provider_factory` runs only the idempotent `reth_db_common::init::init_genesis` ("write the
//! genesis block if it has not already been written" — a no-op here, since genesis is already
//! there). Unlike `RethExecutor::new`, this example never calls `ProviderFactory::check_consistency`
//! — that call is documented to potentially heal (write to) the static file segments, and this tool
//! exists specifically to show the datadir's state BEFORE any healer touches it, for manual
//! confirmation against a real torn shape (e.g. a copy of a Tiber datadir pulled down for
//! inspection).
//!
//! Usage:
//! ```text
//! cargo run -p rome-zk-executor-reth --example inspect_datadir -- <datadir> <genesis.json>
//! ```
//! `<datadir>` is the directory containing `db/`, `static_files/`, `rocksdb/` (i.e. the same path a
//! chain's `RethConfig.datadir` names). `<genesis.json>` is only needed to build the `ChainSpec`
//! `open_provider_factory` requires — it is read, never written.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use reth_chainspec::ChainSpec;
use reth_provider::{
    BlockNumReader, HeaderProvider, StageCheckpointReader, StaticFileProviderFactory,
    StaticFileSegment,
};
use rome_zk_executor_reth::{genesis_from_path, open_provider_factory};

fn main() -> eyre::Result<()> {
    let mut args = env::args().skip(1);
    let datadir = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| eyre::eyre!("usage: inspect_datadir <datadir> <genesis.json>"))?;
    let genesis_path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| eyre::eyre!("usage: inspect_datadir <datadir> <genesis.json>"))?;

    let mdbx_dat = datadir.join("db").join("mdbx.dat");
    if !mdbx_dat.exists() {
        eyre::bail!(
            "refusing to inspect {}: {} does not exist — this tool never creates a datadir \
             (a typo'd path would otherwise silently bring a brand-new, empty chain into \
             existence via reth's own init_db)",
            datadir.display(),
            mdbx_dat.display()
        );
    }

    let genesis = genesis_from_path(&genesis_path)?;
    let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));
    let factory = open_provider_factory(&datadir, chain_spec)?;

    println!("datadir: {}", datadir.display());
    println!();

    println!("-- static file segments (highest committed block per segment) --");
    let static_file_provider = factory.static_file_provider();
    for segment in StaticFileSegment::iter() {
        match static_file_provider.get_highest_static_file_block(segment) {
            Some(block) => println!("  {segment:<20} {block}"),
            None => println!("  {segment:<20} (none)"),
        }
    }
    println!();

    let provider = factory.provider()?;
    let best = provider.best_block_number()?;
    let last = provider.last_block_number()?;
    println!("-- MDBX --");
    println!("  best_block_number (StageId::Finish checkpoint) = {best}");
    println!("  last_block_number (static-file frontier)       = {last}");
    println!();

    println!("-- every stage checkpoint --");
    for (name, checkpoint) in provider.get_all_checkpoints()? {
        println!("  {name:<24} block={}", checkpoint.block_number);
    }
    println!();

    println!("-- header presence, best-2..=best+1 --");
    let range_start = best.saturating_sub(2);
    for n in range_start..=(best + 1) {
        let sealed = provider.sealed_header(n)?.is_some();
        let raw = provider.header_by_number(n)?.is_some();
        println!("  block {n:<10} sealed_header={sealed:<5} header_by_number={raw}");
    }

    Ok(())
}
