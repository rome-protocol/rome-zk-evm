//! Turns one transaction's raw message into the rows this crate writes. Instruction decoding is
//! `zk_inbox_client::decode_instruction`/`zk_settlement_client::decode_instruction` — this module never re-implements a
//! borsh layout, only interprets the already-decoded enum against the settlement data model. Account positions (which
//! slot in an instruction's `AccountMeta` list carries the chunk/batch PDA) come from `zk_inbox_client`'s
//! `chunk_account_index`/`batch_account_index` — this module never hard-codes those positions itself.
//!
//! **One V1 transaction bundles several instructions** (one V1 tx per frame = `Open +
//! Write + Seal + SealLeaf`) — so decoding a transaction produces a *list* of instruction names (for
//! `settlement_tx_program.ix_kind`) plus any number of derived chunk events and any number of derived
//! batch events (a batch-lifecycle transaction carries `OpenBatch` + one or more `GrowBatch` in the same
//! tx; two `Open`s for two different chunks in one tx must both be recorded).

use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;
use solana_transaction_status_client_types::UiRawMessage;
use zk_inbox_client::{batch_account_index, chunk_account_index, InboxIx};

/// One chunk-lifecycle instruction observed in a transaction, keyed by the chunk PDA account named in
/// that instruction (`zk_inbox_client::chunk_account_index`) — never by `(chain_id, batch, idx)` decoded
/// from instruction data, which `Seal`/`SealLeaf` do not carry (a
/// Seal-only tx has no `Open` in it to source those fields from, and fabricating `(0, 0, 0)` produced a
/// bogus row). `chunk_pda` is the account's own base58 address, exactly as `zk_inbox_client::chunk_pda()`
/// would derive it — `ingest.rs` never invents a chunk row from this alone; only `Opened` ever inserts
/// one, and only by that same key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEvent {
    pub chunk_pda: String,
    pub kind: ChunkEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkEventKind {
    /// `Open`'s own instruction data carries `(chain_id, batch, idx, size)` — the only chunk-lifecycle
    /// instruction that does.
    Opened {
        chain_id: u64,
        batch: u64,
        idx: u32,
        byte_len: u32,
    },
    /// `Seal { len, body_hash }` — the authoritative sealed length and content hash.
    Sealed { byte_len: u32, body_hash: [u8; 32] },
    /// `Close` (rent reclaimed): the chunk PDA account is gone from Solana's own state,
    /// but its DA history must still show as posted, not silently vanish — `inbox_chunk.closed_tx` is set
    /// from this, never a row deletion (799 real `Close` instructions in
    /// the fixture left 799 `inbox_chunk` rows looking permanently live).
    Closed,
}

/// What `ingest::write_page` persists per `(settlement_tx_id, kind)` row so `lifecycle::derive_once` never
/// has to re-fetch or re-decode the transaction body (derive reads the feed, never
/// Solana). Empty for a failed on-chain transaction (nothing to derive from a revert) and for the
/// settlement-program ingester (which has no chunk/batch events of its own yet).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedEvents {
    pub chunk_events: Vec<ChunkEvent>,
    pub batch_events: Vec<BatchEvent>,
    /// `#[serde(default)]` (the additive-only rule): an already-ingested row's `events` JSON predates this
    /// field and has none of it, so it must decode as an empty `Vec`, not fail `derive_once`'s whole row.
    #[serde(default)]
    pub exit_events: Vec<ExitEvent>,
}

