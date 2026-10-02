//! Real-BPF tests for `InitVault`/`Fund`. Run:
//! `cargo build-sbf --manifest-path programs/zk-bridge/Cargo.toml --sbf-out-dir <workspace>/target/deploy`,
//! `cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml --sbf-out-dir <workspace>/target/deploy`,
//! and build the real SPL Token program `.so` (see `README.md`) before `cargo test -p zk-bridge`.

mod common;

use common::*;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

/// `init_vault_creates_a_pda_owned_token_account`: the real `InitVault` instruction, real BPF, real SPL
/// Token program CPI (`InitializeAccount3`), signed by the real chain authority (root.authority) — the
/// vault's SPL account exists afterward, owned by the SPL Token program, with `mint`/`owner` set to exactly
/// what `InitVault` was told; `vault_config` records the same mint/decimals/settlement program and the
/// chain authority as `authority`.
#[tokio::test]
async fn init_vault_creates_a_pda_owned_token_account() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let chain_authority = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    // Pinned, not `Pubkey::new_unique()`: `vault_token_pda` is seeded by `mint`, so a random mint makes
    // this test's own measured CU vary run to run with that PDA's bump-seed search depth — see
    // `rome_zk_testkit::fixed_mint_pubkey`'s doc.
    let mint = rome_zk_testkit::fixed_mint_pubkey();
    pt.add_account(mint, mint_account(9));
    pt.add_account(root_pda(), root_account(&chain_authority.pubkey()));
    let mut ctx = pt.start_with_context().await;

    let ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        settlement_program_id(),
        &chain_authority.pubkey(),
    );
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[&chain_authority]).await;
    result.expect("InitVault must succeed");
    eprintln!("InitVault (real BPF, incl. the InitializeAccount3 CPI) consumed {cu} CU");
    // A ceiling, not an exact-equality pin: with `mint` pinned above, this figure IS bit-exact run to
    // run on the same binary (measured 30,753 CU (39,998 before the crate bump) with the chain-authority
    // gate, mint_decimals check, and the settlement_program-keyed PDAs — see the CHANGELOG/README). A
    // ceiling still catches a regression without flaking on an unrelated toolchain rebuild that shifts
    // CU by a handful of units.
    assert!(
        cu <= 55_000,
        "InitVault CU regressed past the reproducible ceiling: {cu}"
    );

    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let cfg_acc = get_account(&mut ctx, vault_config_pda)
        .await
        .expect("vault_config must exist");
    assert_eq!(cfg_acc.owner, bridge_program_id());
    let cfg = zk_bridge_client::decode_vault_config_account(&cfg_acc.data).unwrap();
    assert_eq!(cfg.chain_id, CHAIN_ID);
    assert_eq!(cfg.settlement_program, settlement_program_id());
    assert_eq!(cfg.mint, mint);
    assert_eq!(cfg.mint_decimals, 9);
    assert_eq!(
        cfg.authority,
        chain_authority.pubkey(),
        "vault_config.authority must be the chain authority that signed, not the payer"
    );

    let (vault_token_pda, _) = zk_bridge_client::vault_token_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
        &mint,
    );
    let (vault_authority_pda, _) = zk_bridge_client::vault_authority_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let token_acc = get_account(&mut ctx, vault_token_pda)
        .await
        .expect("vault token account must exist");
    assert_eq!(token_acc.owner, token_program_id());
    // Standard SPL Token Account layout: mint @0..32, owner @32..64, amount @64..72, state (Initialized=1)
    // @108 — see `zk_bridge::token`'s module doc.
    assert_eq!(&token_acc.data[0..32], mint.as_ref());
    assert_eq!(&token_acc.data[32..64], vault_authority_pda.as_ref());
    assert_eq!(decode_token_amount(&token_acc.data), 0);
    assert_eq!(
        token_acc.data[108], 1,
        "InitializeAccount3 must leave the account Initialized"
    );
}

/// `init_vault_by_chain_authority_succeeds`: the minimal case — the real `root.authority` signs
/// `InitVault` and it succeeds (isolates "the real authority is never refused" from
/// `init_vault_creates_a_pda_owned_token_account`'s broader account-shape assertions above).
#[tokio::test]
async fn init_vault_by_chain_authority_succeeds() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let chain_authority = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9));
    pt.add_account(root_pda(), root_account(&chain_authority.pubkey()));
    let mut ctx = pt.start_with_context().await;

    let ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        settlement_program_id(),
        &chain_authority.pubkey(),
    );
    let (result, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[&chain_authority]).await;
    result.expect("the real settlement chain authority must be able to InitVault");
}

