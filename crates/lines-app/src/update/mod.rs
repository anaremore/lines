//! In-app updates (decision 0031): check the release feed at start and every 6 hours,
//! download a newer release in the background, verify it ([`verify`]), and offer
//! **Restart to update**, held while anything is trading ([`hold`]). The restart backs up the
//! saved files, swaps the executable ([`install`]) and starts the new one after this one has
//! saved and quit; two failed starts of the new version offer the rollback, which Settings ›
//! About also has.
//!
//! Interactive runs only: tests, screenshots, bench, backtests and headless runs never reach
//! [`start`], and `--no-update-check` or Settings › General › Updates: Off make no request.
//! A build without the project's public key ([`verify::PUBLIC_KEY`]) never checks.

pub mod hold;
pub mod install;
pub mod manifest;
pub mod verify;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use manifest::Version;

/// Settings › General › Updates (decision 0031 §6).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    /// Download in the background and ask to restart.
    #[default]
    Automatic,
    /// Say a newer version exists; download nothing.
    NotifyOnly,
    /// No request at all.
    Off,
}

impl UpdateMode {
    pub const LABELS: [&'static str; 3] = ["Automatic", "Notify only", "Off"];
    pub const ALL: [UpdateMode; 3] = [
        UpdateMode::Automatic,
        UpdateMode::NotifyOnly,
        UpdateMode::Off,
    ];

    pub fn is_default(&self) -> bool {
        *self == UpdateMode::Automatic
    }

    pub fn index(self) -> usize {
        UpdateMode::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }
}

/// How often the feed is checked.
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How long the updated version must run before its start counts as good.
const CONFIRM_AFTER: Duration = Duration::from_secs(30);
/// The public feed (decision 0031: public builds update from the public repository).
const FEED_URL: &str = "https://api.github.com/repos/anaremore/lines/releases?per_page=10";
/// The largest executable or manifest accepted (a guard against a runaway download).
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;

/// What the updater has to say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    /// Notify only: a newer version exists.
    Available(Version),
    /// Downloaded and verified, staged at this path.
    Ready(Version, PathBuf),
    /// A download that failed verification was discarded (and never run).
    Rejected(Version, String),
}

/// The app-wide updater (a GPUI global, present only in interactive runs that check).
pub struct Updater {
    config: PathBuf,
    status: Arc<Mutex<Status>>,
    mode: Arc<Mutex<UpdateMode>>,
    /// What a restart would interrupt now (refreshed with the workspace's status line).
    pub activity: hold::Activity,
    /// The version to roll back to, offered after two failed starts of this one.
    pub offer_rollback: Option<String>,
}

impl Global for Updater {}

