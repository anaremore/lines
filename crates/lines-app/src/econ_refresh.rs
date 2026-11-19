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
//!   the cache records which build's calendar that was (`# base <hash>`): a build whose
//!   calendar changed (a corrected time, a new product) never uses an older build's cache.

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
    format!("# base {hash:016x}")
}

/// The cache, if it may stand in for `bundled`: built on this build's calendar (`hash`),
/// parses, and is at least as new.
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
            "economic calendar: the cache is from another build, older or unreadable; bundled"
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

/// One refresh of `base` as of `now`: the refreshed calendar and what didn't come in (some
/// releases may fail while others refresh), or the problem when nothing did.
pub fn refresh_once(
    base: &EconSchedule,
    key: &str,
    now: lines_core::UtcMillis,
) -> Result<(EconSchedule, Option<String>), String> {
    let today = base.local_date(now);
    let got = fred::fetch(base, key, today);
    if got.dates.is_empty() {
        return Err(got.problem.unwrap_or_else(|| "no dates".to_owned()));
    }
    let complete = got.complete(base, today);
    let s = base
        .refreshed(&got.dates, &fred::source_page, today, complete)
        .map_err(|e| e.to_string())?;
    Ok((s, got.problem.map(|p| format!("partly: {p}"))))
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
}
