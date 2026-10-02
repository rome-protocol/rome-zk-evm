//! Re-export of [`rome_zk_profile`] under this crate's historical `profile` module path.
//! `Profile`, `ProfileError`, every `DEFAULT_*` constant, `ProfileIdentity`, `FIRST_BLOCK` and
//! the `profile.json` file layer (`PROFILE_JSON_FILENAME`, `read_profile_json`,
//! `write_profile_identity`) all now live in the standalone `rome-zk-profile` crate — reth-free, so
//! `rome-zk-batcher` and `rome-zk-derive` can depend on it directly without pulling in this crate's
//! `reth` feature. Every existing caller of `crate::profile::*` keeps working unchanged through this
//! re-export.
//!
//! `rome-zk-sequencer`'s own numeric constants (`sealer::SUB_BLOCKS_PER_BLOCK`,
//! `executor::DEFAULT_SUB_BLOCK_GAS_LIMIT`) used to be this profile's source of truth; that
//! was inverted — `rome_zk_profile` now owns the literal defaults, and `sealer`/`executor` re-export
//! them instead (see those modules' own doc comments).

pub use rome_zk_profile::*;

#[cfg(test)]
mod tests {
    /// The sequencer's own `sealer`/`executor` constants must stay in lockstep with the
    /// crate's `rome_zk_profile` defaults now that the numeric homes moved — this is the cross-check the
    /// other direction from `rome-zk-profile`'s own
    /// `default_profile_matches_the_pinned_design_defaults` test. **Mutation m1** (bumping
    /// `rome_zk_profile::DEFAULT_BLOCKS_PER_BATCH` without updating anything else) does not touch this
    /// particular test (it does not assert `blocks_per_batch`) — see `recovery::tests::
    /// a_restart_under_a_changed_blocks_per_batch_is_refused_naming_the_field`, which stays in this crate
    /// and does pin `blocks_per_batch == 10`, for that one.
    #[test]
    fn default_profile_matches_the_sealer_and_executor_re_exports() {
        let p = rome_zk_profile::Profile::default();
        assert_eq!(p.sub_blocks_per_block, crate::sealer::SUB_BLOCKS_PER_BLOCK);
        assert_eq!(
            p.sub_block_gas_limit,
            crate::executor::DEFAULT_SUB_BLOCK_GAS_LIMIT
        );
        assert_eq!(
            p.effective_block_gas_limit(),
            crate::sealer::DEFAULT_BLOCK_GAS_LIMIT
        );
    }
}
