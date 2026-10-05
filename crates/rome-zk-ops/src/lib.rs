//! `rome-zk-ops`: the operator CLI. Settlement commands: `chain-id`, `register`, `refund-deposit`,
//! `exit-config propose|activate|show`, `migrate`, `init-cursor` and `vkey register|show`.
//!
//! Three rules hold for every command:
//!
//! - Every transaction goes out as a V1 transaction through `rome-zk-solana-sender`. There is no other send path
//!   in this crate, and no legacy transaction constructor.
//! - Keys come from file paths only, and are never printed.
//! - A command is a dry run unless `--confirm` is given. A dry run builds and signs the transaction against an
//!   all-zero blockhash and prints it; it never sends.
//!
//! The commands are generic over [`chain::Chain`], so their tests run over a fake that records what it was asked
//! to send. The `register_chain`, `governance`, `migrate_chain` and `init_cursor` examples are thin wrappers over
//! [`cli::run`].

#![forbid(unsafe_code)]

pub mod chain;
pub mod cli;
pub mod commands;
pub mod error;
pub mod genesis;
pub mod keys;
pub mod rebuild;
pub mod render;

pub use error::{Mode, OpsError, Report};
