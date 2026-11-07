//! The tracked benchmark (`docs/positioning.md`, decision 0016): frame time and memory with
//! 6 charts + 2 DOMs open while their feeds run.
//!
//! `lines --bench SECONDS [--workspace NAME] [--source …] [--size WxH]` opens the workspace
//! (default `6c2d`) in a hidden window for the given time, after a 2 s warm-up. Frames are
//! demand-driven, as GPUI draws a shown window: every 8 ms (a 120 Hz vsync) a frame is drawn
//! only if some view notified since the last one (a pane, or the window itself). Each frame
//! is `Window::draw`: layout, every pane that changed since the last frame repaints (cached
//! panes replay), scene built. That is the CPU side of a frame; GPU time and presentation are
//! not included (the window is never shown). The report gives frames drawn per second, paints
//! per pane kind and the process's CPU time (all threads: feeds, indicators, UI).
//!
//! `--bench-expect-idle` asserts nothing redraws without new data or input: with a replay
//! that has finished (`--source replay --speed 0`), no frame may be drawn after the warm-up
//! (exit 3 otherwise).
//!
//! On the mock source the six charts are on two contracts (ES front month and the next), three
//! each, and the DOMs one per contract: two feeds at the mock's rate (default 5,000
//! updates/s each), so almost every frame repaints every pane. A second, identical tab stays
//! in the background: it must paint nothing (hidden tabs do no paint work), and the report
//! checks it.
//!
//! `--bench-hide-doms` runs the same workspace with its two DOMs hidden (roadmap 5b): the
//! chart columns reflow over their space, the DOMs keep their feeds, and they must paint
//! nothing.
//!
//! Memory is the process's resident set (working set on Windows) at start (before the UI
//! and the feeds) and at the end. Use a release build for numbers worth comparing.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{App, AppContext};
use lines_feed::catalog::{self, Target};
use lines_feed::registry::FeedRegistry;
use lines_feed::{Source, front_month, now_utc};

use crate::feeds::Feeds;
use crate::layout::PaneKind;
use crate::picker_view::AppSettings;
use crate::tabstate::PaneState;
use crate::workspace::{self, Workspace};
use crate::wsdoc::{PaneDoc, TargetDoc, WorkspaceDoc};

const WARMUP: Duration = Duration::from_secs(2);
const FRAME: Duration = Duration::from_millis(8);

