//! Every way a command stops without sending. A refusal carries a name, so an operator, a script and a test can
//! all tell the cases apart, and an exit code: 2 when the request itself is wrong (flags, keys, a rule the
//! command checks before it looks at the chain), 1 when the chain or the network said no.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsError {
    pub name: &'static str,
    pub detail: String,
    pub exit_code: i32,
}

impl OpsError {
    /// The request is wrong; nothing was read from the chain for it. Exit 2.
    pub fn usage(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            detail: detail.into(),
            exit_code: 2,
        }
    }

    /// The chain or the network said no (a lookup failed, an account is missing, a send failed). Exit 1.
    pub fn chain(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            detail: detail.into(),
            exit_code: 1,
        }
    }
}

impl fmt::Display for OpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.detail)
    }
}

impl std::error::Error for OpsError {}

/// What a command did. `lines` is everything to print; `signature` is set only when a transaction was sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<String>,
    pub signature: Option<String>,
}

impl Report {
    pub fn line(&mut self, s: impl Into<String>) {
        self.lines.push(s.into());
    }

    pub fn sent(&self) -> bool {
        self.signature.is_some()
    }
}

/// `--dry-run` (the default) or `--confirm`. A dry run builds and signs the transaction and prints it; it never
/// calls `send`. A confirmed run sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dry,
    Confirm,
}

impl Mode {
    pub fn is_confirm(self) -> bool {
        self == Mode::Confirm
    }
}