/// `init_vault_by_non_chain_authority_is_refused`: a signer whose key is NOT `root.authority` for
/// `CHAIN_ID` must be refused (`NotChainAuthority`) — no vault created. This closes a front-run gap:
/// without this gate, a front-runner naming a hostile `settlement_program` would decide the vault's root
/// of trust for the real chain_id/mint.
#[tokio::test]
async fn init_vault_by_non_chain_authority_is_refused() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let real_chain_authority = Pubkey::new_unique();
    let impostor = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    pt.add_account(impostor.pubkey(), funded_account(50_000_000_000));
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9));
    pt.add_account(root_pda(), root_account(&real_chain_authority));
    let mut ctx = pt.start_with_context().await;

    let ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        settlement_program_id(),
        &impostor.pubkey(),
    );
    let (result, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[&impostor]).await;
    let err = result.expect_err("a signer that is not root.authority must be refused");
    match err {
        solana_sdk::transaction::TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::Custom(c),
        ) => assert_eq!(c, zk_bridge::errors::BridgeError::NotChainAuthority as u32),
        other => panic!("expected NotChainAuthority, got {other:?}"),
    }

    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    assert!(
        get_account(&mut ctx, vault_config_pda).await.is_none(),
        "no vault_config must be created by a refused InitVault"
    );
}

/// `init_vault_root_not_owned_by_settlement_is_refused`: the `root` account supplied at the expected PDA
/// address is owned by some OTHER program, not `args.settlement_program` — refused before
/// `root.authority` is ever read (an attacker cannot forge a root account under a program they do not
/// control and have it accepted).
#[tokio::test]
async fn init_vault_root_not_owned_by_settlement_is_refused() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let attacker = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    pt.add_account(attacker.pubkey(), funded_account(50_000_000_000));
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9));
    // A root account at the RIGHT address, with the attacker recorded as authority, but owned by a
    // program OTHER than settlement_program_id() — e.g. the attacker's own program.
    let not_settlement = Pubkey::new_unique();
    pt.add_account(
        root_pda(),
        rome_zk_testkit::root_account_with_authority(CHAIN_ID, &attacker.pubkey(), not_settlement),
    );
    let mut ctx = pt.start_with_context().await;

    let ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        settlement_program_id(),
        &attacker.pubkey(),
    );
    let (result, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[&attacker]).await;
    let err =
        result.expect_err("a root account not owned by args.settlement_program must be refused");
    match err {
        solana_sdk::transaction::TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::IncorrectProgramId,
        ) => {}
        other => panic!("expected IncorrectProgramId, got {other:?}"),
    }

    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    assert!(
        get_account(&mut ctx, vault_config_pda).await.is_none(),
        "no vault_config must be created by a refused InitVault"
    );
}

/// `init_vault_with_wrong_mint_decimals_is_refused`: the mint's real decimals byte (offset 44) is 9,
/// but `args.mint_decimals` is 18 — refused (`MintDecimalsMismatch`) rather than silently stored, which
/// would mis-scale every future `ReleaseExit` payout by `10^9`.
#[tokio::test]
async fn init_vault_with_wrong_mint_decimals_is_refused() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let chain_authority = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9)); // the REAL mint is 9-decimal
    pt.add_account(root_pda(), root_account(&chain_authority.pubkey()));
    let mut ctx = pt.start_with_context().await;

    let ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        18, // wrong: does not match the mint's own decimals byte
        settlement_program_id(),
        &chain_authority.pubkey(),
    );
    let (result, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[&chain_authority]).await;
    let err = result.expect_err("mint_decimals must be checked against the mint's real decimals");
    match err {
        solana_sdk::transaction::TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::Custom(c),
        ) => assert_eq!(
            c,
            zk_bridge::errors::BridgeError::MintDecimalsMismatch as u32
        ),
        other => panic!("expected MintDecimalsMismatch, got {other:?}"),
    }

    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    assert!(
        get_account(&mut ctx, vault_config_pda).await.is_none(),
        "no vault_config must be created by a refused InitVault"
    );
}

/// `init_vault_twice_is_refused`: a second `InitVault` for the same chain must not silently overwrite the
/// first — `VaultAlreadyInitialized`.
#[tokio::test]
async fn init_vault_twice_is_refused() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let chain_authority = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9));
    pt.add_account(root_pda(), root_account(&chain_authority.pubkey()));
    let mut ctx = pt.start_with_context().await;

    let ix = || {
        zk_bridge_client::init_vault_ix(
            &bridge_program_id(),
            &payer.pubkey(),
            CHAIN_ID,
            mint,
            9,
            settlement_program_id(),
            &chain_authority.pubkey(),
        )
    };
    let (r1, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix()], &payer, &[&chain_authority]).await;
    r1.expect("first InitVault must succeed");

    let (r2, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix()], &payer, &[&chain_authority]).await;
    let err = r2.expect_err("a second InitVault for the same chain must be refused");
    match err {
        solana_sdk::transaction::TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::Custom(c),
        ) => assert_eq!(
            c,
            zk_bridge::errors::BridgeError::VaultAlreadyInitialized as u32
        ),
        other => panic!("expected VaultAlreadyInitialized, got {other:?}"),
    }
}

