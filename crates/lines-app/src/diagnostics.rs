//! "Export diagnostics" (roadmap U1): one zip a user can attach to a support request.
//!
//! Contents (`README.txt` in the zip lists them):
//! - `about.json`: app version, build revision, OS, GPU adapter, display scale, displays, log
//!   settings, dropped log lines;
//! - `logs/`: the recent log files;
//! - `config/`: settings and workspace files;
//! - `feeds.json`: per running feed, the last few minutes of counters sampled every 5 s
//!   (updates and frames per second, gaps, rejects, reconnects, book checksum mismatches and
//!   resyncs, decode errors) and their rates;
//! - `frames.json`: per pane, paints per second and (charts) paint time.
//!
//! Everything passes through `crate::redact` on the way in: credentials, account ids, API
//! keys, user names and e-mail addresses never reach the zip (tested below with fake secrets).
//! Level data is never included. The zip is written to a folder the user picks (default:
//! Downloads) off the UI thread, and its path is shown in a notice. Nothing is uploaded.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Global, Window};
use serde::Serialize;

use crate::layout::PaneKind;
use crate::redact;
use crate::workspace::{self, Workspace};
use crate::zipw::ZipWriter;

/// Samples kept: 60 × 5 s = the last 5 minutes.
const SAMPLES: usize = 60;
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// A log file larger than this contributes only its tail.
const LOG_TAIL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize)]
pub struct About {
    pub app: String,
    pub version: String,
    pub build_rev: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub gpu: Option<String>,
    pub gpu_driver: Option<String>,
    pub gpu_software_emulated: Option<bool>,
    /// The exporting window's DPI scale.
    pub scale: Option<f32>,
    /// Each display's size in logical px.
    pub displays: Vec<String>,
    pub log_dir: Option<String>,
    pub log_files: Option<usize>,
    pub log_file_mb: Option<u64>,
    pub log_lines_dropped: u64,
    pub exported_at_unix: i64,
}

/// One feed's counters at one moment (cumulative since the feed started).
#[derive(Clone, Debug, Default, Serialize)]
pub struct FeedSample {
    pub at_ms: i64,
    /// Provider and contract, e.g. `cqg ESZ26` (no connection details).
    pub feed: String,
    pub status: String,
    pub frames_in: u64,
    pub updates_in: u64,
    pub trades_in: u64,
    pub gaps: u64,
    pub rejected: u64,
    pub decode_errors: u64,
    pub reconnects: u64,
    pub book_crossed: u64,
    pub book_checksum_mismatches: u64,
    pub book_resyncs: u64,
}

/// One pane's paint counters at one moment.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PaneSample {
    pub at_ms: i64,
    /// `chart 3`, `dom 7` (kind and the pane's id this run).
    pub pane: String,
    pub feed: String,
    pub visible: bool,
    pub paints: u64,
    /// Charts: smoothed paint time.
    pub paint_us: Option<f64>,
}

/// The last few minutes of samples (a GPUI global, filled by [`start_sampler`]).
#[derive(Default)]
pub struct Samples {
    feeds: VecDeque<Vec<FeedSample>>,
    panes: VecDeque<Vec<PaneSample>>,
}

impl Global for Samples {}

impl Samples {
    pub fn push(&mut self, feeds: Vec<FeedSample>, panes: Vec<PaneSample>) {
        self.feeds.push_back(feeds);
        self.panes.push_back(panes);
        while self.feeds.len() > SAMPLES {
            self.feeds.pop_front();
        }
        while self.panes.len() > SAMPLES {
            self.panes.pop_front();
        }
    }
}

/// Everything a bundle is built from (gathered on the UI thread; cheap to clone).
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub about: About,
    /// Settings and workspace files live here (`persist::config_dir`).
    pub config_dir: Option<PathBuf>,
    pub log_files: Vec<PathBuf>,
    pub feeds: Vec<Vec<FeedSample>>,
    pub panes: Vec<Vec<PaneSample>>,
}

