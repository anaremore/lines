use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use lines_core::{Bar, ContractKey, Month, Price, Qty, UtcMillis};

use super::demo::{self, DemoLevels};
use super::runtime::{
    LevelCell, LevelMode, LevelStatus, NOT_RECORDED, legend_newly_dropped, mode_for,
    read_recording, recording_path,
};
use super::*;
use crate::catalog;

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

/// Chart prices from fixed bars and a fixed clock.
struct Fixed {
    bars: Vec<Bar>,
    now: Option<UtcMillis>,
}

/// A chart whose clock a test moves (a replay the bar jumps back in).
struct Moving {
    bars: Vec<Bar>,
    now: std::sync::Mutex<Option<UtcMillis>>,
}

impl ChartPrices for Moving {
    fn price_at(&self, at: UtcMillis, tol: i64) -> Option<Price> {
        super::price_at(&self.bars, at, tol)
    }
    fn now(&self) -> Option<UtcMillis> {
        *self.now.lock().unwrap()
    }
}

impl ChartPrices for Fixed {
    fn price_at(&self, at: UtcMillis, tol: i64) -> Option<Price> {
        // Through the public re-export adapters use.
        super::price_at(&self.bars, at, tol)
    }
    fn now(&self) -> Option<UtcMillis> {
        self.now
    }
}

fn es_ctx() -> LevelContext {
    let es = catalog::get(catalog::ES).unwrap();
    LevelContext {
        product: es,
        contract: Some(ContractKey::monthly(
            catalog::ES,
            2026,
            Month::new(12).unwrap(),
        )),
        settings: SettingValues::new(),
    }
}

fn bar_at(at: &str, open: i64) -> Bar {
    Bar {
        time: t(at),
        open: Price(open),
        high: Price(open),
        low: Price(open),
        close: Price(open),
        volume: Qty(1),
    }
}

/// Adapters can implement the trait under its `PriceAtTime` name.
#[allow(dead_code)]
fn takes_price_at_time(p: &dyn PriceAtTime) -> Option<Price> {
    p.price_at(UtcMillis(0), 0)
}

fn frames() -> Vec<LevelFrame> {
    vec![LevelFrame::new("demo/levels", demo::BODY)]
}

struct Bad;

impl LevelSource for Bad {
    fn id(&self) -> &'static str {
        "Bad Id"
    }
    fn label(&self) -> &'static str {
        "bad"
    }
    fn serves(&self, _: &lines_core::Product) -> bool {
        true
    }
    fn refresh(&self) -> RefreshMode {
        RefreshMode::OneShot
    }
    fn stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
    fn connect(&self, _: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>> {
        Box::pin(async { anyhow::bail!("none") })
    }
    fn decode(
        &self,
        _: &LevelContext,
        _: &dyn ChartPrices,
        _: Option<&LevelMap>,
        _: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        Ok(None)
    }
}

#[test]
fn registration_rejects_bad_ids_and_filters_by_product() {
    assert!(register(Arc::new(Bad)).is_err());
    register(Arc::new(DemoLevels)).unwrap();
    register(Arc::new(DemoLevels)).unwrap(); // same id again: replaced, not duplicated
    assert_eq!(
        registered().iter().filter(|s| s.id() == demo::ID).count(),
        1
    );
    let es = catalog::get(catalog::ES).unwrap();
    let nq = catalog::get(catalog::NQ).unwrap();
    assert!(serving(&es).iter().any(|s| s.id() == demo::ID));
    assert!(!serving(&nq).iter().any(|s| s.id() == demo::ID));
    assert!(by_id(demo::ID).is_some());
}

#[test]
fn batch_encoding_round_trips_and_rejects_garbage() {
    let f = vec![
        LevelFrame::new("a/b", "hello"),
        LevelFrame::new("", Vec::new()),
    ];
    let mut buf = Vec::new();
    encode_batch(&f, &mut buf);
    assert_eq!(decode_batch(&buf).unwrap(), f);
    assert!(decode_batch(&buf[..buf.len() - 1]).is_err());
    assert!(decode_batch(&[9, 0, 0, 0, b'x']).is_err());
    assert!(decode_batch(&[]).unwrap().is_empty());
}