/// One batch-lifecycle instruction observed in a transaction. `OpenBatch` carries `chain_id`/`batch`/
/// `expected_count` in its own data; `FinalizeBatch`/`AbandonBatch`/`CloseBatch`/`GrowBatch` carry no
/// batch id in their instruction data at all (design: the target account is named in the accounts list,
/// not the data) -- `batch_pda` is that account's own base58 address, exactly as `zk_inbox_client`'s
/// `batch_pda()` would derive it, and `ingest.rs` resolves it back to `(chain_id, batch)` via the
/// `batch` table's own `batch_pda` column (set the moment this crate first observes that batch's
/// `OpenBatch`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchEvent {
    Opened {
        chain_id: u64,
        batch: u64,
        expected_count: u32,
        batch_pda: String,
    },
    Grown {
        batch_pda: String,
    },
    /// `step` is `FinalizeBatch`'s own instruction argument -- `0` means "transform every remaining leaf
    /// in this call" (`finalize_cursor` jumps straight to `expected_count`); a nonzero `step` advances
    /// `finalize_cursor` by at most that many leaves and may leave the batch still open
    /// (`programs/zk-inbox/src/batch.rs::finalize_batch_inner`: a
    /// partial call was recorded as `finalized` because this field was discarded). Authority-gated
    /// (the batch's stored `authority` must sign) since this program version, may be called
    /// any number of times by that same authority.
    Finalized {
        batch_pda: String,
        step: u32,
    },
    Abandoned {
        batch_pda: String,
    },
    Closed {
        batch_pda: String,
    },
}

/// One exit-lifecycle instruction observed in a transaction: `ProveExit` (28)
/// carries every field an `exit` row needs directly in its own instruction data — `message_hash` is
/// re-derived here off the decoded `ExitMessageArg` the exact same way `rome_zk_layouts::exit::ExitMessage
/// ::message_hash` computes it on-chain, never re-implemented independently. `ConsumeExit` (29) carries
/// `chain_id`/`message_hash` only, and resolves back to an existing `exit` row purely by that key.
/// `window_index` is deliberately absent: it is a runtime `Clock::slot`-derived value `ProveExit` computes
/// on-chain (`window = slot / root.challenge_window_slots`), not an instruction argument, and this watcher
/// never reads Solana account state -- only transaction history (the same documented bound `batch.root`/
/// `batch.acc` already state for the inbox-side accumulator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitEvent {
    Proved {
        chain_id: u64,
        batch: u64,
        message_hash: [u8; 32],
        sol_recipient: [u8; 32],
        amount: u128,
    },
    Released {
        chain_id: u64,
        message_hash: [u8; 32],
    },
}

/// Everything this crate derives from one transaction's inbox-program or settlement-program instructions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedTx {
    pub ix_names: Vec<&'static str>,
    pub chain_id: Option<u64>,
    pub batch_id: Option<u64>,
    pub chunk_events: Vec<ChunkEvent>,
    pub batch_events: Vec<BatchEvent>,
    pub exit_events: Vec<ExitEvent>,
    /// The fee payer -- Solana's message convention (legacy and V1 alike) puts the
    /// primary signer at `account_keys[0]`.
    pub signer: Option<String>,
}

/// Reads the account at `idx` (from `chunk_account_index`/`batch_account_index`) out of `accounts` -- one slot per
/// entry in the instruction's own `accounts` index list, `None` wherever that index did not resolve against the
/// transaction's static `account_keys` (an ALT-loaded key this crate does not decode, or a malformed message). A
/// missing account must never shift every later account left -- `accounts` here keeps its original length and position
/// for exactly that reason, so an unresolvable index yields `None` for *that* slot only, never a stand-in for whatever
/// real account happens to sit at the same position afterward.
fn account_at<'a>(accounts: &[Option<&'a str>], idx: Option<usize>) -> Option<&'a str> {
    idx.and_then(|i| accounts.get(i).copied().flatten())
}

