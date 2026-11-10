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
//! - **v4**: an optional list of armed exit triggers (decision 0029). The version moved so that
//!   an older build refuses the file instead of reading it and silently dropping a position's
//!   Stop.
//! - **v5**: no saved limit on an option quote's age, which is now one function by expiry
//!   (`risk::option_quote_stale_ms`). The version moved so that an older build refuses the file
//!   instead of reading it and going back to its 15 minutes.
//! - **v6**: the current format: account groups are a list of their own (`groups`: name and
//!   members, `crate::group`), no longer an optional `group` name in each account's config. The
//!   migration makes one group per name, members in account order. The version moved so that an
//!   older build refuses the file instead of reading it and silently dropping the groups.
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

pub const VERSION: u64 = 6;

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
    /// Account groups, in the order they were created (from v6). Optional: none is empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<crate::group::AccountGroup>,
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
            4 => v4_to_v5(v)?,
            5 => v5_to_v6(v)?,
            n => bail!("no migration from version {n}"),
        };
        version += 1;
    }
    Ok(v)
}

/// v5 → v6: each account's optional `config.group` becomes a member of a group of that name in
/// the new `groups` list (one per name, first seen first, members in account order). Names that
/// v6 counts as one (`group::same_name`: trimmed, ignoring case) are merged under the first
/// one's spelling, and a name v6 can't keep (`group::clean_name`) is cut down to one it can, so
/// no membership is dropped on load.
fn v5_to_v6(mut v: Value) -> Result<Value> {
    let o = v.as_object_mut().context("v5 file is not an object")?;
    let Some(accounts) = o.get_mut("accounts").and_then(Value::as_array_mut) else {
        bail!("v5 file has no accounts");
    };
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for a in accounts {
        let Some(config) = a.get_mut("config").and_then(Value::as_object_mut) else {
            continue;
        };
        let group = config.remove("group");
        let (Some(Value::String(name)), Some(id)) = (group, config.get("id").cloned()) else {
            continue;
        };
        let name = keepable_group_name(&name);
        match groups
            .iter_mut()
            .find(|(n, _)| crate::group::same_name(n, &name))
        {
            Some((_, members)) => members.push(id),
            None => groups.push((name, vec![id])),
        }
    }
    if !groups.is_empty() {
        let list: Vec<Value> = groups
            .into_iter()
            .map(|(name, members)| json!({ "name": name, "members": members }))
            .collect();
        o.insert("groups".into(), Value::Array(list));
    }
    o.insert("version".into(), json!(6));
    Ok(v)
}

/// A v5 group name as v6 can keep it: control characters become spaces, then trimmed and cut
/// to `group::MAX_NAME_CHARS`; an empty one is "Group".
fn keepable_group_name(name: &str) -> String {
    let spaced: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cut: String = spaced
        .trim()
        .chars()
        .take(crate::group::MAX_NAME_CHARS)
        .collect();
    match cut.trim() {
        "" => "Group".to_owned(),
        n => n.to_owned(),
    }
}