#[test]
fn demo_decode_translates_the_anchor_only_where_the_chart_has_a_price() {
    let ctx = es_ctx();
    let covered = Fixed {
        bars: vec![bar_at("2026-11-05T14:30:00", 600_100)],
        now: None,
    };
    let m = DemoLevels
        .decode(&ctx, &covered, None, &frames())
        .unwrap()
        .unwrap();
    assert_eq!(m.source, "demo");
    assert_eq!(m.as_of, t("2026-11-05T13:00:00"));
    assert_eq!(
        m.kinds,
        [
            "pivot", "resist", "support", "supply", "zone", "band", "anchor"
        ]
    );
    let band = m.levels.iter().find(|l| l.is_vwap_relative()).unwrap();
    assert_eq!(band.vwap_offsets(), Some((-750.0, 750.0)));
    let anchor = m.levels.last().unwrap();
    assert_eq!(anchor.span_at(None).unwrap().0, 600_850.0); // 6001.00 + 7.50
    // No bar at the anchor time: the anchored level is withheld, the rest shown.
    let bare = Fixed {
        bars: vec![],
        now: None,
    };
    let m2 = DemoLevels
        .decode(&ctx, &bare, None, &frames())
        .unwrap()
        .unwrap();
    assert_eq!(m2.levels.len(), m.levels.len() - 1);
    assert!(m2.kind_index("anchor").is_none());
    // Malformed body: refused with a reason.
    let bad = vec![LevelFrame::new("demo/levels", "{")];
    assert!(DemoLevels.decode(&ctx, &bare, None, &bad).is_err());
    // Not our topic: nothing changed.
    let other = vec![LevelFrame::new("keepalive", "")];
    assert_eq!(DemoLevels.decode(&ctx, &bare, None, &other), Ok(None));
}

#[test]
fn status_is_missing_current_or_stale() {
    let mut s = runtime::LevelState {
        source: "demo",
        label: "Demo",
        map: None,
        received: None,
        error: None,
        series_received: None,
        levels_refused: false,
        stale_after: std::time::Duration::from_secs(60),
        replay: false,
    };
    let now = t("2026-11-05T15:00:00");
    assert_eq!(s.status(now), LevelStatus::Missing);
    s.error = Some("refused".into());
    assert_eq!(
        s.status(now),
        LevelStatus::Missing,
        "an error with no map is missing"
    );
    let bare = Fixed {
        bars: vec![],
        now: None,
    };
    let m = DemoLevels
        .decode(&es_ctx(), &bare, None, &frames())
        .unwrap()
        .unwrap();
    s.map = Some(Arc::new(m));
    s.error = None;
    s.received = Some(t("2026-11-05T14:59:30"));
    assert_eq!(s.status(now), LevelStatus::Current);
    s.received = Some(t("2026-11-05T14:58:00"));
    assert_eq!(s.status(now), LevelStatus::Stale { age_ms: 120_000 });
    s.received = Some(t("2026-11-05T14:59:59"));
    s.error = Some("timeout".into());
    assert_eq!(s.status(now), LevelStatus::Stale { age_ms: 1_000 });
}