/// Decodes every top-level instruction in `message` whose `programIdIndex` resolves to
/// `inbox_program_id` (base58 string -- avoids a `Pubkey` roundtrip since the message already carries
/// account keys as strings). An instruction this crate's `InboxIx` cannot decode (a future variant,
/// an extension seam) is skipped, not fatal -- this watcher must keep making progress on
/// instructions it does understand rather than wedge the whole page on one it does not (a DB stall or
/// RPC hang must never silently stop this service).
///
/// `settlement_program` is the settlement program this watcher follows. The inbox program is shared by every
/// settlement program and anyone can open a batch or chunk for any chain id under their own, so an instruction is
/// attributed to this chain only when it belongs to `settlement_program`: an `OpenBatch` must name it, and a
/// chunk `Open` must create the account at `chunk_pda(inbox, settlement_program, chain_id, batch, idx)`. Anything
/// else is decoded (its name is still listed) but yields no event. `Seal`, `Close` and the later batch
/// instructions carry no chain or batch of their own; they attach by account address to a row an accepted open
/// created, so a foreign address finds nothing.
pub fn decode_inbox_tx(
    message: &UiRawMessage,
    inbox_program_id: &str,
    settlement_program: &Pubkey,
) -> DecodedTx {
    let mut out = DecodedTx {
        signer: message.account_keys.first().cloned(),
        ..DecodedTx::default()
    };
    let Ok(inbox_key) = inbox_program_id.parse::<Pubkey>() else {
        return out;
    };
    for ins in &message.instructions {
        let Some(program_key) = message.account_keys.get(ins.program_id_index as usize) else {
            continue;
        };
        if program_key != inbox_program_id {
            continue;
        }
        let Ok(raw) = bs58::decode(&ins.data).into_vec() else {
            continue;
        };
        let Ok(decoded) = zk_inbox_client::decode_instruction(&raw) else {
            continue;
        };
        // One entry per instruction account, `None` wherever the index does not resolve -- never
        // compacted (a `filter_map` here silently shifted every later account left,
        // handing `chunk_account_index`/`batch_account_index` the WRONG account for a transaction whose
        // account list an index does not resolve against).
        let accounts: Vec<Option<&str>> = ins
            .accounts
            .iter()
            .map(|&i| message.account_keys.get(i as usize).map(String::as_str))
            .collect();
        let chunk_pda = account_at(&accounts, chunk_account_index(&decoded)).map(str::to_string);
        let batch_pda = account_at(&accounts, batch_account_index(&decoded)).map(str::to_string);

        match decoded {
            InboxIx::Open {
                chain_id,
                batch,
                idx,
                size,
            } => {
                out.ix_names.push("Open");
                let expected_chunk = zk_inbox_client::chunk_pda(
                    &inbox_key,
                    settlement_program,
                    chain_id,
                    batch,
                    idx,
                )
                .0
                .to_string();
                if let Some(chunk_pda) = chunk_pda.filter(|a| *a == expected_chunk) {
                    out.chain_id = Some(chain_id);
                    out.batch_id = Some(batch);
                    out.chunk_events.push(ChunkEvent {
                        chunk_pda,
                        kind: ChunkEventKind::Opened {
                            chain_id,
                            batch,
                            idx,
                            byte_len: size,
                        },
                    });
                }
            }
            InboxIx::Write { .. } => out.ix_names.push("Write"),
            InboxIx::Seal { len, body_hash } => {
                out.ix_names.push("Seal");
                if let Some(chunk_pda) = chunk_pda {
                    out.chunk_events.push(ChunkEvent {
                        chunk_pda,
                        kind: ChunkEventKind::Sealed {
                            byte_len: len,
                            body_hash,
                        },
                    });
                }
            }
            InboxIx::Close => {
                out.ix_names.push("Close");
                if let Some(chunk_pda) = chunk_pda {
                    out.chunk_events.push(ChunkEvent {
                        chunk_pda,
                        kind: ChunkEventKind::Closed,
                    });
                }
            }
            InboxIx::OpenBatch {
                chain_id,
                batch,
                expected_count,
                settlement_program: opened_under,
            } => {
                out.ix_names.push("OpenBatch");
                // A batch opened under another settlement program for the same (chain_id, batch) is not ours.
                if opened_under != *settlement_program {
                    continue;
                }
                out.chain_id = Some(chain_id);
                out.batch_id = Some(batch);
                if let Some(pda) = batch_pda {
                    out.batch_events.push(BatchEvent::Opened {
                        chain_id,
                        batch,
                        expected_count,
                        batch_pda: pda,
                    });
                }
            }
            InboxIx::SealLeaf { .. } => out.ix_names.push("SealLeaf"),
            // `FinalizeBatchV2` is the live finalize; the retired `FinalizeBatch` stays so recorded history
            // still decodes. Both feed the same event: the lifecycle only needs the batch and the `step`.
            InboxIx::FinalizeBatch { step } => {
                out.ix_names.push("FinalizeBatch");
                if let Some(pda) = batch_pda {
                    out.batch_events.push(BatchEvent::Finalized {
                        batch_pda: pda,
                        step,
                    });
                }
            }
            InboxIx::FinalizeBatchV2 { step, .. } => {
                out.ix_names.push("FinalizeBatchV2");
                if let Some(pda) = batch_pda {
                    out.batch_events.push(BatchEvent::Finalized {
                        batch_pda: pda,
                        step,
                    });
                }
            }
            InboxIx::CloseBatch => {
                out.ix_names.push("CloseBatch");
                if let Some(pda) = batch_pda {
                    out.batch_events.push(BatchEvent::Closed { batch_pda: pda });
                }
            }
            InboxIx::AbandonBatch => {
                out.ix_names.push("AbandonBatch");
                if let Some(pda) = batch_pda {
                    out.batch_events
                        .push(BatchEvent::Abandoned { batch_pda: pda });
                }
            }
            InboxIx::GrowBatch { chain_id, batch } => {
                out.ix_names.push("GrowBatch");
                out.chain_id.get_or_insert(chain_id);
                out.batch_id.get_or_insert(batch);
                if let Some(pda) = batch_pda {
                    out.batch_events.push(BatchEvent::Grown { batch_pda: pda });
                }
            }
            InboxIx::InitBatchCursor { chain_id, .. } => {
                out.ix_names.push("InitBatchCursor");
                out.chain_id.get_or_insert(chain_id);
            }
        }
    }
    out
}

