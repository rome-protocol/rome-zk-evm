//! Keys come from file paths and nowhere else. Nothing here prints a key; a failure names the path and the
//! role of the file, never its contents. Only public keys are ever shown.

use crate::error::OpsError;
use solana_keypair::Keypair;
use solana_signer::Signer;
use std::path::Path;

/// Reads a keypair file. `role` is the flag name, so the message says which key was meant.
pub fn load(path: &Path, role: &str) -> Result<Keypair, OpsError> {
    solana_keypair::read_keypair_file(path).map_err(|_| {
        OpsError::usage(
            "KeypairUnreadable",
            format!(
                "{role}: {} is not a readable Solana keypair file (nothing about its contents is shown)",
                path.display()
            ),
        )
    })
}

/// A second handle on the same key, for a sender that takes its keys by value.
pub fn copy(k: &Keypair) -> Keypair {
    Keypair::try_from(&k.to_bytes()[..]).expect("a keypair's own bytes always read back")
}

/// The public key in the `solana_program` type the instruction builders take.
pub fn pubkey(k: &Keypair) -> solana_program::pubkey::Pubkey {
    rome_zk_solana_sender::compat::from_v1_pubkey(&k.pubkey())
}

/// The fee payer and any further required signers for one command.
pub struct Signers {
    pub payer: Keypair,
    pub cosigners: Vec<Keypair>,
}

impl Signers {
    pub fn new(payer: Keypair, cosigners: Vec<Keypair>) -> Self {
        Self { payer, cosigners }
    }

    pub fn payer_pubkey(&self) -> solana_program::pubkey::Pubkey {
        pubkey(&self.payer)
    }

    pub fn as_tx_signer(&self) -> rome_zk_solana_sender::PayerAndCosigners<'_> {
        rome_zk_solana_sender::PayerAndCosigners {
            payer: &self.payer,
            cosigners: &self.cosigners,
        }
    }
}
