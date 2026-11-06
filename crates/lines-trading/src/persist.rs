//! The paper trading file: account configs and rule progress, the paper broker's accounts
//! (balances, positions) and open orders (brackets included, so they survive restarts), and the
//! next order id. Versioned JSON, migrated forward on load, written atomically.
//!
//! - **v0**: a file without a `version` (hand-written or from the first prototype): just
//!   `accounts: [{id, starting_balance}]`. Migrated by filling in defaults.
//! - **v1**: the current format.
//!
//! A file from a newer Lines is refused (never half-read), and a damaged file is reported,
//! never silently replaced.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::AccountConfig;
use crate::paper::{PaperConfig, PaperState};
use crate::rules::RuleTracker;

pub const VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAccount {
    pub config: AccountConfig,
    /// Rule progress (high-water mark, day, breaches).
    pub tracker: RuleTracker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradingFile {
    pub version: u64,
    pub paper: PaperConfig,
    pub broker: PaperState,
    pub accounts: Vec<SavedAccount>,
    pub next_order_id: u64,
    /// Orders sent without a known outcome when the file was written (they come back
    /// unresolved, never assumed working). Optional: older files have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_flight: Vec<crate::order::Order>,
}

/// Bring any older layout up to the current one.
pub fn migrate(mut v: Value) -> Result<Value> {
    let mut version = v.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version > VERSION {
        bail!("trading file is version {version}, newer than this build reads ({VERSION})");
    }
    while version < VERSION {
        v = match version {
            0 => v0_to_v1(v)?,
            n => bail!("no migration from version {n}"),
        };
        version += 1;
    }
    Ok(v)
}

/// v0 → v1: accounts gain full configs, rule trackers and broker balances.
fn v0_to_v1(v: Value) -> Result<Value> {
    let accounts = v
        .get("accounts")
        .and_then(Value::as_array)
        .context("v0 file has no accounts")?;
    let mut out = Vec::new();
    let mut broker = serde_json::Map::new();
    for a in accounts {
        let id = a
            .get("id")
            .and_then(Value::as_str)
            .context("account without id")?;
        let start = a
            .get("starting_balance")
            .and_then(Value::as_i64)
            .context("account without starting_balance")?;
        let cfg = AccountConfig::paper(id, start);
        let tracker = RuleTracker::new(crate::types::Money::whole(start));
        out.push(json!({ "config": cfg, "tracker": tracker }));
        broker.insert(
            id.to_owned(),
            json!({ "balance": crate::types::Money::whole(start), "positions": [] }),
        );
    }
    Ok(json!({
        "version": 1,
        "paper": PaperConfig::default(),
        "broker": { "accounts": broker, "orders": [] },
        "accounts": out,
        "next_order_id": 1,
    }))
}

pub fn load(path: &Path) -> Result<Option<TradingFile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is damaged", path.display()))?;
    let v = migrate(v)?;
    serde_json::from_value(v)
        .map(Some)
        .with_context(|| format!("{} doesn't match the trading file format", path.display()))
}

/// Write atomically (temporary file, then rename over the old one).
pub fn save(path: &Path, file: &TradingFile) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(file)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
