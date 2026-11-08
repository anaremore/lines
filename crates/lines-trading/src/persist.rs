//! The paper trading file: account configs and rule progress, the paper broker's accounts
//! (balances, positions) and open orders (brackets included, so they survive restarts), and the
//! next order id. Versioned JSON, migrated forward on load, written atomically.
//!
//! - **v0**: a file without a `version` (hand-written or from the first prototype): just
//!   `accounts: [{id, starting_balance}]`. Migrated by filling in defaults.
//! - **v1**: accounts, rule trackers, the paper broker, in-flight orders (roadmap 6/7). Builds
//!   from roadmap 8b wrote an optional `guardrails` per account into v1 files too.
//! - **v2**: the same layout, with each account's guardrails (roadmap 8b:
//!   rules in force, pending looser ones, the day's counts, a lockout). The version moved so
//!   that a build from before guardrails **refuses** the file instead of reading it and
//!   silently dropping a lockout.
//! - **v3**: the same layout, and contracts may be equities or options (decision 0028,
//!   `types::ContractDoc`). The version moved so that an older build refuses the file instead
//!   of reading an option position as spot.
//! - **v4**: the current format: an optional list of armed exit triggers (decision 0029). The
//!   version moved so that an older build refuses the file instead of reading it and silently
//!   dropping a position's Stop.
//!
//! A file from a newer Lines is refused (never half-read), and a damaged file is reported,
//! never silently replaced.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::AccountConfig;
use crate::guardrails::GuardrailTracker;
use crate::paper::{PaperConfig, PaperState};
use crate::rules::RuleTracker;

pub const VERSION: u64 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAccount {
    pub config: AccountConfig,
    /// Rule progress (high-water mark, day, breaches).
    pub tracker: RuleTracker,
    /// Guardrails in force, pending (looser) ones, the day's counts and any lockout
    /// (roadmap 8b). Optional: older files have none.
    #[serde(default, skip_serializing_if = "GuardrailTracker::is_default")]
    pub guardrails: GuardrailTracker,
    /// Prop rules asked for that wait for the next trade date (looser than those in force,
    /// which are in `config.rules`). Optional: older files have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_rules: Option<crate::rules::PendingRules>,
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
    /// Stops and Targets on open option positions (decision 0029; entries never come back).
    /// Optional: older files have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::trigger::SavedTrigger>,
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
            1 => v1_to_v2(v)?,
            2 => v2_to_v3(v)?,
            3 => v3_to_v4(v)?,
            n => bail!("no migration from version {n}"),
        };
        version += 1;
    }
    Ok(v)
}

/// v3 → v4: the layout is unchanged (triggers are optional, absent = none); only the version
/// moves, so older builds refuse what this one writes.
fn v3_to_v4(mut v: Value) -> Result<Value> {
    let o = v.as_object_mut().context("v3 file is not an object")?;
    if !o.get("accounts").is_some_and(Value::is_array) {
        bail!("v3 file has no accounts");
    }
    o.insert("version".into(), json!(4));
    Ok(v)
}

/// v2 → v3: the layout is unchanged (every v2 contract is a valid v3 contract, read back as
/// the same key); only the version moves, so older builds refuse what this one writes.
fn v2_to_v3(mut v: Value) -> Result<Value> {
    let o = v.as_object_mut().context("v2 file is not an object")?;
    if !o.get("accounts").is_some_and(Value::is_array) {
        bail!("v2 file has no accounts");
    }
    o.insert("version".into(), json!(3));
    Ok(v)
}

/// v1 → v2: the layout is unchanged (guardrails are optional per account, absent = none); only
/// the version moves, so older builds refuse what this one writes.
fn v1_to_v2(mut v: Value) -> Result<Value> {
    let o = v.as_object_mut().context("v1 file is not an object")?;
    if !o.get("accounts").is_some_and(Value::is_array) {
        bail!("v1 file has no accounts");
    }
    o.insert("version".into(), json!(2));
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

/// The guardrails file's version (its own, apart from the trading file's).
pub const GUARDS_VERSION: u64 = 1;

/// Guardrail progress of accounts the trading file doesn't hold, by account id: a broker
/// venue's (its accounts, positions and balances live at the broker). A lockout written here
/// survives a restart; the venue's core restores it when the account joins.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardFile {
    pub version: u64,
    pub accounts: std::collections::BTreeMap<String, VenueGuard>,
}

/// One venue account's saved guardrail progress.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueGuard {
    /// Guardrails in force, pending looser ones, the day's counts and any lockout.
    pub guardrails: GuardrailTracker,
    /// The trade date and the balance it opened with: the daily loss counts from it, so a
    /// restart the same day keeps today's loss.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::rules::opt_date"
    )]
    pub day: Option<lines_core::CivilDate>,
    #[serde(default)]
    pub day_open_balance: crate::types::Money,
}

/// A guardrails file; `None` if there is none. A newer version is refused (never half-read);
/// the app sets a refused file aside and reports it.
pub fn load_guards(path: &Path) -> Result<Option<GuardFile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is damaged", path.display()))?;
    let version = v
        .get("version")
        .and_then(Value::as_u64)
        .with_context(|| format!("{} has no version", path.display()))?;
    if version > GUARDS_VERSION {
        bail!(
            "guardrails file is version {version}, newer than this build reads ({GUARDS_VERSION})"
        );
    }
    serde_json::from_value(v)
        .map(Some)
        .with_context(|| format!("{} doesn't match the guardrails format", path.display()))
}

