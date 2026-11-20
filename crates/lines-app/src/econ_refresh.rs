//! The economic calendar's refresh (roadmap 8b): official release dates from FRED
//! (`lines_feed::econ::fred`), on start and every [`EVERY`], cached in the data folder
//! (`econ-calendar.txt`, the bundled format) so the next start has them offline.
//!
//! - Only the interactive app refreshes, and only with a FRED API key (Settings ›
//!   Connections › Economic calendar; kept in the OS keychain, or `LINES_FRED_API_KEY`).
//! - A failed refresh keeps the calendar in use and says so; it is tried again after
//!   [`RETRY`]. It never blocks trading: the news blackout reads whatever calendar is in use,
//!   and a stale one refuses entries under block or flatten as before (never taken as clear).
//! - Each refresh merges onto the build's own calendar, never onto an earlier refresh, and
//!   the cache records which build's calendar that was and under which FRED mapping
//!   (`# base <hash> mapping <n>`): a build whose calendar or mapping changed never uses an
//!   older build's cache (dates a wrong mapping added go with it).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use lines_core::econ::EconSchedule;
use lines_feed::econ::fred;

use crate::econ_ui;

/// Settings › Connections's name for it, and its keychain entry (`lines`, `fred.api-key`).
pub const NAME: &str = "fred";
pub const KEY: &str = "api-key";
/// Overrides the keychain (tests, headless setups).
pub const ENV: &str = "LINES_FRED_API_KEY";

/// Between refreshes that worked.
pub const EVERY: Duration = Duration::from_secs(12 * 3600);
/// After one that didn't.
pub const RETRY: Duration = Duration::from_secs(3600);

const FILE: &str = "econ-calendar.txt";

fn cache_path() -> Option<PathBuf> {
    crate::data_dir::root().map(|d| d.join(FILE))
}

/// The cache's first line: the build calendar it was merged onto.
fn base_line(hash: u64) -> String {
    format!("# base {hash:016x} mapping {}", fred::MAPPING_VERSION)
}

/// The cache, if it may stand in for `bundled`: built on this build's calendar (`hash`)
/// under this build's FRED mapping (`fred::MAPPING_VERSION`: a cache from a mapping since
/// found wrong is discarded with what it added), parses, and is at least as new.
pub fn usable_cache(text: &str, bundled: &EconSchedule, hash: u64) -> Option<EconSchedule> {
    if text.lines().next() != Some(base_line(hash).as_str()) {
        return None;
    }
    let s = EconSchedule::parse(text).ok()?;
    (s.version >= bundled.version).then_some(s)
}

/// At start, before any trading core: the cached calendar, if usable.
pub fn load_cache() {
    let Some(path) = cache_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    match usable_cache(
        &text,
        &lines_feed::econ::bundled(),
        lines_feed::econ::bundled_hash(),
    ) {
        Some(s) => {
            tracing::info!("economic calendar: using the cached one of {}", s.version);
            econ_ui::replace(Arc::new(s));
        }
        None => tracing::info!(
            "economic calendar: the cache is from another build or FRED mapping, older or unreadable; bundled"
        ),
    }
}

fn write_cache(path: &Path, s: &EconSchedule) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("txt.tmp");
    let text = format!(
        "{}\n{}",
        base_line(lines_feed::econ::bundled_hash()),
        s.to_text()
    );
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Wakes the refresher early (a key saved or removed, Refresh now).
fn bell() -> &'static (Mutex<bool>, Condvar) {
    static B: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
    B.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// Refresh now (Settings › Connections's Refresh now, a key saved or removed). Does nothing
/// in a run that doesn't refresh.
pub fn refresh_now() {
    let (m, c) = bell();
    *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
    c.notify_all();
}

fn sleep(d: Duration) {
    let (m, c) = bell();
    let rung = m.lock().unwrap_or_else(|e| e.into_inner());
    let (mut rung, _) = c
        .wait_timeout_while(rung, d, |r| !*r)
        .unwrap_or_else(|e| e.into_inner());
    *rung = false;
}

fn api_key() -> Result<Option<String>, String> {
    if let Ok(k) = std::env::var(ENV)
        && !k.trim().is_empty()
    {
        return Ok(Some(k.trim().to_owned()));
    }
    crate::connections::secret(NAME, KEY).map(|k| k.filter(|k| !k.trim().is_empty()))
}