#[derive(Serialize)]
struct FeedReport {
    feed: String,
    status: String,
    window_s: f64,
    updates_per_s: f64,
    frames_per_s: f64,
    /// Counts within the window.
    gaps: u64,
    rejected: u64,
    reconnects: u64,
    decode_errors: u64,
    book_checksum_mismatches: u64,
    book_resyncs: u64,
    /// Cumulative since the feed started.
    totals: FeedSample,
    samples: Vec<FeedSample>,
}

#[derive(Serialize)]
struct PaneReport {
    pane: String,
    feed: String,
    visible: bool,
    window_s: f64,
    paints_per_s: f64,
    paint_us: Option<f64>,
}

/// Rates over the sampled window, per feed.
fn feed_reports(history: &[Vec<FeedSample>]) -> Vec<FeedReport> {
    let mut by: BTreeMap<String, Vec<FeedSample>> = BTreeMap::new();
    for snap in history {
        for s in snap {
            by.entry(s.feed.clone()).or_default().push(s.clone());
        }
    }
    by.into_iter()
        .filter_map(|(feed, samples)| {
            let (a, b) = (samples.first()?.clone(), samples.last()?.clone());
            let dt = ((b.at_ms - a.at_ms) as f64 / 1000.0).max(0.0);
            let rate = |x: u64, y: u64| {
                if dt > 0.0 {
                    y.saturating_sub(x) as f64 / dt
                } else {
                    0.0
                }
            };
            Some(FeedReport {
                feed,
                status: b.status.clone(),
                window_s: dt,
                updates_per_s: rate(a.updates_in, b.updates_in),
                frames_per_s: rate(a.frames_in, b.frames_in),
                gaps: b.gaps.saturating_sub(a.gaps),
                rejected: b.rejected.saturating_sub(a.rejected),
                reconnects: b.reconnects.saturating_sub(a.reconnects),
                decode_errors: b.decode_errors.saturating_sub(a.decode_errors),
                book_checksum_mismatches: b
                    .book_checksum_mismatches
                    .saturating_sub(a.book_checksum_mismatches),
                book_resyncs: b.book_resyncs.saturating_sub(a.book_resyncs),
                totals: b,
                samples,
            })
        })
        .collect()
}

fn pane_reports(history: &[Vec<PaneSample>]) -> Vec<PaneReport> {
    let mut by: BTreeMap<String, Vec<PaneSample>> = BTreeMap::new();
    for snap in history {
        for s in snap {
            by.entry(s.pane.clone()).or_default().push(s.clone());
        }
    }
    by.into_iter()
        .filter_map(|(pane, samples)| {
            let (a, b) = (samples.first()?, samples.last()?);
            let dt = ((b.at_ms - a.at_ms) as f64 / 1000.0).max(0.0);
            Some(PaneReport {
                pane,
                feed: b.feed.clone(),
                visible: b.visible,
                window_s: dt,
                paints_per_s: if dt > 0.0 {
                    b.paints.saturating_sub(a.paints) as f64 / dt
                } else {
                    0.0
                },
                paint_us: b.paint_us,
            })
        })
        .collect()
}

const README: &str = "Lines diagnostics bundle\n\
\n\
Made on this computer by \"Export diagnostics\"; nothing was uploaded. Attach it to a\n\
support request if you want to.\n\
\n\
about.json   version, build, OS, GPU, display scale, log settings\n\
logs/        recent log files\n\
config/      settings and workspace files\n\
feeds.json   per feed: updates and frames per second, gaps, rejects, reconnects,\n\
             book checksum mismatches over the last few minutes\n\
frames.json  per pane: paints per second, chart paint time\n\
\n\
Credentials, account ids, API keys, user names and e-mail addresses are removed\n\
(shown as [redacted]). Market level data is never included.\n";

/// The settings and workspace files in `dir`: `settings.json`, `workspace.json` and saved
/// workspaces, by their name in the bundle. Backups and other files are left out.
fn config_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for name in ["settings.json", "workspace.json"] {
        let p = dir.join(name);
        if p.is_file() {
            out.push((format!("config/{name}"), p));
        }
    }
    if let Ok(rd) = std::fs::read_dir(crate::persist::named_dir(dir)) {
        let mut named: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        named.sort();
        for p in named {
            if let Some(n) = p.file_name() {
                out.push((format!("config/workspaces/{}", n.to_string_lossy()), p));
            }
        }
    }
    out
}