/// Write atomically (temporary file, then rename over the old one).
pub fn save_guards(path: &Path, file: &GuardFile) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(file)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrails::GuardState;

    /// Pinned: exactly what a v1 build with guardrails (roadmap 8b, before the bump) wrote, a
    /// lockout included. It must keep migrating to the current format with the lockout intact.
    const V1: &str = r#"
        {
          "version": 1,
          "paper": {
            "latency_ms": 0,
            "fill_model": "trade_through",
            "fees": {
              "1": 20000000000
            },
            "default_fee": 0,
            "check_sessions": true
          },
          "broker": {
            "accounts": {
              "paper-1": {
                "balance": 499710000000000,
                "positions": []
              }
            },
            "orders": []
          },
          "accounts": [
            {
              "config": {
                "id": "paper-1",
                "name": "paper-1",
                "starting_balance": 50000,
                "limits": {},
                "day_product": 1
              },
              "tracker": {
                "start_balance": 500000000000000,
                "high_water": 500000000000000,
                "locked": false,
                "day": "2026-11-10",
                "day_open_balance": 500000000000000,
                "day_profits": [],
                "drawdown_breached": false,
                "daily_breached": false
              },
              "guardrails": {
                "active": {
                  "max_trades_per_day": 1,
                  "max_daily_loss": 500
                },
                "pending": {},
                "day": "2026-11-10",
                "trades": 1,
                "streak": 1,
                "lock": {
                  "state": "locked",
                  "reason": "max 1 trade reached",
                  "until": 1794348000000,
                  "date": "2026-11-10"
                }
              }
            }
          ],
          "next_order_id": 3
        }
    "#;

    /// Pinned: a v2 file as written today. If this stops reading, the format changed and needs
    /// a version bump plus a migration, not an edit here.
    const V2: &str = r#"
        {
          "version": 2,
          "paper": {
            "latency_ms": 0,
            "fill_model": "trade_through",
            "fees": {
              "1": 20000000000
            },
            "default_fee": 0,
            "check_sessions": true
          },
          "broker": {
            "accounts": {
              "paper-1": {
                "balance": 499710000000000,
                "positions": []
              }
            },
            "orders": []
          },
          "accounts": [
            {
              "config": {
                "id": "paper-1",
                "name": "paper-1",
                "starting_balance": 50000,
                "limits": {},
                "day_product": 1
              },
              "tracker": {
                "start_balance": 500000000000000,
                "high_water": 500000000000000,
                "locked": false,
                "day": "2026-11-10",
                "day_open_balance": 500000000000000,
                "day_profits": [],
                "drawdown_breached": false,
                "daily_breached": false
              },
              "guardrails": {
                "active": {
                  "max_trades_per_day": 1,
                  "max_daily_loss": 500
                },
                "pending": {},
                "day": "2026-11-10",
                "trades": 1,
                "streak": 1,
                "lock": {
                  "state": "locked",
                  "reason": "max 1 trade reached",
                  "until": 1794348000000,
                  "date": "2026-11-10"
                }
              }
            }
          ],
          "next_order_id": 3
        }
    "#;

    fn read(text: &str) -> TradingFile {
        serde_json::from_value(migrate(serde_json::from_str(text).unwrap()).unwrap()).unwrap()
    }

    #[test]
    fn a_v1_file_migrates_to_v2_and_keeps_its_lockout() {
        let f = read(V1);
        assert_eq!(f.version, VERSION);
        let g = &f.accounts[0].guardrails;
        assert_eq!(g.state(), GuardState::Locked);
        assert_eq!(g.lock.as_ref().unwrap().reason, "max 1 trade reached");
        assert_eq!(g.active.max_trades_per_day, Some(1));
        assert_eq!(g.trades, 1);
        assert!(
            g.pending.as_ref().unwrap().is_default(),
            "loosened: from tomorrow"
        );
        assert_eq!(f, read(V2), "v1 migrated = the same file written as v2");
    }

    #[test]
    fn a_v2_file_as_written_today_still_reads() {
        let f = read(V2);
        assert_eq!(f.version, VERSION);
        assert_eq!(f.accounts[0].guardrails.state(), GuardState::Locked);
        // Written back: v3 (decision 0028: contracts may be options), the same content.
        let again = serde_json::to_value(&f).unwrap();
        assert_eq!(again["version"], json!(3));
        assert_eq!(read(&again.to_string()), f);
    }

    #[test]
    fn older_shapes_and_newer_versions() {
        // v1 without guardrails (before roadmap 8b): fine, none set.
        let mut plain: Value = serde_json::from_str(V1).unwrap();
        plain["accounts"][0]
            .as_object_mut()
            .unwrap()
            .remove("guardrails");
        let f = read(&plain.to_string());
        assert!(f.accounts[0].guardrails.is_default());
        // A v1 shape the migration can't follow is an error, not a guess.
        assert!(migrate(json!({"version": 1})).is_err());
        // A newer file is refused (this is how a v1 build refuses a v2 file).
        let e = migrate(json!({"version": VERSION + 1}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("newer"), "{e}");
    }
}