/// Decodes every top-level instruction in `message` whose `programIdIndex` resolves to
/// `settlement_program_id`. The fixture never exercises `PostRoot`/`PostRootProved` (the recorded Tiber
/// batches contain no posted root), so only the instruction *name* is recorded for
/// `settlement_tx_program.ix_kind`; per-batch `root_post` derivation is exercised by unit tests on hand-built
/// rows, wired the same way `decode_inbox_tx` wires `batch_events` above, once a real signature exists to
/// pin it against (this crate never invents wire shapes it has not observed on-chain).
pub fn decode_settlement_tx(message: &UiRawMessage, settlement_program_id: &str) -> DecodedTx {
    let mut out = DecodedTx {
        signer: message.account_keys.first().cloned(),
        ..DecodedTx::default()
    };
    for ins in &message.instructions {
        let Some(program_key) = message.account_keys.get(ins.program_id_index as usize) else {
            continue;
        };
        if program_key != settlement_program_id {
            continue;
        }
        let Ok(raw) = bs58::decode(&ins.data).into_vec() else {
            continue;
        };
        let Ok(decoded) = zk_settlement_client::decode_instruction(&raw) else {
            continue;
        };
        use zk_settlement_client::SettleIx;
        let (name, chain_id, batch) = match decoded {
            SettleIx::Init { chain_id, .. } => ("Init", Some(chain_id), None),
            SettleIx::UpdateRoot { .. } => ("UpdateRoot", None, None),
            SettleIx::UpdateRootZisk { .. } => ("UpdateRootZisk", None, None),
            SettleIx::InitChain(args) => ("InitChain", Some(args.chain_id), None),
            SettleIx::InitChainV2(args) => ("InitChainV2", Some(args.chain_id), None),
            SettleIx::PostRoot(args) => ("PostRoot", Some(args.chain_id), Some(args.batch)),
            SettleIx::PostRootProved { args, .. } => {
                ("PostRootProved", Some(args.chain_id), Some(args.batch))
            }
            SettleIx::FinalizeBatch { chain_id, batch } => {
                ("FinalizeBatch", Some(chain_id), Some(batch))
            }
            SettleIx::ClosePending { chain_id, batch } => {
                ("ClosePending", Some(chain_id), Some(batch))
            }
            SettleIx::RootView { chain_id, batch } => ("RootView", Some(chain_id), Some(batch)),
            SettleIx::RejectBatch { chain_id, batch } => {
                ("RejectBatch", Some(chain_id), Some(batch))
            }
            SettleIx::InitGlobalConfig(_) => ("InitGlobalConfig", None, None),
            SettleIx::AllowReservedId { chain_id } => ("AllowReservedId", Some(chain_id), None),
            SettleIx::RevokeReservedId { chain_id } => ("RevokeReservedId", Some(chain_id), None),
            SettleIx::SetFee { chain_id, .. } => ("SetFee", Some(chain_id), None),
            SettleIx::SetTreasury { .. } => ("SetTreasury", None, None),
            SettleIx::MigrateChain { chain_id } => ("MigrateChain", Some(chain_id), None),
            SettleIx::MigrateChainV2 { chain_id, .. } => ("MigrateChainV2", Some(chain_id), None),
            SettleIx::RefundDeposit { chain_id } => ("RefundDeposit", Some(chain_id), None),
            SettleIx::ReclaimChain { chain_id } => ("ReclaimChain", Some(chain_id), None),
            SettleIx::SetGlobalConfig(_) => ("SetGlobalConfig", None, None),
            SettleIx::SetRegistryAuthority { .. } => ("SetRegistryAuthority", None, None),
            SettleIx::ProposeRegistryAuthority { .. } => ("ProposeRegistryAuthority", None, None),
            SettleIx::AcceptRegistryAuthority => ("AcceptRegistryAuthority", None, None),
            SettleIx::SetDriftBound { chain_id, .. } => ("SetDriftBound", Some(chain_id), None),
            SettleIx::SetRegistryEntry { chain_id, .. } => {
                ("SetRegistryEntry", Some(chain_id), None)
            }
            SettleIx::ProposeExitConfig { chain_id, .. } => {
                ("ProposeExitConfig", Some(chain_id), None)
            }
            SettleIx::ActivateExitConfig { chain_id } => {
                ("ActivateExitConfig", Some(chain_id), None)
            }
            SettleIx::ProveExit(args) => {
                let message_hash = exit_message_hash(&args.message);
                out.exit_events.push(ExitEvent::Proved {
                    chain_id: args.chain_id,
                    batch: args.batch,
                    message_hash,
                    sol_recipient: args.message.sol_recipient,
                    amount: args.message.amount,
                });
                ("ProveExit", Some(args.chain_id), Some(args.batch))
            }
            SettleIx::ConsumeExit(args) => {
                out.exit_events.push(ExitEvent::Released {
                    chain_id: args.chain_id,
                    message_hash: args.message_hash,
                });
                ("ConsumeExit", Some(args.chain_id), None)
            }
        };
        out.ix_names.push(name);
        out.chain_id = out.chain_id.or(chain_id);
        out.batch_id = out.batch_id.or(batch);
    }
    out
}