/// FRED's answers as of `today` onto `base`, through the mapping and the cadence check
/// (`fred::validated`): the calendar, and what was left out (in words).
pub fn merge(
    base: &EconSchedule,
    answers: Vec<(String, Vec<lines_core::CivilDate>)>,
    today: lines_core::CivilDate,
) -> (EconSchedule, Option<String>) {
    let got = fred::validated(base, answers, today);
    let complete = got.complete(base, today);
    match base.refreshed(&got.dates, &fred::source_page, today, complete) {
        Ok(s) => (s, got.problem),
        Err(e) => (base.clone(), Some(e.to_string())),
    }
}

/// Screenshots (`--econ-fred-fixture`): FRED's answers from a file (a line per release: its
/// key, then its dates) go through [`merge`] onto the bundled calendar as of `now`, as a
/// refresh at `now` would; no network.
#[cfg(feature = "screenshot")]
pub fn apply_fixture(path: &Path, now: lines_core::UtcMillis) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let mut answers = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let mut w = line.split_whitespace();
        let key = w.next().unwrap_or_default().to_owned();
        let dates = w
            .map(|d| {
                let p: Vec<&str> = d.split('-').collect();
                let n = |i: usize| p.get(i).and_then(|x| x.parse::<u16>().ok());
                match (n(0), n(1), n(2)) {
                    (Some(y), Some(m), Some(dd)) => {
                        lines_core::CivilDate::new(y, m as u8, dd as u8)
                    }
                    _ => None,
                }
                .with_context(|| format!("{}: bad date {d:?}", path.display()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        answers.push((key, dates));
    }
    let base = lines_feed::econ::bundled();
    let (s, problem) = merge(&base, answers, base.local_date(now));
    econ_ui::replace(Arc::new(s));
    econ_ui::set_refresh(|r| {
        r.not_this_run = false;
        r.updated = Some(now);
        r.problem = problem.map(|p| format!("partly: {p}"));
    });
    Ok(())
}

/// One refresh of `base` as of `now`: the refreshed calendar and what didn't come in (some
/// releases may fail while others refresh), or the problem when nothing did.
pub fn refresh_once(
    base: &EconSchedule,
    key: &str,
    now: lines_core::UtcMillis,
) -> Result<(EconSchedule, Option<String>), String> {
    let today = base.local_date(now);
    let (answers, failed) = fred::fetch(base, key, today);
    if answers.is_empty() {
        return Err(failed.unwrap_or_else(|| "no dates".to_owned()));
    }
    // The one path FRED's dates take into the calendar (the mapping and the cadence check).
    let (s, refused) = merge(base, answers, today);
    let problem = refused.or(failed);
    if s == *base && problem.is_some() {
        return Err(problem.unwrap_or_default());
    }
    Ok((s, problem.map(|p| format!("partly: {p}"))))
}

/// Start refreshing (the interactive app, once).
pub fn start() {
    econ_ui::set_refresh(|r| r.not_this_run = false);
    let spawned = std::thread::Builder::new()
        .name("lines-econ-refresh".into())
        .spawn(|| {
            loop {
                let key = match api_key() {
                    Ok(Some(k)) => k,
                    Ok(None) => {
                        econ_ui::set_refresh(|r| {
                            r.no_key = true;
                            r.problem = None;
                        });
                        sleep(EVERY);
                        continue;
                    }
                    Err(e) => {
                        econ_ui::set_refresh(|r| r.problem = Some(format!("keychain: {e}")));
                        sleep(RETRY);
                        continue;
                    }
                };
                econ_ui::set_refresh(|r| {
                    r.no_key = false;
                    r.running = true;
                    r.problem = None;
                });
                // Always onto the build's calendar: its corrections are never lost.
                let base = lines_feed::econ::bundled();
                let now = lines_feed::now_utc();
                let wait = match refresh_once(&base, &key, now) {
                    Ok((s, partly)) => {
                        tracing::info!(
                            "economic calendar refreshed from FRED (calendar of {})",
                            s.version
                        );
                        if let Some(path) = cache_path()
                            && let Err(e) = write_cache(&path, &s)
                        {
                            tracing::warn!("economic calendar: can't save the cache: {e}");
                        }
                        econ_ui::replace(Arc::new(s));
                        let wait = if partly.is_some() { RETRY } else { EVERY };
                        econ_ui::set_refresh(|r| {
                            r.updated = Some(now);
                            r.problem = partly;
                        });
                        wait
                    }
                    Err(e) => {
                        tracing::warn!("economic calendar refresh failed: {e}");
                        econ_ui::set_refresh(|r| r.problem = Some(e));
                        RETRY
                    }
                };
                econ_ui::set_refresh(|r| r.running = false);
                sleep(wait);
            }
        });
    if let Err(e) = spawned {
        tracing::error!("economic calendar: can't start the refresh: {e}");
        econ_ui::set_refresh(|r| r.problem = Some("couldn't start".to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache stands in for the bundled calendar only when built on this build's calendar
    /// and at least as new.
    #[test]
    fn the_cache_is_used_only_when_built_on_this_calendar() {
        let b = lines_feed::econ::bundled();
        let h = lines_feed::econ::bundled_hash();
        let cache = |body: &str, hash: u64| format!("{}\n{body}", base_line(hash));
        let same = b.to_text();
        assert!(usable_cache(&cache(&same, h), &b, h).is_some());
        assert!(
            usable_cache(&cache(&same, h ^ 1), &b, h).is_none(),
            "another build's calendar"
        );
        assert!(usable_cache(&same, &b, h).is_none(), "no base line");
        let older = same.replacen(&format!("version {}", b.version), "version 2026-01-01", 1);
        assert!(usable_cache(&cache(&older, h), &b, h).is_none(), "older");
        assert!(usable_cache(&cache("junk", h), &b, h).is_none());
    }

    /// The owner's cache (2026-11-19): written under the first mapping (no `mapping` on its
    /// base line), holding FOMC "statements" every day from FRED. This build discards it at
    /// start; the next refresh merges onto the bundled calendar again.
    #[test]
    fn a_cache_from_the_old_mapping_is_discarded_with_its_daily_fomc() {
        let b = lines_feed::econ::bundled();
        let h = lines_feed::econ::bundled_hash();
        let today = lines_core::CivilDate::new(2026, 11, 19).unwrap();
        let daily: Vec<lines_core::CivilDate> = (19..=30)
            .map(|n| lines_core::CivilDate::new(2026, 11, n).unwrap())
            .collect();
        // What the old mapping made (no cadence check, FOMC taken from FRED).
        let bad = b
            .refreshed(&[("fomc".to_owned(), daily)], &|_| None, today, false)
            .unwrap();
        assert!(bad.events.len() > b.events.len() + 10);
        let old = format!("# base {h:016x}\n{}", bad.to_text());
        assert!(usable_cache(&old, &b, h).is_none(), "old mapping");
        // The same calendar written under this mapping would load: the line is what
        // decides.
        assert!(usable_cache(&format!("{}\n{}", base_line(h), bad.to_text()), &b, h).is_some());
    }

    /// The live path past the network (`refresh_once` → `merge`): FRED's answers as the owner
    /// got them, FOMC every day and CPI monthly, through the mapping and the cadence check
    /// onto the bundled calendar: no FOMC line is added, CPI's are.
    #[test]
    fn fred_answers_with_daily_fomc_add_no_fomc_line() {
        let b = lines_feed::econ::bundled();
        let today = lines_core::CivilDate::new(2026, 11, 19).unwrap();
        let d = |y, m, n| lines_core::CivilDate::new(y, m, n).unwrap();
        let answers = vec![
            (
                "fomc".to_owned(),
                (19..=30).map(|n| d(2026, 11, n)).collect::<Vec<_>>(),
            ),
            (
                "cpi".to_owned(),
                vec![d(2026, 12, 10), d(2027, 1, 14), d(2027, 2, 11)],
            ),
        ];
        let (s, _) = merge(&b, answers, today);
        let fomc = |s: &EconSchedule| {
            s.events
                .iter()
                .filter(|e| s.release(e).key == "fomc")
                .count()
        };
        assert_eq!(fomc(&s), fomc(&b));
        // CPI's new date (one the bundled list lacks) is added.
        assert!(
            s.events
                .iter()
                .any(|e| s.release(e).key == "cpi" && s.local_date(e.at) == d(2027, 1, 14))
        );
    }
}