/// A config file's redacted text: JSON is redacted by key; anything else line by line.
fn redacted_config(bytes: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(v) => serde_json::to_vec_pretty(&redact::json(&v)).unwrap_or_default(),
        Err(_) => redacted_text(bytes),
    }
}

fn redacted_text(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, nl) = line.split_at(line.trim_end_matches(['\r', '\n']).len());
        out.push_str(&redact::text(body));
        out.push_str(nl);
    }
    out.into_bytes()
}

/// Build the bundle (blocking: run it off the UI thread).
pub fn build(inputs: &Inputs) -> std::io::Result<Vec<u8>> {
    let mut zip = ZipWriter::new(inputs.about.exported_at_unix);
    zip.add("README.txt", README.as_bytes())?;
    let about = serde_json::to_value(&inputs.about).unwrap_or_default();
    zip.add(
        "about.json",
        &serde_json::to_vec_pretty(&redact::json(&about)).unwrap_or_default(),
    )?;
    for (i, path) in inputs.log_files.iter().enumerate() {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let tail = &bytes[bytes.len().saturating_sub(LOG_TAIL_BYTES)..];
        let name = path.file_name().map_or_else(
            || format!("lines.{i}.log"),
            |n| n.to_string_lossy().into_owned(),
        );
        zip.add(&format!("logs/{name}"), &redacted_text(tail))?;
    }
    if let Some(dir) = &inputs.config_dir {
        for (name, path) in config_files(dir) {
            if let Ok(bytes) = std::fs::read(&path) {
                zip.add(&name, &redacted_config(&bytes))?;
            }
        }
    }
    let feeds = serde_json::to_value(feed_reports(&inputs.feeds)).unwrap_or_default();
    zip.add(
        "feeds.json",
        &serde_json::to_vec_pretty(&redact::json(&feeds)).unwrap_or_default(),
    )?;
    let panes = serde_json::to_value(pane_reports(&inputs.panes)).unwrap_or_default();
    zip.add(
        "frames.json",
        &serde_json::to_vec_pretty(&redact::json(&panes)).unwrap_or_default(),
    )?;
    Ok(zip.finish())
}

// ---- in the app ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    lines_feed::now_utc().0
}

/// Take one sample of every feed and pane in the workspace.
fn sample(cx: &mut App) -> (Vec<FeedSample>, Vec<PaneSample>) {
    let at_ms = now_ms();
    let panes = workspace::all_panes(cx);
    let mut feeds: Vec<FeedSample> = Vec::new();
    let mut seen: Vec<Arc<lines_feed::store::ChartStore>> = Vec::new();
    let mut pane_samples = Vec::new();
    for (id, handle) in panes {
        let feed = handle.feed(cx);
        let store = feed.store().clone();
        let (label, stats, status) = match store.try_read() {
            Some(d) => (
                format!(
                    "{} {}",
                    d.provider,
                    d.contract_label()
                        .map_or_else(|| d.product.symbol.to_owned(), |l| l.to_string())
                ),
                d.stats,
                d.status.label().to_owned(),
            ),
            None => continue,
        };
        let label = if feed.unavailable.is_some() {
            format!("{label} (unavailable)")
        } else {
            label
        };
        if feed.unavailable.is_none() && !seen.iter().any(|s| Arc::ptr_eq(s, &store)) {
            seen.push(store.clone());
            feeds.push(FeedSample {
                at_ms,
                feed: label.clone(),
                status,
                frames_in: stats.frames_in,
                updates_in: stats.updates_in,
                trades_in: stats.trades_in,
                gaps: stats.gaps,
                rejected: stats.rejected,
                decode_errors: stats.decode_errors,
                reconnects: stats.reconnects,
                book_crossed: stats.book_crossed,
                book_checksum_mismatches: stats.book_checksum_mismatches,
                book_resyncs: stats.book_resyncs,
            });
        }
        let kind = match handle.kind() {
            PaneKind::Chart => "chart",
            PaneKind::Dom => "dom",
            PaneKind::Tape => "tape",
            PaneKind::Journal => "journal",
            PaneKind::Strategies => "strategies",
            PaneKind::Chain => "chain",
            PaneKind::Positions => "positions",
        };
        let (visible, paints, paint_us) = handle.perf(cx);
        pane_samples.push(PaneSample {
            at_ms,
            pane: format!("{kind} {}", id.0),
            feed: label,
            visible,
            paints,
            paint_us,
        });
    }
    (feeds, pane_samples)
}

