//! Test-only helpers shared between this crate's own unit tests and its `tests/` integration tests.
//! Always public (not gated behind `#[cfg(test)]`): an integration test binary links only this crate's
//! public API — mirrors `rome_zk_sequencer::testutil`'s own always-public pattern. See
//! [`crate::engine::mock`] for [`crate::engine::EngineApi`]'s own scripted test double.

use std::collections::HashMap;

use solana_program::pubkey::Pubkey;

use crate::reader::AccountReader;
use crate::PipelineError;

/// An in-memory [`AccountReader`] driven entirely by a test's own script — no Solana process
/// involved. The real, on-chain-backed path is exercised separately by
/// `tests/real_program_inbox.rs`.
#[derive(Default, Clone)]
pub struct FakeAccountReader {
    pub accounts: HashMap<Pubkey, Vec<u8>>,
}

impl AccountReader for FakeAccountReader {
    async fn get_account_data(&mut self, pubkey: Pubkey) -> Result<Option<Vec<u8>>, PipelineError> {
        Ok(self.accounts.get(&pubkey).cloned())
    }
}