impl Updater {
    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Where a restart started from `lib.rs` after the app quit and saved.
static RELAUNCH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// After the app has quit and flushed its saves: start the executable a restart or rollback
/// put in place, with this run's arguments.
pub fn relaunch_if_requested() {
    let Some(exe) = RELAUNCH.lock().ok().and_then(|mut r| r.take()) else {
        return;
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let Err(e) = std::process::Command::new(&exe).args(args).spawn() {
        tracing::error!("couldn't start {}: {e}", exe.display());
    }
}

/// The version a rollback returns to, if the last update kept one.
pub fn rollback_target(config: &Path) -> Option<String> {
    let s = install::load_state(config)?;
    let exe = std::env::current_exe().ok()?;
    (s.to == Version::current().to_string() && install::previous_exe(&exe).exists())
        .then_some(s.from)
}

/// Start the updater: count this start (offering the rollback after two failed ones), then
/// check in the background. Interactive runs only (`lib.rs`), never with `--no-update-check`.
pub fn start(config: PathBuf, mode: UpdateMode, cx: &mut App) {
    let current = Version::current().to_string();
    let (state, offer) = install::on_start(install::load_state(&config), &current);
    if let Some(s) = &state {
        let _ = install::save_state(&config, s);
    }
    let offer_rollback = offer
        .then(|| state.as_ref().map(|s| s.from.clone()))
        .flatten();
    let status = Arc::new(Mutex::new(Status::Idle));
    let mode = Arc::new(Mutex::new(mode));
    cx.set_global(Updater {
        config: config.clone(),
        status: status.clone(),
        mode: mode.clone(),
        activity: hold::Activity::default(),
        offer_rollback,
    });
    // This version ran long enough: its start counts as good.
    if state.is_some_and(|s| s.to == current && !s.confirmed) {
        let c = config.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(CONFIRM_AFTER).await;
            if let Some(mut s) = install::load_state(&c)
                && s.to == Version::current().to_string()
            {
                s.confirmed = true;
                let _ = install::save_state(&c, &s);
            }
            cx.update(|cx| {
                if cx.has_global::<Updater>() {
                    cx.global_mut::<Updater>().offer_rollback = None;
                }
            });
        })
        .detach();
    }
    if verify::PUBLIC_KEY.is_none() {
        tracing::info!("updates: this build has no update key; it never checks");
        return;
    }
    std::thread::Builder::new()
        .name("updates".into())
        .spawn(move || {
            loop {
                let m = mode.lock().map(|m| *m).unwrap_or(UpdateMode::Off);
                if m != UpdateMode::Off {
                    match check(&config, m) {
                        Ok(s) => {
                            if let Ok(mut st) = status.lock() {
                                *st = s;
                            }
                        }
                        Err(e) => tracing::warn!("updates: {e}"),
                    }
                    std::thread::sleep(EVERY);
                } else {
                    // Off: no request; look again when the setting may have changed.
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
        })
        .ok();
}

/// Screenshots (`--update-shot ready|available|rejected|rollback`): the updater in that state,
/// with no request and nothing on disk. A `--paper` position shows the trade hold.
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
pub fn screenshot(kind: &str, cx: &mut App) -> Result<(), String> {
    let v = Version(0, 3, 99);
    let (status, offer_rollback) = match kind {
        "ready" => (Status::Ready(v, PathBuf::from("lines.exe")), None),
        "available" => (Status::Available(v), None),
        "rejected" => (
            Status::Rejected(v, "its signature didn't verify".into()),
            None,
        ),
        "rollback" => (Status::Idle, Some("0.3.10".to_owned())),
        other => {
            return Err(format!(
                "{other}: expected ready, available, rejected or rollback"
            ));
        }
    };
    cx.set_global(Updater {
        config: PathBuf::new(),
        status: Arc::new(Mutex::new(status)),
        mode: Arc::new(Mutex::new(UpdateMode::Automatic)),
        activity: hold::Activity::default(),
        offer_rollback,
    });
    Ok(())
}

/// The mode changed in Settings: the updater's thread reads it before its next check.
pub fn set_mode(mode: UpdateMode, cx: &mut App) {
    if let Some(u) = cx.try_global::<Updater>()
        && let Ok(mut m) = u.mode.lock()
    {
        *m = mode;
    }
}

fn http_get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("lines/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    resp.body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(|e| format!("GET {url}: {e}"))
}

/// One check: the feed, then (if newer) the manifest, then (Automatic) the download, verified
/// and staged. Blocking (the updater's own thread).
fn check(config: &Path, mode: UpdateMode) -> Result<Status, String> {
    let current = Version::current();
    let feed = manifest::parse_feed(&http_get(FEED_URL, Duration::from_secs(20))?)?;
    let Some((v, manifest_url)) = manifest::newest(&feed, current) else {
        return Ok(Status::Idle);
    };
    if mode == UpdateMode::NotifyOnly {
        return Ok(Status::Available(v));
    }
    let staged = staged_path(config, v);
    if staged.is_file() {
        return Ok(Status::Ready(v, staged));
    }
    let m = manifest::parse_manifest(&http_get(&manifest_url, Duration::from_secs(20))?)
        .map_err(|e| e.to_string())?;
    let (v, file) =
        manifest::pick(&m, v, current, manifest::target()).map_err(|e| e.to_string())?;
    let data = http_get(&file.url, Duration::from_secs(30 * 60))?;
    Ok(stage(
        config,
        v,
        &data,
        &file.sha256,
        &file.signature,
        verify::PUBLIC_KEY,
    ))
}

/// Where a verified download waits for the restart.
pub fn staged_path(config: &Path, v: Version) -> PathBuf {
    let name = if cfg!(windows) { "lines.exe" } else { "lines" };
    config.join("updates").join(v.to_string()).join(name)
}

/// Verify `data` and stage it, or discard it and say why (decision 0031 §3: never run).
pub fn stage(
    config: &Path,
    v: Version,
    data: &[u8],
    sha256: &str,
    signature: &str,
    key: Option<&str>,
) -> Status {
    if let Err(why) = verify::verify(data, sha256, signature, key) {
        tracing::error!("updates: v{v} discarded: {why}");
        return Status::Rejected(v, why.to_string());
    }
    let path = staged_path(config, v);
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| crate::versioned::save(&path, data));
    match written {
        Ok(()) => {
            // A sidecar records what the staged file must still match at the restart.
            let _ = std::fs::write(path.with_extension("sha256"), verify::sha256_hex(data));
            let _ = std::fs::write(path.with_extension("minisig"), signature);
            Status::Ready(v, path)
        }
        Err(e) => {
            tracing::error!("updates: couldn't stage v{v}: {e}");
            Status::Idle
        }
    }
}

/// Why a restart (to update or roll back) can't happen now, if it can't.
pub fn held(cx: &App) -> Option<String> {
    hold::hold_reason(cx.try_global::<Updater>()?.activity)
}

/// Refresh what a restart would interrupt (with the workspace's status line, once a second).
pub fn refresh_activity(cx: &mut App) {
    if cx.try_global::<Updater>().is_none() {
        return;
    }
    let mut a = hold::Activity::default();
    if let Some(t) = crate::trading::Trading::try_global(cx) {
        let accounts = t.accounts();
        let Some(st) = t.cache.try_read() else {
            return; // busy this instant: last time's stands
        };
        for acct in &accounts {
            a.positions += st.positions_of(&acct.id).count();
            a.orders += st.open_orders_of(&acct.id).count();
        }
        a.triggers = st
            .triggers()
            .filter(|t| t.state == lines_trading::trigger::TriggerState::Armed)
            .count();
    }
    if let Some(s) = cx.try_global::<crate::strategy_panel::Strategies>() {
        a.strategies =
            s.0.snapshot()
                .iter()
                .filter(|i| {
                    i.state == lines_trading::strategy::StrategyState::Armed && i.fault.is_none()
                })
                .count();
    }
    if cx.has_global::<Updater>() {
        cx.global_mut::<Updater>().activity = a;
    }
}

/// Restart to update: refused while anything is trading (the reason is returned); otherwise
/// re-verify the staged file, back up the saved files, swap the executable, and quit (the new
/// version starts once this one has saved: [`relaunch_if_requested`]).
pub fn restart_to_update(cx: &mut App) -> Result<(), String> {
    if let Some(why) = held(cx) {
        return Err(why);
    }
    let u = cx
        .try_global::<Updater>()
        .ok_or("updates are off in this run")?;
    let Status::Ready(v, staged) = u.status() else {
        return Err("no update is ready".into());
    };
    let config = u.config.clone();
    let data = std::fs::read(&staged).map_err(|e| format!("the update is gone: {e}"))?;
    let sha = std::fs::read_to_string(staged.with_extension("sha256")).unwrap_or_default();
    let sig = std::fs::read_to_string(staged.with_extension("minisig")).unwrap_or_default();
    if let Err(why) = verify::verify(&data, &sha, &sig, verify::PUBLIC_KEY) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("v{v} discarded: {why}"));
    }
    let current = Version::current().to_string();
    install::backup(&config, &current).map_err(|e| format!("couldn't back up settings: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    install::swap(&exe, &staged).map_err(|e| format!("couldn't install v{v}: {e}"))?;
    let _ = install::save_state(
        &config,
        &install::UpdateState {
            from: current,
            to: v.to_string(),
            starts: 0,
            confirmed: false,
        },
    );
    if let Ok(mut r) = RELAUNCH.lock() {
        *r = Some(exe);
    }
    cx.quit();
    Ok(())
}

/// Roll back to the version the last update replaced: its executable and its saved files.
/// Held as a restart to update is.
pub fn roll_back(cx: &mut App) -> Result<(), String> {
    if let Some(why) = held(cx) {
        return Err(why);
    }
    let config = crate::persist::config_dir().ok_or("no settings folder")?;
    let target = rollback_target(&config).ok_or("no earlier version is kept")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    install::roll_back_exe(&exe).map_err(|e| format!("couldn't roll back: {e}"))?;
    if let Err(e) = install::restore(&config, &target) {
        tracing::error!("updates: rolled back to v{target} but its saved files: {e}");
    }
    let _ = std::fs::remove_file(install::state_path(&config));
    if let Ok(mut r) = RELAUNCH.lock() {
        *r = Some(exe);
    }
    cx.quit();
    Ok(())
}

/// The status line's word on updates: text, whether it's a warning, and its link's command.
pub fn status_label(cx: &App) -> Option<(String, bool, Option<crate::commands::Command>)> {
    use crate::commands::Command;
    let u = cx.try_global::<Updater>()?;
    if let Some(v) = &u.offer_rollback {
        return Some((
            format!("This version failed to start twice: Roll back to v{v}"),
            true,
            Some(Command::RollBackUpdate),
        ));
    }
    match u.status() {
        Status::Idle => None,
        Status::Available(v) => Some((format!("Update available: v{v}"), false, None)),
        Status::Ready(v, _) => Some(match held(cx) {
            Some(why) => (format!("Update ready: v{v} · {why}"), false, None),
            None => (
                format!("Update ready: v{v} · Restart to update"),
                false,
                Some(Command::RestartToUpdate),
            ),
        }),
        Status::Rejected(v, why) => Some((format!("Update v{v} discarded: {why}"), true, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use verify::test_key::TestKey;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lines-stage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A verified download is staged; a tampered one or a wrong signature is discarded,
    /// reported, and nothing is written where a restart would find it.
    #[test]
    fn only_a_verified_download_is_staged() {
        let c = dir("verify");
        let k = TestKey::new();
        let v = Version(0, 3, 12);
        let exe = b"MZ new lines";
        let sig = k.sign(exe);
        let sha = verify::sha256_hex(exe);
        match stage(&c, v, exe, &sha, &sig, Some(&k.public)) {
            Status::Ready(sv, p) => {
                assert_eq!(sv, v);
                assert_eq!(std::fs::read(&p).unwrap(), exe);
            }
            s => panic!("{s:?}"),
        }
        let _ = std::fs::remove_dir_all(&c);
        let c = dir("tamper");
        let bad = b"MZ new lines, changed";
        assert!(matches!(
            stage(&c, v, bad, &sha, &sig, Some(&k.public)),
            Status::Rejected(_, _)
        ));
        let other = TestKey::new();
        assert!(matches!(
            stage(&c, v, exe, &sha, &other.sign(exe), Some(&k.public)),
            Status::Rejected(_, _)
        ));
        assert!(matches!(
            stage(&c, v, exe, &sha, &sig, None),
            Status::Rejected(_, _)
        ));
        assert!(!staged_path(&c, v).exists(), "nothing staged");
        let _ = std::fs::remove_dir_all(&c);
    }

    #[test]
    fn the_mode_saves_by_name() {
        assert_eq!(
            serde_json::to_string(&UpdateMode::NotifyOnly).unwrap(),
            "\"notify_only\""
        );
        assert!(UpdateMode::default().is_default());
        assert_eq!(UpdateMode::Off.index(), 2);
    }
}