/// Sample feeds and panes every 5 s (cheap: counters only), keeping the last 5 minutes.
pub fn start_sampler(cx: &mut App) {
    cx.set_global(Samples::default());
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(SAMPLE_EVERY).await;
            cx.update(|cx| {
                let (f, p) = sample(cx);
                cx.global_mut::<Samples>().push(f, p);
            });
        }
    })
    .detach();
}

/// Best-effort OS version text (blocking: off the UI thread).
fn os_version() -> String {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    if cfg!(windows) {
        run("cmd", &["/C", "ver"])
    } else if cfg!(target_os = "macos") {
        run("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}"))
    } else {
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                    .map(|v| v.trim_matches('"').to_owned())
            })
    }
    .unwrap_or_else(|| "unknown".into())
}

/// Where the save dialog starts: Downloads, else the data directory, else the temp directory.
fn default_folder() -> PathBuf {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    home.map(|h| h.join("Downloads"))
        .filter(|d| d.is_dir())
        .or_else(crate::data_dir::root)
        .unwrap_or_else(std::env::temp_dir)
}

/// `lines-diagnostics-20261106-140501.zip` (UTC).
fn file_name(unix: i64) -> String {
    let t = lines_core::UtcMillis(unix * 1000);
    let iso = t.to_string(); // 2026-11-06T14:05:01.000Z
    let digits: String = iso
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    let (d, h) = digits.split_at(8.min(digits.len()));
    format!("lines-diagnostics-{d}-{h}.zip")
}