pub fn run(
    registry: FeedRegistry,
    ws: Workspace,
    mut doc: WorkspaceDoc,
    size: (f32, f32),
    seconds: u64,
    hide_doms: bool,
    expect_idle: bool,
) -> Result<i32> {
    let mock = matches!(ws.env.base.source, Source::CqgMock(_));
    if mock && doc.name == "6c2d" {
        spread_over_two_contracts(&mut doc);
    }
    if hide_doms {
        for t in &mut doc.windows[0].tabs {
            t.layout = t.layout.map(&mut |p: &PaneDoc| PaneDoc {
                state: if p.kind == PaneKind::Dom {
                    PaneState::Hidden
                } else {
                    p.state
                },
                ..p.clone()
            });
        }
        doc.normalise().map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    // The background tab: an identical copy that is never shown.
    let mut window = doc.windows[0].clone();
    let shown = window.tabs[window.active_tab].clone();
    window.tabs = vec![shown.clone(), shown];
    window.active_tab = 0;
    let charts = window.tabs[0]
        .layout
        .leaves()
        .iter()
        .filter(|p| p.kind == PaneKind::Chart)
        .count();
    let doms = window.tabs[0]
        .layout
        .leaves()
        .iter()
        .filter(|p| p.kind == PaneKind::Dom)
        .count();
    let rss_start = resident_bytes();

    // Trading headers are part of what a DOM draws: measure them (in memory, no file).
    let trading = crate::trading::Trading::start(
        crate::trading::TradingSetup::manual(),
        registry.clone(),
        ws.env.base.clone(),
    );
    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(Feeds(registry));
        cx.set_global(trading);
        cx.set_global(AppSettings::ephemeral());
        cx.set_global(crate::drawings_store::Drawings::ephemeral(Vec::new()));
        cx.set_global(ws);
        workspace::set_meta(&doc, cx);
        let handle = match workspace::open_doc_window(
            &window,
            Some(workspace::hidden_window("Lines (benchmark)", size)),
            cx,
        ) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("benchmark failed: {e:#}");
                std::process::exit(1);
            }
        };
        // Frame demand: a pane or the window itself notified since the last frame.
        let demand = Rc::new(Cell::new(1u64));
        let mut subscriptions = Vec::new();
        if let Ok(root) = handle.entity(cx) {
            let d = demand.clone();
            subscriptions.push(cx.observe(&root, move |_, _| d.set(d.get() + 1)));
        }
        let panes: Vec<workspace::PaneHandle> = handle
            .read(cx)
            .map(|wv| wv.panes_by_state().into_iter().map(|p| p.2).collect())
            .unwrap_or_default();
        for p in &panes {
            let d = demand.clone();
            subscriptions.push(p.on_notify(cx, move || d.set(d.get() + 1)));
        }
        cx.spawn(async move |cx| {
            let _subscriptions = subscriptions;
            let mut times: Vec<Duration> = Vec::new();
            let mut drawn_at = 0u64;
            let mut vsyncs = 0u64;
            let mut paints_at_start: Option<Vec<u64>> = None;
            let mut cpu_at_start = None;
            let start = Instant::now();
            let measure_from = start + WARMUP;
            let end = measure_from + Duration::from_secs(seconds);
            while Instant::now() < end {
                cx.background_executor().timer(FRAME).await;
                let measuring = Instant::now() >= measure_from;
                if measuring && paints_at_start.is_none() {
                    cpu_at_start = cpu_time();
                    paints_at_start =
                        Some(cx.update(|cx| panes.iter().map(|p| p.paints(cx)).collect()));
                }
                if measuring {
                    vsyncs += 1;
                }
                if demand.get() == drawn_at {
                    continue; // nothing asked for a frame: GPUI draws nothing either
                }
                drawn_at = demand.get();
                let r = cx.update_window(handle.into(), |_, window, cx| {
                    let t = Instant::now();
                    window.draw(cx).clear(cx);
                    t.elapsed()
                });
                match r {
                    Ok(dt) if measuring => times.push(dt),
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("benchmark failed: {e:#}");
                        std::process::exit(1);
                    }
                }
            }
            let cpu = cpu_at_start.zip(cpu_time()).map(|(a, b)| b.saturating_sub(a));
            let measured = measure_from.elapsed().as_secs_f64();
            let start_paints = paints_at_start.unwrap_or_default();
            let mut by_kind: Vec<(PaneKind, u64)> = Vec::new();
            cx.update(|cx| {
                for (i, p) in panes.iter().enumerate() {
                    let n = p.paints(cx) - start_paints.get(i).copied().unwrap_or(0);
                    match by_kind.iter_mut().find(|k| k.0 == p.kind()) {
                        Some(k) => k.1 += n,
                        None => by_kind.push((p.kind(), n)),
                    }
                }
            });
            let frames = times.len();
            println!(
                "frames drawn (demand-driven, 8 ms vsync): {frames} of {vsyncs} = {:.1} frames/s; pane paints/s: {}; process CPU: {}",
                frames as f64 / measured,
                by_kind
                    .iter()
                    .map(|(k, n)| format!("{k:?} {:.1}", *n as f64 / measured))
                    .collect::<Vec<_>>()
                    .join(", "),
                cpu.map_or_else(
                    || "n/a".to_owned(),
                    |c| format!("{:.1}% of one core", c.as_secs_f64() / measured * 100.0)
                ),
            );
            if expect_idle {
                println!(
                    "idle check: {}",
                    if frames == 0 {
                        "ok: no frame drawn without new data or input"
                    } else {
                        "FAIL: frames drawn while idle"
                    }
                );
                if frames > 0 {
                    std::process::exit(3);
                }
            }
            // (tab, state, paints) for every pane.
            let paints: Vec<(usize, PaneState, u64)> = cx.update(|cx| {
                let panes: Vec<(usize, PaneState, workspace::PaneHandle)> = handle
                    .read(cx)
                    .map(|wv| wv.panes_by_state())
                    .unwrap_or_default();
                panes
                    .into_iter()
                    .map(|(t, s, h)| (t, s, h.paints(cx)))
                    .collect()
            });
            let shown: u64 = paints
                .iter()
                .filter(|(t, s, _)| *t == 0 && s.is_shown())
                .map(|p| p.2)
                .sum();
            let hidden_in_tab: Vec<u64> = paints
                .iter()
                .filter(|(t, s, _)| *t == 0 && !s.is_shown())
                .map(|p| p.2)
                .collect();
            let background: u64 = paints.iter().filter(|(t, _, _)| *t > 0).map(|p| p.2).sum();
            report(
                &times,
                seconds,
                size,
                charts,
                doms,
                rss_start,
                resident_bytes(),
                Paints {
                    shown,
                    hidden_in_tab: &hidden_in_tab,
                    background,
                },
            );
            let wasted = background + hidden_in_tab.iter().sum::<u64>();
            std::process::exit(if wasted == 0 { 0 } else { 2 });
        })
        .detach();
    });
    Ok(1)
}