/// A map whose levels were refused: the levels stay stale (their time not renewed, the reason
/// as the error), its series are current; another error makes both stale.
#[test]
fn refused_levels_leave_the_series_current() {
    let mut s = runtime::LevelState {
        source: "demo",
        label: "Demo",
        map: None,
        received: None,
        error: None,
        series_received: None,
        levels_refused: false,
        stale_after: std::time::Duration::from_secs(60),
        replay: false,
    };
    let bare = Fixed {
        bars: vec![],
        now: None,
    };
    let good = DemoLevels
        .decode(&es_ctx(), &bare, None, &frames())
        .unwrap();
    s.take(Ok(good.clone()), t("2026-11-05T14:00:00"));
    let mut refused = good.unwrap();
    refused.levels_refused = Some("capture is stale".into());
    let at = t("2026-11-05T14:30:00");
    s.take(Ok(Some(refused)), at);
    let now = t("2026-11-05T14:30:30");
    assert_eq!(
        s.status(now),
        LevelStatus::Stale { age_ms: 1_830_000 },
        "levels: stale since the last good map"
    );
    assert_eq!(s.error.as_deref(), Some("capture is stale"));
    assert_eq!(s.series_status(now), LevelStatus::Current);
    // Unchanged next refresh: the levels stay refused and stale, the series current.
    s.take(Ok(None), now);
    assert!(matches!(s.status(now), LevelStatus::Stale { .. }));
    assert_eq!(s.error.as_deref(), Some("capture is stale"));
    assert_eq!(s.series_status(now), LevelStatus::Current);
    // A series-only update (a live push): the series current, the levels' state untouched.
    let mut pushed = (**s.map.as_ref().unwrap()).clone();
    pushed.series_update = true;
    let later = t("2026-11-05T14:31:00");
    s.take(Ok(Some(pushed)), later);
    assert!(matches!(s.status(later), LevelStatus::Stale { .. }));
    assert_eq!(s.error.as_deref(), Some("capture is stale"));
    assert_eq!(s.series_status(later), LevelStatus::Current);
    // A push after a failed refresh: the lines move, but the series stay stale (a push never
    // hides a failing refresh).
    s.take(Err("timeout".into()), later);
    let mut pushed = (**s.map.as_ref().unwrap()).clone();
    pushed.series_update = true;
    s.take(Ok(Some(pushed)), later);
    assert!(matches!(s.series_status(later), LevelStatus::Stale { .. }));
    assert_eq!(s.error.as_deref(), Some("timeout"));
    // A failed refresh with the same words is still a failure: the series go stale too.
    s.take(Err("capture is stale".into()), now);
    assert!(matches!(s.series_status(now), LevelStatus::Stale { .. }));
    // Unchanged with no map yet: the reason shown stays.
    let mut waiting = runtime::LevelState {
        map: None,
        series_received: None,
        error: Some("capture is stale".into()),
        ..s.clone()
    };
    waiting.take(Ok(None), now);
    assert_eq!(waiting.error.as_deref(), Some("capture is stale"));
    // An empty state: missing for both.
    let fresh = runtime::LevelState {
        map: None,
        series_received: None,
        ..s.clone()
    };
    assert_eq!(fresh.series_status(now), LevelStatus::Missing);
}

/// Live with recording, then replay of the recording: the same map, applied at its recorded
/// time on the replay clock, and never before it.
/// Owner's bug: a 2025 NQ replay without `--replay-levels` showed a source's live map ("as
/// of 22:19Z", current) against the replay clock. A market replay never runs a source live.
#[test]
fn a_replay_never_fetches_live() {
    let d = std::path::PathBuf::from("levels");
    assert_eq!(mode_for(true, None, None), LevelMode::NotRecorded);
    assert_eq!(
        mode_for(true, None, Some(d.clone())),
        LevelMode::Replay(d.clone())
    );
    // Recording live levels next to a replay would record today's map: not recorded.
    assert_eq!(
        mode_for(true, Some(d.clone()), None),
        LevelMode::NotRecorded
    );
    assert_eq!(mode_for(false, None, None), LevelMode::Live);
    assert_eq!(mode_for(false, Some(d.clone()), None), LevelMode::Record(d));
}