/// `fund_increases_vault_balance`: a permissionless SPL transfer into the vault, real BPF. Hand-builds the
/// vault's config + token account directly (this test is not about `InitVault`'s own ceremony).
#[tokio::test]
async fn fund_increases_vault_balance() {
    // Pinned, not `Pubkey::new_unique()`: `vault_token_pda` is seeded by the mint, so a random mint moves the
    // CU printed below with the bump search (8,902 filtered vs 7,402 in a full run, same code).
    let mint = rome_zk_testkit::fixed_mint_pubkey();
    let (vault_authority, _) = zk_bridge_client::vault_authority_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (vault_config_pda, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (vault_token_pda, _) = zk_bridge_client::vault_token_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
        &mint,
    );

    let funder = Keypair::new();
    let funder_token = Pubkey::new_unique();

    let mut pt = base_program_test();
    pt.add_account(funder.pubkey(), funded_account(10_000_000_000));
    pt.add_account(
        vault_config_pda,
        vault_config_account(settlement_program_id(), mint, 9, Pubkey::new_unique()),
    );
    pt.add_account(vault_token_pda, token_account(&mint, &vault_authority, 0));
    pt.add_account(funder_token, token_account(&mint, &funder.pubkey(), 5_000));
    let mut ctx = pt.start_with_context().await;

    let ix = zk_bridge_client::fund_ix(
        &bridge_program_id(),
        &funder.pubkey(),
        &funder_token,
        &settlement_program_id(),
        CHAIN_ID,
        &mint,
        2_000,
    );
    let (result, cu, _logs) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &funder, &[]).await;
    result.expect("Fund must succeed");
    eprintln!("Fund (real BPF, incl. the SPL Transfer CPI) consumed {cu} CU");

    let vault_after =
        decode_token_amount(&get_account(&mut ctx, vault_token_pda).await.unwrap().data);
    let funder_after =
        decode_token_amount(&get_account(&mut ctx, funder_token).await.unwrap().data);
    assert_eq!(vault_after, 2_000);
    assert_eq!(funder_after, 3_000);
}

/// `attacker_first_does_not_lock_out_real_authority`: the un-frontrunnable re-keying.
/// The attacker-first race, RUN real BPF, is closed AT THE CORE. Under the OLD chain_id-only keying the
/// attacker's InitVault SUCCEEDS at the sole per-chain slot and the real authority's later InitVault is
/// refused `VaultAlreadyInitialized` — locked out. With vault PDAs keyed by `[settlement_program,
/// chain_id]`, the attacker's hostile-settlement InitVault still succeeds (the chain-authority gate only
/// proves self-consistency of whatever settlement they name), but at a DIFFERENT address
/// (`pda(hostile_settlement, chain_id)`); the real authority's InitVault naming the REAL settlement then
/// ALSO succeeds, at `pda(real_settlement, chain_id)` — not locked out, because the two calls were never
/// contending for the same address.
#[tokio::test]
async fn attacker_first_does_not_lock_out_real_authority() {
    let mut pt = base_program_test();
    let payer = Keypair::new();
    let real_chain_authority = Keypair::new();
    let attacker_chain_authority = Keypair::new();
    let real_settlement = settlement_program_id();
    let hostile_settlement = Pubkey::new_unique();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    pt.add_account(
        attacker_chain_authority.pubkey(),
        funded_account(50_000_000_000),
    );
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9));
    // the REAL root, owned by the REAL settlement, authority = the real chain authority.
    pt.add_account(root_pda(), root_account(&real_chain_authority.pubkey()));
    // the ATTACKER's own self-consistent root: owned by their OWN (undeployed, but that is not checked
    // here — `InitVault` never CPIs into `settlement_program`) hostile settlement id, with themselves as
    // `authority`.
    let attacker_root_pda = rome_zk_layouts::root::pda(&hostile_settlement, CHAIN_ID).0;
    pt.add_account(
        attacker_root_pda,
        rome_zk_testkit::root_account_with_authority(
            CHAIN_ID,
            &attacker_chain_authority.pubkey(),
            hostile_settlement,
        ),
    );
    let mut ctx = pt.start_with_context().await;

    // --- ATTACKER FIRST: InitVault naming the hostile settlement, signed by their own root.authority ---
    let attacker_ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        hostile_settlement,
        &attacker_chain_authority.pubkey(),
    );
    let (r1, ..) = rome_zk_testkit::send_measuring_cu(
        &mut ctx,
        &[attacker_ix],
        &payer,
        &[&attacker_chain_authority],
    )
    .await;
    r1.expect(
        "the attacker's self-consistent hostile-settlement InitVault succeeds — at ITS OWN address",
    );

    // --- REAL AUTHORITY SECOND: InitVault naming the real settlement, signed by the real root.authority —
    // must NOT be refused VaultAlreadyInitialized: this is a DIFFERENT address than the attacker's. ---
    let real_ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        real_settlement,
        &real_chain_authority.pubkey(),
    );
    let (r2, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[real_ix], &payer, &[&real_chain_authority])
            .await;
    r2.expect(
        "the real chain authority must NOT be locked out by an earlier hostile-settlement InitVault",
    );

    let (attacker_cfg_pda, _) =
        zk_bridge_client::vault_config_pda(&bridge_program_id(), &hostile_settlement, CHAIN_ID);
    let (real_cfg_pda, _) =
        zk_bridge_client::vault_config_pda(&bridge_program_id(), &real_settlement, CHAIN_ID);
    assert_ne!(
        attacker_cfg_pda, real_cfg_pda,
        "the attacker's config and the real config must live at DIFFERENT addresses"
    );

    let attacker_cfg = zk_bridge_client::decode_vault_config_account(
        &get_account(&mut ctx, attacker_cfg_pda).await.unwrap().data,
    )
    .unwrap();
    assert_eq!(attacker_cfg.settlement_program, hostile_settlement);
    assert_eq!(attacker_cfg.authority, attacker_chain_authority.pubkey());

    let real_cfg = zk_bridge_client::decode_vault_config_account(
        &get_account(&mut ctx, real_cfg_pda).await.unwrap().data,
    )
    .unwrap();
    assert_eq!(
        real_cfg.settlement_program, real_settlement,
        "the real chain's config must name the REAL settlement, not be overwritten/blocked by the attacker's"
    );
    assert_eq!(real_cfg.authority, real_chain_authority.pubkey());
}