/// Charts 0-2 and DOM 0 on the front month, the rest on the next contract (the mock serves
/// every ES contract).
fn spread_over_two_contracts(doc: &mut WorkspaceDoc) {
    let Some(es) = catalog::get(catalog::ES) else {
        return;
    };
    let Some(front) = front_month(&es, None) else {
        return;
    };
    let today = lines_core::CivilDate::from_utc(now_utc());
    let Some(next) = es.contracts_from(today).find(|k| *k > front) else {
        return;
    };
    let (a, b) = (
        TargetDoc::from_target(&Target::Contract(front)),
        TargetDoc::from_target(&Target::Contract(next)),
    );
    let (mut charts, mut doms) = (0, 0);
    let tab = &mut doc.windows[0].tabs[0];
    tab.layout = tab.layout.map(&mut |p| {
        let mut p = p.clone();
        let n = match p.kind {
            PaneKind::Chart => {
                charts += 1;
                charts
            }
            PaneKind::Dom => {
                doms += 1;
                doms * 3
            }
            PaneKind::Tape | PaneKind::Journal | PaneKind::Strategies => 1,
        };
        p.target = Some(if n <= 3 { a.clone() } else { b.clone() });
        p
    });
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

/// Frames painted by the shown panes, by each hidden pane of the shown tab, and by the
/// background tab.
struct Paints<'a> {
    shown: u64,
    hidden_in_tab: &'a [u64],
    background: u64,
}