/// A source with no recorded batches in a replay (no levels directory, or no file for it in
/// the directory): missing with the reason, no map, never fetched, settled at once.
#[test]
fn unrecorded_sources_are_missing_in_a_replay() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let empty = std::env::temp_dir().join(format!("lines-levels-none-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&empty);
    for dir in [None, Some(empty.clone())] {
        let src: Arc<dyn LevelSource> = Arc::new(DemoLevels);
        let prices: Arc<dyn ChartPrices> = Arc::new(Fixed {
            bars: vec![bar_at("2025-09-11T14:30:00", 600_100)],
            now: Some(t("2025-09-11T15:00:00")),
        });
        let cell = Arc::new(LevelCell::for_test(&*src, true));
        let (next, loaded) = (
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let (cell2, loaded2) = (cell.clone(), loaded.clone());
        rt.block_on(async move {
            // Returns by itself: nothing to wait for.
            runtime::replay(src, es_ctx(), prices, cell2, dir, next, loaded2).await;
        });
        let s = cell.read().clone();
        assert!(s.map.is_none(), "nothing drawn");
        assert_eq!(s.received, None);
        assert_eq!(s.error.as_deref(), Some(NOT_RECORDED));
        assert_eq!(s.status(t("2025-09-11T15:00:00")), LevelStatus::Missing);
        assert!(loaded.load(std::sync::atomic::Ordering::Acquire));
    }
    let _ = std::fs::remove_dir_all(&empty);
}

/// A real source that counts connects (any connect is a live fetch).
struct Counting(std::sync::atomic::AtomicUsize);

impl LevelSource for Counting {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn label(&self) -> &'static str {
        "Counting"
    }
    fn serves(&self, _: &lines_core::Product) -> bool {
        true
    }
    fn refresh(&self) -> RefreshMode {
        RefreshMode::OneShot
    }
    fn stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
    fn connect(&self, _: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { anyhow::bail!("live fetch in a replay") })
    }
    fn decode(
        &self,
        _: &LevelContext,
        _: &dyn ChartPrices,
        _: Option<&LevelMap>,
        _: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        Ok(None)
    }
}

/// End to end through `acquire` (the only test here that sets the process-wide mode): in a
/// market replay without recordings a real source never connects and is missing; the
/// synthetic demo still draws (screenshots with `--demo-levels`).
#[test]
fn acquire_in_a_replay_without_recordings() {
    runtime::set_mode(mode_for(true, None, None));
    let src = Arc::new(Counting(Default::default()));
    let prices: Arc<dyn ChartPrices> = Arc::new(Fixed {
        bars: vec![bar_at("2026-11-05T14:30:00", 600_100)],
        now: Some(t("2025-09-11T15:00:00")),
    });
    let real = runtime::acquire(src.clone(), es_ctx(), prices.clone());
    let demo = runtime::acquire(Arc::new(DemoLevels), es_ctx(), prices);
    assert!(runtime::wait_settled(std::time::Duration::from_secs(10)));
    std::thread::sleep(std::time::Duration::from_millis(100)); // a live task would connect
    runtime::set_mode(LevelMode::Live);
    assert_eq!(
        src.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "never fetched"
    );
    let s = real.cell().read().clone();
    assert!(s.map.is_none() && s.replay);
    assert_eq!(s.error.as_deref(), Some(NOT_RECORDED));
    assert_eq!(s.status(t("2025-09-11T15:00:00")), LevelStatus::Missing);
    let d = demo.cell().read().clone();
    assert!(
        d.map.is_some(),
        "synthetic sources still draw over a replay"
    );
}