/// `real_vault_address_is_derivable_from_the_real_settlement_only`: `vault_config_pda(program,
/// real_settlement, chain_id)` is a fixed, publicly-computable address — and an attacker who does NOT
/// control `real_settlement`'s `root.authority` cannot occupy it, no matter what they try, because
/// `InitVault`'s own account-derivation check (`InvalidSeeds`) refuses any `vault_config` account key other
/// than that exact PDA, and the chain-authority gate (`NotChainAuthority`) refuses any signer who is not the
/// real `root.authority`. The address stays un-occupied by anyone but the real authority.
#[tokio::test]
async fn real_vault_address_is_derivable_from_the_real_settlement_only() {
    let (real_cfg_addr_1, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    let (real_cfg_addr_2, _) = zk_bridge_client::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    );
    assert_eq!(
        real_cfg_addr_1, real_cfg_addr_2,
        "the real chain's vault_config address must be a pure function of (program, real_settlement, chain_id)"
    );

    let mut pt = base_program_test();
    let payer = Keypair::new();
    let real_chain_authority = Pubkey::new_unique(); // nobody in this test holds this key
    let attacker = Keypair::new();
    pt.add_account(payer.pubkey(), funded_account(50_000_000_000));
    pt.add_account(attacker.pubkey(), funded_account(50_000_000_000));
    let mint = Pubkey::new_unique();
    pt.add_account(mint, mint_account(9));
    // the real root, owned by the real settlement, authority = the real chain authority — the attacker does
    // not hold this key and cannot sign as it.
    pt.add_account(root_pda(), root_account(&real_chain_authority));
    let mut ctx = pt.start_with_context().await;

    // The attacker targets the REAL address (names the real settlement_program, so their instruction's own
    // `vault_config`/`vault_token`/`vault_authority` accounts resolve to `real_cfg_addr_1`), but signs with
    // their own key instead of the real root.authority.
    let ix = zk_bridge_client::init_vault_ix(
        &bridge_program_id(),
        &payer.pubkey(),
        CHAIN_ID,
        mint,
        9,
        settlement_program_id(),
        &attacker.pubkey(),
    );
    let (result, ..) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &payer, &[&attacker]).await;
    let err = result.expect_err(
        "an attacker who does not hold the real settlement's root.authority key cannot InitVault at the real address",
    );
    match err {
        solana_sdk::transaction::TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::Custom(c),
        ) => assert_eq!(c, zk_bridge::errors::BridgeError::NotChainAuthority as u32),
        other => panic!("expected NotChainAuthority, got {other:?}"),
    }

    assert!(
        get_account(&mut ctx, real_cfg_addr_1).await.is_none(),
        "the real chain's un-frontrunnable address must remain un-occupied by the attacker's attempt"
    );
}