#[allow(clippy::too_many_arguments)]
fn report(
    times: &[Duration],
    seconds: u64,
    size: (f32, f32),
    charts: usize,
    doms: usize,
    rss_start: Option<u64>,
    rss_end: Option<u64>,
    paints: Paints,
) {
    let mut sorted = times.to_vec();
    sorted.sort();
    let mean = if sorted.is_empty() {
        0.0
    } else {
        sorted.iter().map(|d| d.as_secs_f64()).sum::<f64>() / sorted.len() as f64 * 1e3
    };
    let over_8 = sorted.iter().filter(|d| d.as_secs_f64() > 0.008).count();
    let over_16 = sorted.iter().filter(|d| d.as_secs_f64() > 0.016).count();
    let mb = |b: Option<u64>| {
        b.map_or_else(
            || "n/a".to_owned(),
            |b| format!("{:.1} MB", b as f64 / 1048576.0),
        )
    };
    println!(
        "benchmark: {charts} charts + {doms} DOMs{}, {}x{} logical px, {seconds} s after a {} s warm-up ({})",
        if paints.hidden_in_tab.is_empty() {
            ""
        } else {
            " (DOMs hidden)"
        },
        size.0,
        size.1,
        WARMUP.as_secs(),
        if cfg!(debug_assertions) {
            "DEBUG build: use --release for real numbers"
        } else {
            "release build"
        },
    );
    println!(
        "frame CPU time (Window::draw): n={}  mean {mean:.2} ms  p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2} ms  >8 ms: {over_8}  >16 ms: {over_16}",
        sorted.len(),
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99),
        percentile(&sorted, 1.0),
    );
    println!(
        "resident memory: {} at start (no UI, no feeds), {} at the end (workspace open, feeds running)",
        mb(rss_start),
        mb(rss_end)
    );
    if !paints.hidden_in_tab.is_empty() {
        let n = paints.hidden_in_tab.len();
        let sum: u64 = paints.hidden_in_tab.iter().sum();
        println!(
            "hidden panes in the shown tab: {n}, {sum} paints ({})",
            if sum == 0 {
                "ok: hidden panes do no paint work"
            } else {
                "FAIL: hidden panes painted"
            }
        );
    }
    println!(
        "pane paints: {} by shown panes, {} in the background tab ({})",
        paints.shown,
        paints.background,
        if paints.background == 0 {
            "ok: hidden tabs do no paint work"
        } else {
            "FAIL: hidden tab painted"
        }
    );
}

/// CPU time used by the whole process so far (user + kernel, all threads).
pub fn cpu_time() -> Option<Duration> {
    #[cfg(windows)]
    {
        win::cpu_time()
    }
    #[cfg(target_os = "linux")]
    {
        // utime and stime: fields 14 and 15 of /proc/self/stat, in clock ticks (100 Hz).
        let s = std::fs::read_to_string("/proc/self/stat").ok()?;
        let rest = &s[s.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
        Some(Duration::from_millis(ticks * 10))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// The process's resident memory in bytes, if the platform tells us.
pub fn resident_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        win::working_set()
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb: u64 = s
            .lines()
            .find(|l| l.starts_with("VmRSS:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // macOS and others: ask `ps` (resident set in KiB).
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kb * 1024)
    }
}

#[cfg(windows)]
mod win {
    /// `PROCESS_MEMORY_COUNTERS` (psapi.h).
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
        fn GetProcessTimes(
            process: isize,
            creation: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
    }

    /// Kernel + user time of every thread so far.
    pub fn cpu_time() -> Option<std::time::Duration> {
        let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
        // SAFETY: four writable FILETIMEs (u64, 100 ns units, same layout); the pseudo-handle
        // from GetCurrentProcess needs no closing.
        let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
        (ok != 0).then(|| std::time::Duration::from_nanos((k + u) * 100))
    }

    pub fn working_set() -> Option<u64> {
        let mut c = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            ..Default::default()
        };
        // SAFETY: `c` is a correctly sized, writable PROCESS_MEMORY_COUNTERS and `cb` says so;
        // the pseudo-handle from GetCurrentProcess needs no closing.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        (ok != 0).then_some(c.working_set_size as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cpu_time() {
        if cfg!(any(windows, target_os = "linux")) {
            assert!(cpu_time().is_some());
        }
    }

    #[test]
    fn reads_resident_memory() {
        let b = resident_bytes().expect("platform reports resident memory");
        assert!(b > 1024 * 1024, "{b}");
    }

    #[test]
    fn bench_spreads_over_two_contracts() {
        let mut doc = WorkspaceDoc::bench();
        spread_over_two_contracts(&mut doc);
        let leaves = doc.windows[0].tabs[0].layout.leaves();
        let targets: Vec<_> = leaves.iter().map(|p| p.target.clone().unwrap()).collect();
        let distinct: std::collections::HashSet<_> =
            targets.iter().map(|t| format!("{t:?}")).collect();
        assert_eq!(distinct.len(), 2);
    }
}