/// Run "Export diagnostics" from `window`: ask where to save (default: Downloads), build the
/// bundle off the UI thread, and say where it went.
pub fn export(window: &mut Window, cx: &mut App) {
    let unix = now_ms() / 1000;
    let log = crate::logs::config();
    let gpu = window.gpu_specs();
    let about = About {
        app: "Lines".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        build_rev: crate::logs::build_rev().into(),
        os: std::env::consts::OS.into(),
        os_version: String::new(),
        arch: std::env::consts::ARCH.into(),
        gpu: gpu.as_ref().map(|g| g.device_name.clone()),
        gpu_driver: gpu.as_ref().map(|g| {
            format!("{} {}", g.driver_name, g.driver_info)
                .trim()
                .to_owned()
        }),
        gpu_software_emulated: gpu.as_ref().map(|g| g.is_software_emulated),
        scale: Some(window.scale_factor()),
        displays: cx
            .displays()
            .iter()
            .map(|d| {
                let b = d.bounds();
                format!("{}x{}", f32::from(b.size.width), f32::from(b.size.height))
            })
            .collect(),
        log_dir: log.as_ref().map(|c| c.dir.display().to_string()),
        log_files: log.as_ref().map(|c| c.files),
        log_file_mb: log.as_ref().map(|c| c.max_bytes / (1024 * 1024)),
        log_lines_dropped: crate::logs::dropped(),
        exported_at_unix: unix,
    };
    // A fresh sample, so a bundle made right after start still has numbers.
    let (f, p) = sample(cx);
    if cx.has_global::<Samples>() {
        cx.global_mut::<Samples>().push(f, p);
    }
    let (feeds, panes) = cx
        .try_global::<Samples>()
        .map(|s| {
            (
                s.feeds.iter().cloned().collect(),
                s.panes.iter().cloned().collect(),
            )
        })
        .unwrap_or_default();
    let inputs = Inputs {
        about,
        config_dir: cx.global::<Workspace>().config_dir().map(Path::to_path_buf),
        log_files: log.as_ref().map(|c| c.existing()).unwrap_or_default(),
        feeds,
        panes,
    };
    let folder = default_folder();
    let name = file_name(unix);
    let chosen = cx.prompt_for_new_path(&folder, Some(&name));
    let fallback = folder.join(&name);
    cx.spawn(async move |cx| {
        let path = match chosen.await {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return, // cancelled
            // No dialog on this platform (or it failed): the default place.
            _ => fallback,
        };
        let target = path.clone();
        let written = cx
            .background_executor()
            .spawn(async move {
                let mut inputs = inputs;
                inputs.about.os_version = os_version();
                let bytes = build(&inputs)?;
                if let Some(dir) = target.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&target, bytes)
            })
            .await;
        cx.update(|cx| {
            let msg = match written {
                Ok(()) => format!(
                    "diagnostics saved to {} (nothing was uploaded)",
                    path.display()
                ),
                Err(e) => format!("couldn't save diagnostics to {}: {e}", path.display()),
            };
            tracing::info!("{msg}");
            cx.global_mut::<Workspace>().add_report(msg);
            cx.refresh_windows();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zipw;

    /// Fake secrets of every kind a user's files might hold.
    const SECRETS: &[&str] = &[
        "trader-FAKE-user",
        "FAKE-pass-123",
        "FAKE-ACCT-0001",
        "FAKE-ACCT-0002",
        "FAKE-ACCT-0003",
        "sk_FAKE_0123456789abcdefABCDEF",
        // A Databento key's shape with no digit (the general token rule needs one).
        "db-FAKEFAKEFAKEFAKEFAKEFAKEFAKEF",
        "fake.person@example.com",
        "FAKE-session-cookie",
        "9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        // Account ids as the keys of account-keyed settings maps.
        "FAKE-ACCT-0004",
        "FAKE-ACCT-0005",
        "FAKE-ACCT-0006",
        // The OS user name in paths (logs and about.json's log_dir).
        "FAKEUSERNAME",
        "FAKEGIVEN",
        "FAKESURNAME",
        // An Authorization header's credentials and URL signing parameters.
        "FAKE-bearer-credential",
        "FAKEQUERYKEY",
        "FAKEQUERYSIG",
    ];

    fn temp(name: &str) -> lines_core::scratch::Scratch {
        lines_core::scratch::Scratch::new(&format!("diag-{name}"))
    }

    #[test]
    fn no_secret_survives_into_a_bundle() {
        let dir = temp("redact");
        let config = dir.join("config");
        std::fs::create_dir_all(config.join("workspaces")).unwrap();
        std::fs::write(
            config.join("settings.json"),
            r#"{"version":1,"recents":[2,1],"favourites":[1],
                "connections":{"rithmic":{"user":"trader-FAKE-user","password":"FAKE-pass-123",
                  "account_ids":["FAKE-ACCT-0001"],"api_key":"sk_FAKE_0123456789abcdefABCDEF"}},
                "contact":"fake.person@example.com","session":"FAKE-session-cookie",
                "note":"databento db-FAKEFAKEFAKEFAKEFAKEFAKEFAKEF",
                "guardrails":{"FAKE-ACCT-0004":{"max_trades_per_day":3}},
                "bracket_presets":{"FAKE-ACCT-0005":{"presets":[]}},
                "prop_rules":{"FAKE-ACCT-0006":{"preset":"50K evaluation"}}}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("workspace.json"),
            r#"{"version":2,"name":"desk","windows":[{"tabs":[{"layout":{"pane":{"kind":"dom",
                "account":"FAKE-ACCT-0002","state":"shown"}}}]}]}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("workspaces").join("prop.json"),
            r#"{"version":2,"name":"prop","notes":"account_id=FAKE-ACCT-0003 key 9f8e7d6c5b4a39281706f5e4d3c2b1a0"}"#,
        )
        .unwrap();
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("lines.log"),
            "2026-11-06 INFO logon user=trader-FAKE-user password=FAKE-pass-123\n\
             2026-11-06 INFO broker account_id: FAKE-ACCT-0001 ready\n\
             2026-11-06 WARN mail to fake.person@example.com failed\n\
             2026-11-06 INFO feed ESZ26 live\n\
             2026-11-06 INFO logs in C:\\Users\\FAKEUSERNAME\\AppData\\Local\\lines\\logs\n\
             2026-11-06 INFO saved \"C:\\\\Users\\\\FAKEUSERNAME\\\\AppData\\\\x.json\"\n\
             2026-11-06 DEBUG Authorization: Bearer FAKE-bearer-credential\n\
             2026-11-06 INFO saved C:\\Users\\FAKEGIVEN FAKESURNAME\\Documents\\w.json\n\
             2026-11-06 DEBUG GET https://example.invalid/v1?key=FAKEQUERYKEY&sig=FAKEQUERYSIG\n",
        )
        .unwrap();
        let inputs = Inputs {
            about: About {
                version: "0.1.0".into(),
                exported_at_unix: 1_762_437_600,
                log_dir: Some(r"C:\Users\FAKEUSERNAME\AppData\Local\lines\logs".into()),
                ..Default::default()
            },
            config_dir: Some(config),
            log_files: vec![logs.join("lines.log")],
            feeds: vec![vec![FeedSample {
                at_ms: 0,
                feed: "cqg ESZ26".into(),
                updates_in: 100,
                ..Default::default()
            }]],
            panes: vec![],
        };
        let zip = build(&inputs).unwrap();
        let entries = zipw::read(&zip);
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        for want in [
            "README.txt",
            "about.json",
            "logs/lines.log",
            "config/settings.json",
            "config/workspace.json",
            "config/workspaces/prop.json",
            "feeds.json",
            "frames.json",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for (name, data) in &entries {
            let text = String::from_utf8_lossy(data);
            for s in SECRETS {
                assert!(!text.contains(s), "{s} survived in {name}:\n{text}");
            }
        }
        // What isn't secret is still there to help.
        let all: String = entries
            .iter()
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
            .collect();
        for keep in [
            "recents",
            "feed ESZ26 live",
            "\"name\": \"desk\"",
            "cqg ESZ26",
            "0.1.0",
        ] {
            assert!(all.contains(keep), "{keep} lost");
        }
    }

    /// The journal folder (its lines hold account ids and its charts show trades) stays out of
    /// the bundle; a journal line that ever passed through redaction loses its account, and the
    /// chart's file name in it isn't a path.
    #[test]
    fn journal_files_and_charts_stay_out_of_a_bundle() {
        let dir = temp("journal");
        let shots = dir.join("journal").join("shots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"version":1}"#).unwrap();
        let name = "2026-11-05-0123456789abcdef.svg";
        std::fs::write(shots.join(name), "<svg>FAKE-CHART</svg>").unwrap();
        let line = serde_json::json!({
            "trade": {"account": "FAKE-ACCT-0007", "screenshots": [name]}
        });
        std::fs::write(dir.join("journal").join("mock.jsonl"), line.to_string()).unwrap();
        let inputs = Inputs {
            config_dir: Some(dir.to_path_buf()),
            ..Default::default()
        };
        let entries = zipw::read(&build(&inputs).unwrap());
        for (entry, data) in &entries {
            assert!(
                !entry.contains("journal") && !entry.contains("shots"),
                "{entry}"
            );
            let text = String::from_utf8_lossy(data);
            for s in ["FAKE-CHART", "FAKE-ACCT-0007", name] {
                assert!(!text.contains(s), "{s} in {entry}");
            }
        }
        let red = redact::json(&line).to_string();
        assert!(!red.contains("FAKE-ACCT-0007"), "{red}");
        assert!(!red.contains('/') && !red.contains('\\'), "{red}");
    }

    #[test]
    fn feed_rates_over_the_window() {
        let s = |at_ms, updates_in, gaps, reconnects| FeedSample {
            at_ms,
            feed: "cqg ESZ26".into(),
            updates_in,
            frames_in: updates_in / 10,
            gaps,
            reconnects,
            ..Default::default()
        };
        let history = vec![
            vec![s(0, 1_000, 1, 0)],
            vec![s(5_000, 26_000, 1, 0)],
            vec![s(10_000, 51_000, 3, 1)],
        ];
        let r = feed_reports(&history);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].updates_per_s, 5_000.0);
        assert_eq!(r[0].frames_per_s, 500.0);
        assert_eq!((r[0].gaps, r[0].reconnects), (2, 1));
        assert_eq!(r[0].totals.updates_in, 51_000);
    }

    #[test]
    fn file_names_sort_by_time() {
        assert_eq!(
            file_name(1_762_437_901),
            "lines-diagnostics-20251106-140501.zip"
        );
    }
}