/// `zk_settlement_client::ExitMessageArg` -> its `message_hash()` -- field-for-field into
/// `rome_zk_layouts::exit::ExitMessage`, the single owner of that arithmetic (never re-implemented here),
/// matching `zk-settlement-client`'s own private `message_fields` helper.
fn exit_message_hash(m: &zk_settlement_client::ExitMessageArg) -> [u8; 32] {
    rome_zk_layouts::exit::ExitMessage {
        nonce: m.nonce,
        l2_sender: m.l2_sender,
        sol_recipient: m.sol_recipient,
        asset: m.asset,
        amount: m.amount,
    }
    .message_hash()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::pubkey::Pubkey;

    /// Stand-in settlement program the fixture chain is registered under (inbox accounts are keyed by it).
    const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);
    use solana_sdk::message::{Message, MessageHeader};
    use zk_inbox_client::{
        finalize_batch_ix, open_batch_ix, open_chunk_ix, seal_chunk_ix, write_chunk_ix,
    };

    /// Compiles real builder `Instruction`s into a `UiRawMessage` -- the exact shape a real
    /// `getTransaction` call returns, rather than hand-rolling
    /// discriminant bytes and account lists by hand as this file's tests used to.
    fn raw_message(
        payer: &Pubkey,
        ixs: &[solana_program::instruction::Instruction],
    ) -> UiRawMessage {
        let message = Message::new(ixs, Some(payer));
        UiRawMessage {
            header: MessageHeader {
                num_required_signatures: message.header.num_required_signatures,
                num_readonly_signed_accounts: message.header.num_readonly_signed_accounts,
                num_readonly_unsigned_accounts: message.header.num_readonly_unsigned_accounts,
            },
            account_keys: message.account_keys.iter().map(|k| k.to_string()).collect(),
            recent_blockhash: message.recent_blockhash.to_string(),
            instructions: message
                .instructions
                .iter()
                .map(
                    |ci| solana_transaction_status_client_types::UiCompiledInstruction {
                        program_id_index: ci.program_id_index,
                        accounts: ci.accounts.clone(),
                        data: bs58::encode(&ci.data).into_string(),
                        stack_height: None,
                    },
                )
                .collect(),
            address_table_lookups: None,
            // New field on `UiRawMessage` (API fallout) — `None` matches every
            // other optional field here (no address-table-lookup/transaction-config data in this fixture).
            transaction_config: None,
        }
    }

    /// The design's one-frame-one-tx bundle: `Open` then `Seal` for the same chunk in one transaction
    /// must produce two chunk events attributed to the *same* `chunk_pda` -- `Opened` carrying `Open`'s
    /// own `(chain_id, batch, idx, size)`, `Sealed` carrying `Seal`'s `len`/`body_hash`.
    #[test]
    fn decode_inbox_tx_produces_opened_then_sealed_for_the_same_chunk_pda() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let chunk =
            zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4005, 17).0;

        let ixs = [
            open_chunk_ix(
                &program_id,
                &payer,
                &SETTLEMENT_PROGRAM,
                200_101,
                4005,
                17,
                3_681,
            ),
            seal_chunk_ix(
                &program_id,
                &payer,
                &SETTLEMENT_PROGRAM,
                200_101,
                4005,
                17,
                3_681,
                [9u8; 32],
            ),
        ];
        let msg = raw_message(&payer, &ixs);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);

        assert_eq!(decoded.ix_names, vec!["Open", "Seal"]);
        assert_eq!(decoded.chain_id, Some(200_101));
        assert_eq!(decoded.batch_id, Some(4005));
        assert_eq!(decoded.chunk_events.len(), 2, "{:?}", decoded.chunk_events);
        assert_eq!(decoded.chunk_events[0].chunk_pda, chunk.to_string());
        assert_eq!(decoded.chunk_events[1].chunk_pda, chunk.to_string());
        assert_eq!(
            decoded.chunk_events[0].kind,
            ChunkEventKind::Opened {
                chain_id: 200_101,
                batch: 4005,
                idx: 17,
                byte_len: 3_681,
            }
        );
        assert_eq!(
            decoded.chunk_events[1].kind,
            ChunkEventKind::Sealed {
                byte_len: 3_681,
                body_hash: [9u8; 32],
            }
        );
    }

    /// Two `Open`s in one transaction (two different chunks) must both be recorded -- the old
    /// `get_or_insert` silently dropped the second.
    #[test]
    fn decode_inbox_tx_records_both_opens_when_a_tx_carries_two() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let ixs = [
            open_chunk_ix(
                &program_id,
                &payer,
                &SETTLEMENT_PROGRAM,
                200_101,
                4005,
                1,
                100,
            ),
            open_chunk_ix(
                &program_id,
                &payer,
                &SETTLEMENT_PROGRAM,
                200_101,
                4005,
                2,
                200,
            ),
        ];
        let msg = raw_message(&payer, &ixs);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(decoded.chunk_events.len(), 2);
        let chunk1 = zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4005, 1)
            .0
            .to_string();
        let chunk2 = zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4005, 2)
            .0
            .to_string();
        assert_ne!(chunk1, chunk2);
        let pdas: Vec<&str> = decoded
            .chunk_events
            .iter()
            .map(|e| e.chunk_pda.as_str())
            .collect();
        assert!(pdas.contains(&chunk1.as_str()));
        assert!(pdas.contains(&chunk2.as_str()));
    }

    /// A `Seal`-only transaction (no `Open` in it -- the batcher's own re-seal/resubmit path) still
    /// produces a `Sealed` chunk event attributed by the chunk PDA account alone; `ingest.rs` is what
    /// refuses to fabricate a row from it (this module just reports what the instruction said).
    #[test]
    fn decode_inbox_tx_seal_only_still_carries_the_chunk_pda() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let ixs = [seal_chunk_ix(
            &program_id,
            &payer,
            &SETTLEMENT_PROGRAM,
            200_101,
            4005,
            17,
            3_681,
            [7u8; 32],
        )];
        let msg = raw_message(&payer, &ixs);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(decoded.chunk_events.len(), 1);
        assert_eq!(
            decoded.chunk_events[0].chunk_pda,
            zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4005, 17)
                .0
                .to_string()
        );
        // No Open in this tx -- decode.rs cannot and must not know chain_id/batch from a bare Seal.
        assert_eq!(decoded.chain_id, None);
        assert_eq!(decoded.batch_id, None);
    }

    /// `Close` (rent reclaimed) must carry a `ChunkEventKind::Closed` event keyed by the chunk PDA
    /// -- before this fix, `Close` only pushed an instruction name and derived
    /// nothing, leaving a reclaimed chunk looking permanently live.
    #[test]
    fn decode_inbox_tx_close_carries_the_chunk_pda() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let settlement_program = SETTLEMENT_PROGRAM;
        let chunk =
            zk_inbox_client::chunk_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4004, 3).0;
        let ix = zk_inbox_client::close_chunk_ix(
            &program_id,
            &payer,
            &settlement_program,
            200_101,
            4004,
            3,
        );
        let msg = raw_message(&payer, &[ix]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(decoded.ix_names, vec!["Close"]);
        assert_eq!(
            decoded.chunk_events,
            vec![ChunkEvent {
                chunk_pda: chunk.to_string(),
                kind: ChunkEventKind::Closed,
            }]
        );
    }

    #[test]
    fn decode_inbox_tx_open_batch_carries_the_batch_pda_account() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let settlement_program = SETTLEMENT_PROGRAM;
        let ix = open_batch_ix(&program_id, &payer, 200_101, 4003, 290, &settlement_program);
        let batch_pda = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4003)
            .0
            .to_string();
        let msg = raw_message(&payer, &[ix]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(decoded.ix_names, vec!["OpenBatch"]);
        assert_eq!(
            decoded.batch_events,
            vec![BatchEvent::Opened {
                chain_id: 200_101,
                batch: 4003,
                expected_count: 290,
                batch_pda,
            }]
        );
    }

    /// The inbox program is shared by every settlement program, so anyone can open a batch for the same
    /// `(chain_id, batch)` under their own. An `OpenBatch` naming another settlement program is listed by name but
    /// yields no event and attributes no chain or batch id.
    #[test]
    fn decode_inbox_tx_ignores_an_open_batch_under_a_foreign_settlement_program() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let foreign = Pubkey::new_unique();
        let ix = open_batch_ix(&program_id, &payer, 200_101, 4003, 290, &foreign);
        let msg = raw_message(&payer, &[ix]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(decoded.ix_names, vec!["OpenBatch"]);
        assert!(
            decoded.batch_events.is_empty(),
            "{:?}",
            decoded.batch_events
        );
        assert_eq!((decoded.chain_id, decoded.batch_id), (None, None));
    }

    /// A chunk `Open` creates its account at the settlement-keyed address; one created at a foreign program's
    /// address (same chain, batch and idx) is not attributed to this chain.
    #[test]
    fn decode_inbox_tx_ignores_a_chunk_open_at_a_foreign_settlement_address() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let foreign = Pubkey::new_unique();
        let ix = open_chunk_ix(&program_id, &payer, &foreign, 200_101, 4005, 17, 3_681);
        let msg = raw_message(&payer, &[ix]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(decoded.ix_names, vec!["Open"]);
        assert!(
            decoded.chunk_events.is_empty(),
            "{:?}",
            decoded.chunk_events
        );
        assert_eq!((decoded.chain_id, decoded.batch_id), (None, None));
    }

    /// `FinalizeBatch { step }` must carry `step` through -- a partial call (`step` nonzero, or `step ==
    /// 0` on a batch that is not the caller's real intent to finish) is distinguishable downstream from a
    /// completing one.
    #[test]
    fn decode_inbox_tx_finalize_batch_carries_step() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let batch_pda = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4003)
            .0
            .to_string();
        let ix = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 4003, 1);
        let msg = raw_message(&payer, &[ix]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(
            decoded.batch_events,
            vec![BatchEvent::Finalized {
                batch_pda: batch_pda.clone(),
                step: 1,
            }]
        );

        let ix0 = finalize_batch_ix(&program_id, &payer, &SETTLEMENT_PROGRAM, 200_101, 4003, 0);
        let msg0 = raw_message(&payer, &[ix0]);
        let decoded0 = decode_inbox_tx(&msg0, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert_eq!(
            decoded0.batch_events,
            vec![BatchEvent::Finalized { batch_pda, step: 0 }]
        );
    }

    /// `FinalizeBatchV2 { step, deposit_to }` is recorded as a finalize step with its `step`, exactly as
    /// `FinalizeBatch { step }` is; the batch account is still account 0.
    #[test]
    fn decode_inbox_tx_finalize_batch_v2_is_recorded_like_finalize_batch() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let bridge = Pubkey::new_unique();
        let batch_pda = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4003)
            .0
            .to_string();
        for (step, deposit_to, bridge_program) in
            [(1, 0, None), (0, 3, Some(&bridge)), (0, 0, Some(&bridge))]
        {
            let ix = zk_inbox_client::finalize_batch_v2_ix(
                &program_id,
                &payer,
                &SETTLEMENT_PROGRAM,
                200_101,
                4003,
                step,
                deposit_to,
                bridge_program,
            );
            let msg = raw_message(&payer, &[ix]);
            let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
            assert_eq!(decoded.ix_names, vec!["FinalizeBatchV2"]);
            assert_eq!(
                decoded.batch_events,
                vec![BatchEvent::Finalized {
                    batch_pda: batch_pda.clone(),
                    step,
                }]
            );
        }
    }

    #[test]
    fn decode_inbox_tx_ignores_instructions_for_a_different_program() {
        let program_id = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let ix = write_chunk_ix(&other, &payer, &SETTLEMENT_PROGRAM, 1, 1, 1, 0, vec![1]);
        let msg = raw_message(&payer, &[ix]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert!(decoded.ix_names.is_empty());
        assert!(decoded.chunk_events.is_empty());
    }

    #[test]
    fn decode_inbox_tx_skips_undecodable_instruction_data_without_panicking() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        // discriminant 0 (Open) but far too short to hold its fields -- borsh must reject it.
        let bogus = solana_program::instruction::Instruction {
            program_id,
            accounts: vec![],
            data: vec![0, 1, 2],
        };
        let msg = raw_message(&payer, &[bogus]);
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);
        assert!(decoded.ix_names.is_empty());
    }

    /// RED before the fix: an unresolvable account index (out of range against this transaction's own static
    /// `account_keys` -- an ALT-loaded key this crate does not decode, or a malformed message) must never silently
    /// shift every later account left. The unfixed `filter_map` compaction turned `Open`'s unresolvable chunk-account
    /// slot into the BATCH account occupying the next position -- attributing an `Opened` chunk event to the batch's
    /// own PDA.
    #[test]
    fn decode_inbox_tx_unresolvable_account_index_does_not_shift_later_accounts() {
        let program_id = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let ix = open_chunk_ix(
            &program_id,
            &payer,
            &SETTLEMENT_PROGRAM,
            200_101,
            4005,
            17,
            3_681,
        );
        let mut msg = raw_message(&payer, &[ix]);
        // `open_chunk_ix`'s account index 1 (`chunk_account_index`) -- corrupt it to an index this
        // message's own (short) `account_keys` cannot resolve.
        msg.instructions[0].accounts[1] = 200;
        let decoded = decode_inbox_tx(&msg, &program_id.to_string(), &SETTLEMENT_PROGRAM);

        let batch_pda = zk_inbox_client::batch_pda(&program_id, &SETTLEMENT_PROGRAM, 200_101, 4005)
            .0
            .to_string();
        for event in &decoded.chunk_events {
            assert_ne!(
                event.chunk_pda, batch_pda,
                "an unresolvable account index must never be silently mis-attributed to a later, real \
                 account (here, the batch PDA one position over)"
            );
        }
        // account_at returns None for the unresolvable slot -- ingest.rs already refuses to fabricate a
        // row without a chunk_pda, so this Open derives no chunk event at
        // all rather than a mis-attributed one.
        assert!(
            decoded.chunk_events.is_empty(),
            "an Open whose chunk account does not resolve must derive no chunk event, not a \
             mis-attributed one: {:?}",
            decoded.chunk_events
        );
    }
}
