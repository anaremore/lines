//! Broker extension point (decisions 0004, 0018, 0020).
//!
//! Brokers that are not built in (live connectors kept outside this repository) register a
//! [`BrokerExtension`]: a name and label for the Connections screen, the capabilities the UI
//! enables controls from, an [`AccountSource`] listing the accounts it offers, and a factory
//! for the [`Broker`] the trading core drives. The core can't tell an extension's broker from
//! the paper broker (0004).
//!
//! Nothing here names a vendor; the public `lines` binary registers none.

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::broker::{Broker, BrokerCaps};
use crate::types::AccountId;

/// Names the built-in brokers use; an extension can't take them.
pub const RESERVED_BROKER_NAMES: &[&str] = &["paper", "replay"];

/// Whether an account trades real money (decision 0006: paper vs live is unmistakable).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AccountKind {
    /// A simulation account at the venue (no real money).
    Simulated,
    /// Real money.
    Live,
    /// The venue doesn't say: treat as live, never as simulated.
    Unknown,
}

/// One account a broker extension offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionAccount {
    pub id: AccountId,
    /// What the UI shows (the extension decides how much of the id to reveal).
    pub label: String,
    pub kind: AccountKind,
    /// `Err(reason)` while the account can't be traded through Lines (shown as a disabled
    /// control with the reason, decision 0018).
    pub tradable: Result<(), String>,
}

/// The accounts an extension offers right now (they can change while connected).
pub trait AccountSource: Send + Sync {
    fn accounts(&self) -> Vec<ExtensionAccount>;
    /// Bumped whenever [`AccountSource::accounts`] may have changed, so callers can poll cheaply.
    fn revision(&self) -> u64;
}

/// A broker registered from outside the public crates.
pub trait BrokerExtension: Send + Sync {
    /// Stable name (lowercase letters, digits, `-`), used in saved account choices.
    fn name(&self) -> &str;
    /// Shown on the Connections screen.
    fn label(&self) -> &str;
    fn capabilities(&self) -> BrokerCaps;
    fn accounts(&self) -> Arc<dyn AccountSource>;
    /// A broker for the trading core. Called once per trading session.
    fn open(&self) -> Result<Box<dyn Broker>>;
}

/// Check a set of broker extensions: well-formed, unique names, none reserved.
pub fn validate(brokers: &[Arc<dyn BrokerExtension>]) -> Result<()> {
    for (i, b) in brokers.iter().enumerate() {
        let name = b.name();
        let well_formed = !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if !well_formed {
            bail!("broker extension name {name:?}: use lowercase letters, digits and '-'");
        }
        if RESERVED_BROKER_NAMES.contains(&name) {
            bail!("broker extension name {name:?} is taken by a built-in broker");
        }
        if brokers[..i].iter().any(|o| o.name() == name) {
            bail!("two broker extensions are named {name:?}");
        }
    }
    Ok(())
}

/// Account counts by kind, for logs and status text (never account ids).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountSummary {
    pub simulated: usize,
    pub live: usize,
    pub unknown: usize,
    pub tradable: usize,
}

pub fn summarize(accounts: &[ExtensionAccount]) -> AccountSummary {
    let mut s = AccountSummary::default();
    for a in accounts {
        match a.kind {
            AccountKind::Simulated => s.simulated += 1,
            AccountKind::Live => s.live += 1,
            AccountKind::Unknown => s.unknown += 1,
        }
        if a.tradable.is_ok() {
            s.tradable += 1;
        }
    }
    s
}