/// v4 → v5: the 15 minutes every file holds (and any hand-set age) go, for the function by expiry.
fn v4_to_v5(mut v: Value) -> Result<Value> {
    let o = v.as_object_mut().context("v4 file is not an object")?;
    if let Some(paper) = o.get_mut("paper").and_then(Value::as_object_mut) {
        paper.remove("option_quote_stale_ms");
    }
    let Some(accounts) = o.get_mut("accounts").and_then(Value::as_array_mut) else {
        bail!("v4 file has no accounts");
    };
    for a in accounts {
        if let Some(guard) = a
            .pointer_mut("/config/limits/option_guard")
            .and_then(Value::as_object_mut)
        {
            guard.remove("stale_after_ms");
        }
    }
    o.insert("version".into(), json!(5));
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

/// Keep `path` as it is now beside it, dated in UTC: `paper/mock.json` becomes
/// `paper/mock-20261109-221500.json.bak` (a second one that second gets `-2`, and so on). Before
/// a paper account's reset (`Command::ResetPaper`) is saved over it. `None`: no file yet.
pub fn backup(path: &Path, now: lines_core::UtcMillis) -> Result<Option<std::path::PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stem = path
        .file_stem()
        .map_or_else(|| "trading".into(), |s| s.to_string_lossy().into_owned());
    let (y, mo, d, h, mi, s) = now.to_civil();
    let base = format!("{stem}-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut n = 1;
    let to = loop {
        let name = if n == 1 {
            format!("{base}.json.bak")
        } else {
            format!("{base}-{n}.json.bak")
        };
        let to = dir.join(name);
        if !to.exists() {
            break to;
        }
        n += 1;
    };
    std::fs::copy(path, &to)?;
    Ok(Some(to))
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

    #[test]
    fn a_backup_keeps_the_file_dated_and_never_overwrites_one() {
        let dir = std::env::temp_dir().join(format!("lines-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mock.json");
        let now = lines_core::UtcMillis::parse_iso("2026-11-09T22:15:00Z").unwrap();
        assert_eq!(backup(&path, now).unwrap(), None, "no file yet");
        std::fs::write(&path, b"before").unwrap();
        let a = backup(&path, now).unwrap().unwrap();
        assert_eq!(a, dir.join("mock-20261109-221500.json.bak"));
        assert_eq!(std::fs::read(&a).unwrap(), b"before");
        let b = backup(&path, now).unwrap().unwrap();
        assert_eq!(b, dir.join("mock-20261109-221500-2.json.bak"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"before",
            "the file itself is left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
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
        // Written back: the current version (decision 0028: contracts may be options), the same content.
        let again = serde_json::to_value(&f).unwrap();
        assert_eq!(again["version"], json!(VERSION));
        assert_eq!(read(&again.to_string()), f);
    }

    /// A v5 file's per-account group names become the v6 `groups` list: one group per name,
    /// first seen first, members in account order; the configs lose the field, and the file
    /// written back reads the same.
    #[test]
    fn a_v5_files_group_names_become_groups() {
        let mut v: Value = serde_json::from_str(V2).unwrap();
        v["version"] = json!(5);
        let account = v["accounts"][0].clone();
        let long = format!("{} tail", "x".repeat(crate::group::MAX_NAME_CHARS - 1));
        let mut accounts = Vec::new();
        for (id, group) in [
            ("a", Some("Evals")),
            ("b", None),
            ("c", Some("Funded")),
            // v6 counts these as "Evals" (case, spaces): merged, never dropped.
            ("d", Some("evals ")),
            ("e", Some("  ")),
            // Kept, not dropped: a control character becomes a space, a long name is cut.
            (
                "f",
                Some(
                    "a
b",
                ),
            ),
            ("g", Some(&long)),
        ] {
            let mut a = account.clone();
            a["config"]["id"] = json!(id);
            if let Some(g) = group {
                a["config"]["group"] = json!(g);
            }
            accounts.push(a);
        }
        v["accounts"] = json!(accounts);
        let f = read(&v.to_string());
        assert_eq!(f.version, VERSION);
        let g = |n: &str, m: &[&str]| crate::group::AccountGroup {
            name: n.into(),
            members: m.iter().map(|s| crate::types::AccountId::new(s)).collect(),
        };
        assert_eq!(
            f.groups,
            vec![
                g("Evals", &["a", "d"]),
                g("Funded", &["c"]),
                g("Group", &["e"]),
                g("a b", &["f"]),
                g(&"x".repeat(crate::group::MAX_NAME_CHARS - 1), &["g"]),
            ]
        );
        // What loads is what the core keeps: nothing for `restore_groups` to drop.
        assert_eq!(crate::group::restored(f.groups.clone()), f.groups);
        let again = serde_json::to_value(&f).unwrap();
        assert!(
            again["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["config"].get("group").is_none()),
            "the configs lose the field"
        );
        assert_eq!(read(&again.to_string()), f);
        // A v5 file without groups gets none, and writes none.
        let mut plain: Value = serde_json::from_str(V2).unwrap();
        plain["version"] = json!(5);
        let f = read(&plain.to_string());
        assert!(f.groups.is_empty());
        assert!(serde_json::to_value(&f).unwrap().get("groups").is_none());
    }

    /// A v4 file holds 900000 twice. Both go, the guard's other settings stay, and a file written
    /// back has neither.
    #[test]
    fn a_v4_file_loses_its_saved_quote_age() {
        let mut v: Value = serde_json::from_str(V2).unwrap();
        v["version"] = json!(4);
        v["paper"]["option_quote_stale_ms"] = json!(900_000);
        v["accounts"][0]["config"]["limits"]["option_guard"] = json!({
            "max_from_mid_pct": 5,
            "min_band_ticks": 1,
            "allow_market": true,
            "stale_after_ms": 900_000,
            "trigger_stale_ms": 20_000,
            "target_chase_ms": 4_000
        });
        let migrated = migrate(v).unwrap();
        assert_eq!(migrated["version"], json!(VERSION));
        // On the migrated value: reading the file would drop unknown keys anyway.
        assert!(migrated["paper"].get("option_quote_stale_ms").is_none());
        let saved = &migrated["accounts"][0]["config"]["limits"]["option_guard"];
        assert!(saved.get("stale_after_ms").is_none(), "{saved}");
        assert_eq!(saved["max_from_mid_pct"], json!(5));
        let f: TradingFile = serde_json::from_value(migrated).unwrap();
        let guard = f.accounts[0].config.limits.option_guard.clone().unwrap();
        assert_eq!(guard.max_from_mid_pct, 5);
        assert!(guard.allow_market);
        assert_eq!(guard.trigger_stale_ms, 20_000);
        assert_eq!(guard.target_chase_ms, 4_000);
        let written = serde_json::to_string(&f).unwrap();
        assert!(!written.contains("option_quote_stale_ms"), "{written}");
        assert!(!written.contains("stale_after_ms"), "{written}");
        // No accounts is no file, as in every migration.
        assert!(migrate(json!({"version": 4})).is_err());
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