/// The app's "Start recording" mid-session: a live source already running starts its file
/// with its newest batch, at the time that batch came (a one-shot source sends no other), and
/// stopping ends it; batches before the start and after the stop are not in it.
#[test]
fn a_recording_started_mid_session_begins_with_the_newest_batch() {
    let dir = std::env::temp_dir().join(format!("lines-levels-mid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ctx = es_ctx();
    let prices: Arc<dyn ChartPrices> = Arc::new(Fixed {
        bars: vec![bar_at("2026-11-05T14:30:00", 600_100)],
        now: None,
    });
    let src: Arc<dyn LevelSource> = Arc::new(DemoLevels);
    let cell = Arc::new(LevelCell::for_test(&*src, false));
    let rec = runtime::recorder_slot(&*src, &ctx, None, false);
    let last: runtime::Shared<(UtcMillis, Vec<LevelFrame>)> = Default::default();
    {
        let (src, ctx, prices, cell, rec, last) = (
            src.clone(),
            ctx.clone(),
            prices.clone(),
            cell.clone(),
            rec.clone(),
            last.clone(),
        );
        rt.block_on(async move {
            let task = tokio::spawn(runtime::live(src, ctx, prices, cell.clone(), rec, last));
            while cell.read().map.is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            task.abort();
        });
    }
    let path = recording_path(&dir, "demo", "ES");
    assert!(!path.exists(), "nothing recorded before the start");
    let received = cell.read().received;
    let header = runtime::header_for(&*src, &ctx);
    runtime::begin(&rec, &last, Some(&dir), "demo", &header);
    runtime::begin(&rec, &last, None, "demo", &header);
    let batches = read_recording(&path, "demo", "ES").unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].1, frames());
    assert_eq!(Some(batches[0].0), received, "at the time it came");
    assert!(rec.lock().is_none(), "stopped");
    // `--record-levels` from launch: the app's Start and Stop leave its recorder alone.
    let pinned = runtime::recorder_slot(&*src, &ctx, Some(dir.join("cli")), true);
    runtime::begin(&pinned, &last, None, "demo", &header);
    assert!(pinned.lock().is_some(), "a stop doesn't end it");
    runtime::begin(&pinned, &last, Some(&dir.join("app")), "demo", &header);
    assert!(
        !recording_path(&dir.join("app"), "demo", "ES").exists(),
        "nor a start move it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recorded_levels_replay_to_the_same_map() {
    let dir = std::env::temp_dir().join(format!("lines-levels-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ctx = es_ctx();
    let bars = || vec![bar_at("2026-11-05T14:30:00", 600_100)];
    let prices: Arc<dyn ChartPrices> = Arc::new(Fixed {
        bars: bars(),
        now: None,
    });
    let src: Arc<dyn LevelSource> = Arc::new(DemoLevels);
    let live_cell = Arc::new(LevelCell::for_test(&*src, false));
    {
        let (src, ctx, prices, cell, dir) = (
            src.clone(),
            ctx.clone(),
            prices.clone(),
            live_cell.clone(),
            dir.clone(),
        );
        rt.block_on(async move {
            let rec = runtime::recorder_slot(&*src, &ctx, Some(dir), true);
            let task = tokio::spawn(runtime::live(
                src,
                ctx,
                prices,
                cell.clone(),
                rec,
                Default::default(),
            ));
            while cell.read().map.is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            task.abort();
        });
    }
    let live = live_cell.read().clone();
    let path = recording_path(&dir, "demo", "ES");
    let batches = read_recording(&path, "demo", "ES").unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].1, frames());
    assert_eq!(Some(batches[0].0), live.received);
    // Wrong source or product: refused.
    assert!(read_recording(&path, "other", "ES").is_err());
    assert!(read_recording(&path, "demo", "NQ").is_err());

    let recorded = batches[0].0;
    for (clock, expect) in [(recorded.0 - 1, false), (recorded.0, true)] {
        let replay_prices: Arc<dyn ChartPrices> = Arc::new(Fixed {
            bars: bars(),
            now: Some(UtcMillis(clock)),
        });
        let cell = Arc::new(LevelCell::for_test(&*src, true));
        let (next, loaded) = (
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let (src2, ctx2, cell2, dir2) = (src.clone(), ctx.clone(), cell.clone(), dir.clone());
        rt.block_on(async move {
            let task = tokio::spawn(runtime::replay(
                src2,
                ctx2,
                replay_prices,
                cell2,
                Some(dir2),
                next,
                loaded,
            ));
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            task.abort();
        });
        let s = cell.read().clone();
        assert_eq!(s.map.is_some(), expect, "clock {clock}");
        if expect {
            assert_eq!(s.map, live.map, "replay shows exactly the live map");
            assert_eq!(s.received, Some(recorded));
            // Freshness follows the replay clock, not the wall clock.
            assert_eq!(s.status(recorded), LevelStatus::Current);
        }
    }

    // The replay bar jumps back before the batch: its map goes; forward again, it is back.
    let moving = Arc::new(Moving {
        bars: bars(),
        now: std::sync::Mutex::new(Some(recorded)),
    });
    let cell = Arc::new(LevelCell::for_test(&*src, true));
    let (src2, ctx2, cell2, dir2, m2) = (
        src.clone(),
        ctx.clone(),
        cell.clone(),
        dir.clone(),
        moving.clone(),
    );
    rt.block_on(async move {
        let task = tokio::spawn(runtime::replay(
            src2,
            ctx2,
            m2,
            cell2.clone(),
            Some(dir2),
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicBool::new(false)),
        ));
        let until = |want: bool| {
            let c = cell2.clone();
            async move {
                for _ in 0..200 {
                    if c.read().map.is_some() == want {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                false
            }
        };
        assert!(until(true).await, "applied at its time");
        *moving.now.lock().unwrap() = Some(UtcMillis(recorded.0 - 1));
        assert!(until(false).await, "gone after a jump back before it");
        assert_eq!(cell2.read().received, None);
        *moving.now.lock().unwrap() = Some(recorded);
        assert!(until(true).await, "applied again");
        task.abort();
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn serving_changed_bumps_the_generation() {
    let before = serving_generation();
    serving_changed();
    assert!(serving_generation() > before);
}

#[test]
fn only_live_modes_are_live() {
    assert!(LevelMode::Live.is_live());
    assert!(LevelMode::Record("x".into()).is_live());
    assert!(!LevelMode::Replay("x".into()).is_live());
    assert!(!LevelMode::NotRecorded.is_live());
    assert!(!mode_for(true, None, None).is_live());
    assert!(!mode_for(true, Some("r".into()), Some("x".into())).is_live());
    assert!(mode_for(false, Some("r".into()), None).is_live());
}

#[test]
fn replays_know_which_products_were_recorded() {
    let dir = std::env::temp_dir().join(format!("lines-levels-rec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(recording_path(&dir, "acme", "CL"), b"").unwrap();
    let replay = LevelMode::Replay(dir.clone());
    assert!(replay.recorded("acme", "CL"));
    assert!(!replay.recorded("acme", "ES"), "no file for ES");
    assert!(!replay.recorded("other", "CL"), "another source's file");
    // Only a levels replay reads files.
    assert!(!LevelMode::Record(dir.clone()).recorded("acme", "CL"));
    assert!(!LevelMode::NotRecorded.recorded("acme", "CL"));
    assert!(!LevelMode::Live.recorded("acme", "CL"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn registering_announces_a_serving_change() {
    let before = serving_generation();
    register(Arc::new(DemoLevels)).unwrap();
    assert!(serving_generation() > before);
}

/// A source that keeps sending a bad legend is warned about once, when its legend is first
/// dropped (and again only after a good one), not on every refresh.
#[test]
fn a_dropped_legend_warns_once_per_change() {
    let ctx = es_ctx();
    let none = Fixed {
        bars: Vec::new(),
        now: None,
    };
    let good = DemoLevels
        .decode(&ctx, &none, None, &frames())
        .unwrap()
        .unwrap();
    let mut bad = good.clone();
    bad.legend = None;
    bad.legend_dropped = true;
    assert!(legend_newly_dropped(None, &bad));
    assert!(legend_newly_dropped(Some(&good), &bad));
    assert!(
        !legend_newly_dropped(Some(&bad), &bad),
        "the same again: quiet"
    );
    assert!(!legend_newly_dropped(Some(&bad), &good));
    assert!(!legend_newly_dropped(None, &good));
}

/// A live source whose map is one line at its `n` setting (ticks) and whose `url` setting is
/// part of its connection: it counts its connections.
struct SettingsProbe(Arc<std::sync::atomic::AtomicUsize>);

static PROBE_SETTINGS: [SettingSpec; 2] = [
    SettingSpec::new(
        "n",
        "N",
        SettingKind::Number {
            min: 0,
            max: 1000,
            step: 1,
            decimals: 0,
            unit: "",
            optional: false,
        },
        "4",
        "",
    ),
    SettingSpec::new("url", "URL", SettingKind::Url, "http://127.0.0.1:1", "").connection(),
];

struct ProbeTransport;

impl LevelTransport for ProbeTransport {
    fn fetch(&mut self) -> BoxFuture<'_, Result<Vec<LevelFrame>>> {
        Box::pin(async { Ok(vec![LevelFrame::new("probe", "{}")]) })
    }
}

impl LevelSource for SettingsProbe {
    fn id(&self) -> &'static str {
        "settings-probe"
    }
    fn label(&self) -> &'static str {
        "Settings probe"
    }
    fn serves(&self, _: &lines_core::Product) -> bool {
        true
    }
    fn refresh(&self) -> RefreshMode {
        RefreshMode::Poll(std::time::Duration::from_millis(20))
    }
    /// Runs live whatever another test set the process's mode to.
    fn synthetic(&self) -> bool {
        true
    }
    fn stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
    fn settings(&self) -> &'static [SettingSpec] {
        &PROBE_SETTINGS
    }
    fn connect(&self, _: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(Box::new(ProbeTransport) as Box<dyn LevelTransport>) })
    }
    fn decode(
        &self,
        ctx: &LevelContext,
        _: &dyn ChartPrices,
        _: Option<&LevelMap>,
        _: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        let n: i64 = ctx.setting(&PROBE_SETTINGS[0]).parse().map_err(|_| "n")?;
        let tick = ctx.product.price.tick;
        LevelMap::builder(self.id(), ctx.product.id, t("2026-11-05T14:30:00"))
            .line(
                "p",
                LevelValue::Tick(Price(n * tick)),
                Some("P"),
                LevelStyle {
                    rgba: 0xffffffff,
                    weight: 1,
                    dashed: false,
                },
            )
            .build(tick)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

/// Settings apply live: a change that isn't part of the connection decodes the newest batch
/// again (no new connection); a connection setting connects again; the same values again do
/// nothing (the generation moves only on a change).
#[test]
fn settings_apply_live() {
    let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let src = Arc::new(SettingsProbe(connects.clone()));
    let prices: Arc<dyn ChartPrices> = Arc::new(Fixed {
        bars: Vec::new(),
        now: None,
    });
    let h = runtime::acquire(src, es_ctx(), prices);
    let line = |h: &runtime::LevelHandle| {
        h.cell()
            .read()
            .map
            .as_ref()
            .and_then(|m| match m.levels.first()?.shape {
                lines_core::levels::LevelShape::Line(LevelValue::Tick(p)) => Some(p.0),
                _ => None,
            })
    };
    let wait = |what: &str, ok: &dyn Fn() -> bool| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ok() {
            assert!(std::time::Instant::now() < end, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    wait("first map", &|| line(&h) == Some(4 * 25));
    let c0 = connects.load(std::sync::atomic::Ordering::SeqCst);
    let g0 = runtime::settings_generation("settings-probe");
    let mut v = SettingValues::new();
    v.insert("n".into(), "8".into());
    runtime::set_settings("settings-probe", v.clone());
    assert_eq!(runtime::settings_generation("settings-probe"), g0 + 1);
    runtime::set_settings("settings-probe", v.clone());
    assert_eq!(
        runtime::settings_generation("settings-probe"),
        g0 + 1,
        "unchanged"
    );
    wait("decoded again", &|| line(&h) == Some(8 * 25));
    assert_eq!(
        connects.load(std::sync::atomic::Ordering::SeqCst),
        c0,
        "no new connection for a display setting"
    );
    v.insert("url".into(), "http://127.0.0.1:2".into());
    runtime::set_settings("settings-probe", v);
    wait("connected again", &|| {
        connects.load(std::sync::atomic::Ordering::SeqCst) > c0
    });
    assert_eq!(line(&h), Some(8 * 25), "the map stays shown");
    runtime::set_settings("settings-probe", SettingValues::new());
}
