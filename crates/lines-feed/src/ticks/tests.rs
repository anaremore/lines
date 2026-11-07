use std::path::{Path, PathBuf};
use std::time::Duration;

use lines_core::{Bar, Capabilities, ContractKey, Month, Price, Qty};

use super::*;
use crate::catalog;
use crate::disk::coverage::Coverage;
use crate::disk::segment::{FlushPolicy, SegmentHeader, SegmentWriter};
use crate::native::{self, Aggressor, NativeEvent};
use crate::provider::{Frame, MarketEvent};
use crate::recording::{Direction, FORMAT_VERSION, Header, Writer};
use crate::store::TAPE_PRINTS;

const KEY: TickKey = TickKey {
    n: 233,
    reset: TickReset::Session,
};

fn es() -> (Product, ContractKey) {
    let p = catalog::get(catalog::ES).unwrap();
    (
        p,
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap()),
    )
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-ticks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Thu 2026-11-05 20:00 UTC (14:00 CST), in the session that closes at 22:00 UTC.
fn start() -> UtcMillis {
    UtcMillis::parse_iso("2026-11-05T20:00:00Z").unwrap()
}

/// 30 000 trades, three per frame, every 600 ms with every fifth sharing the previous
/// millisecond: four hours, over the 22:00 UTC session close into the next session (a reset).
fn frames() -> Vec<(i64, Vec<NativeEvent>)> {
    frames_with(|_, t| t)
}

/// [`frames`] with each trade's time changed by `time(i, t)` (frames keep their own time).
fn frames_with(time: impl Fn(i64, i64) -> i64) -> Vec<(i64, Vec<NativeEvent>)> {
    let mut out: Vec<(i64, Vec<NativeEvent>)> = Vec::new();
    let mut t = start().0;
    for i in 0..30_000i64 {
        if i % 5 != 0 {
            t += 600 + i % 7;
        }
        let e = NativeEvent::Trade {
            time_us: time(i, t) * 1000,
            price: Price(600_000 + (i * 13 % 11) * 25),
            qty: Qty(1 + i % 4),
            aggressor: if i % 3 == 0 {
                Aggressor::Sell
            } else {
                Aggressor::Buy
            },
        };
        match out.last_mut() {
            Some((_, v)) if v.len() < 3 => v.push(e),
            _ => out.push((t, vec![e])),
        }
    }
    out
}

fn encode(events: &[NativeEvent]) -> Vec<u8> {
    let mut buf = Vec::new();
    for e in events {
        native::encode(e, &mut buf);
    }
    buf
}

/// The frames as a native `.lfr` recording.
fn recording(dir: &Path, frames: &[(i64, Vec<NativeEvent>)]) -> PathBuf {
    let (_, k) = es();
    let path = dir.join("r.lfr");
    let header = Header {
        start_wall_ms: start().0,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: "ES".into(),
        contract: Some(k),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    for (t, events) in frames {
        let at = Duration::from_millis((t - start().0) as u64);
        w.write(Direction::FromServer, at, &Frame::binary(encode(events)))
            .unwrap();
    }
    w.finish().unwrap();
    path
}

/// The local store's segment for the session of [`start`], written through the cut (and past
/// it, as a live writer does), with trade coverage set by `cover(coverage, open, close)`.
fn stored_session(
    dir: &Path,
    frames: &[(i64, Vec<NativeEvent>)],
    cover: impl FnOnce(&mut Coverage, i64, i64),
) -> ContractDir {
    let (product, k) = es();
    let cdir = ContractDir::new(dir, "mock", &product, Some(k));
    let date = crate::disk::partition::trade_date(&product, start());
    let header = SegmentHeader {
        created_wall_ms: start().0,
        source: "mock".into(),
        encoding: native::PROTOCOL.into(),
        product: "ES".into(),
        contract: Some(k),
        date,
    };
    let (mut w, _) =
        SegmentWriter::open(&cdir.segment_path(date), &header, FlushPolicy::default()).unwrap();
    // Only this session's frames (the next session goes to the next date's segment).
    let (open, close) = crate::disk::partition::trade_date_span(&product, date);
    for (t, e) in frames.iter().filter(|(t, _)| *t < close.0) {
        w.append(t * 1000, &encode(e)).unwrap();
    }
    w.close().unwrap();
    let mut cov = Coverage::default();
    cover(&mut cov, open.0, close.0);
    cov.save(&cdir.coverage_path()).unwrap();
    cdir
}

fn store() -> Arc<ChartStore> {
    let (p, k) = es();
    Arc::new(ChartStore::new(
        p,
        Some(k),
        Capabilities::default(),
        "lines",
        60_000,
        50_000,
    ))
}

fn events(e: &[NativeEvent]) -> Vec<MarketEvent> {
    e.iter()
        .map(|e| match *e {
            NativeEvent::Trade {
                time_us,
                price,
                qty,
                aggressor,
            } => MarketEvent::Trade {
                time: UtcMillis(time_us / 1000),
                price,
                qty,
                aggressor: match aggressor {
                    Aggressor::Buy => Some(lines_core::Aggressor::Buy),
                    Aggressor::Sell => Some(lines_core::Aggressor::Sell),
                    Aggressor::Unknown => None,
                },
            },
            _ => unreachable!("trades only"),
        })
        .collect()
}

/// Bars built live from the first trade: the reference. The feed starts mid-session, so
/// they say from when they begin.
fn live_built(frames: &[(i64, Vec<NativeEvent>)]) -> TickBars {
    let s = store();
    let lease = s.acquire_ticks(KEY);
    for (_, e) in frames {
        s.update(|d| d.apply(&events(e)));
    }
    let d = s.read();
    let bars = d.ticks.bars(lease.key()).unwrap().clone();
    assert_eq!(
        d.ticks.status(KEY, &d.series, &d.product),
        TickStatus::From(bars.bars()[0].time)
    );
    bars
}

/// A chart switches to 233t after `cut` frames: the rebuild from `source`, the tape and the
/// trades counted while it runs give exactly the live-built bars (none counted twice).
fn rebuilt_mid_feed(
    frames: &[(i64, Vec<NativeEvent>)],
    source: TickSource,
    cut: usize,
) -> (TickBars, TickStatus, u64) {
    let s = store();
    s.update(|d| d.tick_source = Some(source));
    for (_, e) in &frames[..cut] {
        s.update(|d| d.apply(&events(e)));
    }
    assert!(
        s.read().counted_n > TAPE_PRINTS as u64,
        "the tape lost trades: a real rebuild"
    );
    let mut lease = s.acquire_ticks(KEY);
    // Counted while the rebuild runs (held and applied when it lands, or applied after).
    for (_, e) in &frames[cut..cut + 50] {
        s.update(|d| d.apply(&events(e)));
    }
    lease.join_rebuild();
    for (_, e) in &frames[cut + 50..] {
        s.update(|d| d.apply(&events(e)));
    }
    let d = s.read();
    let bars = d.ticks.bars(KEY).unwrap().clone();
    (
        bars,
        d.ticks.status(KEY, &d.series, &d.product),
        d.counted_n,
    )
}

#[test]
fn rebuilt_from_a_replay_equals_live_built() {
    let dir = scratch("replay");
    let frames = frames();
    let path = recording(&dir, &frames);
    let live = live_built(&frames);
    assert!(live.len() > 100 && (0..live.len()).any(|i| live.is_partial(i)));
    let (rebuilt, status, counted) = rebuilt_mid_feed(&frames, TickSource::Replay(path), 6_000);
    assert!(rebuilt.same_bars(&live), "rebuilt from the replay");
    assert_eq!(rebuilt.trades(), counted, "every counted trade once");
    // The recording starts mid-session: labelled from its first trade, as live-built.
    assert_eq!(status, TickStatus::From(live.bars()[0].time));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rebuilt_from_the_local_store_equals_live_built() {
    let dir = scratch("stored");
    let frames = frames();
    // The session's trades covered from the open.
    let cdir = stored_session(&dir, &frames, |cov, open, close| {
        cov.trades.add(open, close - 1)
    });

    let live = live_built(&frames);
    let (rebuilt, status, counted) =
        rebuilt_mid_feed(&frames, TickSource::Stored(cdir.clone()), 4_000);
    assert!(rebuilt.same_bars(&live), "rebuilt from the local store");
    assert_eq!(rebuilt.trades(), counted, "every counted trade once");
    assert_eq!(status, TickStatus::From(live.bars()[0].time));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The live part is read from the segments regardless of trade coverage (a claim not saved
/// yet, a hole): the writer stores exactly the trades live counted, so they are the same set.
#[test]
fn the_live_part_ignores_trade_coverage() {
    let dir = scratch("hole");
    let frames = frames();
    let gap = (start().0 + 5 * 60_000, start().0 + 10 * 60_000);
    let cdir = stored_session(&dir, &frames, |cov, open, close| {
        cov.trades.add(open, gap.0 - 1);
        cov.trades.add(gap.1, close - 1);
    });
    let live = live_built(&frames);
    let (bars, _, counted) = rebuilt_mid_feed(&frames, TickSource::Stored(cdir.clone()), 4_000);
    assert!(bars.same_bars(&live));
    assert_eq!(bars.trades(), counted);
    std::fs::remove_file(cdir.coverage_path()).unwrap();
    let (bars, _, _) = rebuilt_mid_feed(&frames, TickSource::Stored(cdir), 4_000);
    assert!(bars.same_bars(&live), "no coverage at all");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stored trades from before this process that the warm start didn't replay (lookback off,
/// outside the coverage) were never counted: a rebuild that would read them falls back to the
/// tape, labelled, instead of counting more than the tape and VAP did.
#[test]
fn stored_trades_this_process_never_counted_stay_out() {
    let dir = scratch("earlier");
    let (product, k) = es();
    let cdir = ContractDir::new(&dir, "mock", &product, Some(k));
    // An earlier launch stored an hour before this one's first trade.
    let earlier = frames_with(|_, t| t - 2 * 3_600_000);
    store_frames(&cdir, &earlier[..1_000]);
    let frames = frames();
    store_frames(&cdir, &frames);
    let live = live_built(&frames);
    let (bars, status, _) = rebuilt_mid_feed(&frames, TickSource::Stored(cdir), 4_000);
    let TickStatus::From(t) = status else {
        panic!("labelled: {status:?}");
    };
    assert_eq!(bars.bars()[0].time, t);
    assert!(t > live.bars()[0].time, "from the tape");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Append `frames` to the store's segments as the writer does: one block per frame, under the
/// trade date of its newest trade.
fn store_frames(cdir: &ContractDir, frames: &[(i64, Vec<NativeEvent>)]) {
    let (product, k) = es();
    let mut open: Option<(CivilDate, SegmentWriter)> = None;
    for (_, events) in frames {
        let newest = events
            .iter()
            .map(|e| match e {
                NativeEvent::Trade { time_us, .. } => *time_us,
                _ => unreachable!("trades only"),
            })
            .max()
            .unwrap();
        let date = crate::disk::partition::trade_date(&product, UtcMillis(newest / 1000));
        if open.as_ref().is_none_or(|(d, _)| *d != date) {
            if let Some((_, w)) = open.take() {
                w.close().unwrap();
            }
            let header = SegmentHeader {
                created_wall_ms: start().0,
                source: "mock".into(),
                encoding: native::PROTOCOL.into(),
                product: "ES".into(),
                contract: Some(k),
                date,
            };
            let (w, _) =
                SegmentWriter::open(&cdir.segment_path(date), &header, FlushPolicy::default())
                    .unwrap();
            open = Some((date, w));
        }
        let (_, w) = open.as_mut().unwrap();
        w.append(newest, &encode(events)).unwrap();
    }
    if let Some((_, w)) = open {
        w.close().unwrap();
    }
}

/// [`live_built`] for any key, without its label check.
fn built_live(frames: &[(i64, Vec<NativeEvent>)], key: TickKey) -> TickBars {
    let s = store();
    let _lease = s.acquire_ticks(key);
    for (_, e) in frames {
        s.update(|d| d.apply(&events(e)));
    }
    s.read().ticks.bars(key).unwrap().clone()
}

/// The feed's warm start (`lib.rs::warm_start`): the stored trades replayed into the store in
/// slices, `during(slice)` after each, then the loading ends (its rebuilds returned).
fn warm_start(
    s: &Arc<ChartStore>,
    cdir: &ContractDir,
    mut during: impl FnMut(usize),
) -> Vec<JoinHandle<()>> {
    s.update(|d| {
        d.orderflow_loading = true;
        d.stored.dir = Some(cdir.clone());
    });
    let mut slice = 0;
    crate::disk::orderflow::read(cdir, i64::MIN, |chunk| {
        for part in chunk.trades.chunks(5_000) {
            s.update(|d| d.apply_stored(part, &[]));
            during(slice);
            slice += 1;
        }
        s.update(|d| d.apply_stored(&[], &chunk.covered));
    })
    .unwrap();
    s.end_orderflow_loading()
}

const AT_START: TickKey = TickKey {
    n: 233,
    reset: TickReset::Session,
};
const DURING: TickKey = TickKey {
    n: 144,
    reset: TickReset::Session,
};
const AFTER: TickKey = TickKey {
    n: 89,
    reset: TickReset::Continuous,
};
const LATER: TickKey = TickKey {
    n: 55,
    reset: TickReset::Session,
};

/// Restart with stored order flow: an earlier launch stored the first 12 003 trades; this one
/// replays them at startup, then counts the rest live (and stores them, as its writer does).
/// Tick bars asked for at startup, during the warm start, after it and well into the live
/// feed (read again from the store's record and the live segments) all equal bars built from
/// every trade in one run: the same set as the tape and VAP, none twice.
#[test]
fn tick_bars_and_the_stored_order_flow_agree_across_a_restart() {
    let dir = scratch("restart");
    let (product, k) = es();
    let cdir = ContractDir::new(&dir, "mock", &product, Some(k));
    let frames = frames();
    // Trade 12 003 isn't one that shares the previous millisecond: every live trade is after
    // the newest stored one (`stored_through`).
    let split = 4_001;
    store_frames(&cdir, &frames[..split]);
    let last_stored = frames[split].0 - 1;
    let date = crate::disk::partition::trade_date(&product, start());
    let (open, _) = crate::disk::partition::trade_date_span(&product, date);
    let mut cov = Coverage::default();
    cov.trades.add(open.0, last_stored);
    cov.save(&cdir.coverage_path()).unwrap();

    let s = store();
    s.update(|d| d.tick_source = Some(TickSource::Stored(cdir.clone())));
    let at_start = s.acquire_ticks(AT_START);
    let mut during = None;
    let handles = warm_start(&s, &cdir, |slice| {
        if slice == 1 {
            let lease = s.acquire_ticks(DURING);
            let d = s.read();
            assert_eq!(
                d.ticks.status(DURING, &d.series, &d.product),
                TickStatus::Building,
                "deferred while the warm start runs"
            );
            during = Some(lease);
        }
    });
    for h in handles {
        h.join().unwrap();
    }
    {
        let d = s.read();
        assert_eq!(d.counted_n, 12_003);
        assert!(
            d.stored_through.is_some_and(|t| t <= last_stored),
            "the newest stored trade"
        );
        assert_eq!(d.ticks.bars(AT_START).unwrap().trades(), 12_003);
    }
    // Before any live trade: the tape holds the newest stored ones, the rest come from the store.
    let mut after = s.acquire_ticks(AFTER);
    after.join_rebuild();
    // Live: counted and stored (frame by frame, as the writer does after each).
    let (live, rest) = frames[split..].split_at(4_000);
    for f in live {
        s.update(|d| d.apply(&events(&f.1)));
        assert!(
            s.read().frame_counted.iter().all(|c| *c),
            "after the newest stored trade"
        );
        store_frames(&cdir, std::slice::from_ref(f));
    }
    // The tape holds only live trades now: stored + live segments before it.
    let mut later = s.acquire_ticks(LATER);
    for f in &rest[..50] {
        s.update(|d| d.apply(&events(&f.1)));
    }
    later.join_rebuild();
    for f in &rest[50..] {
        s.update(|d| d.apply(&events(&f.1)));
    }
    let d = s.read();
    assert_eq!(d.counted_n, 30_000);
    for key in [AT_START, DURING, AFTER, LATER] {
        let bars = d.ticks.bars(key).unwrap();
        assert!(bars.same_bars(&built_live(&frames, key)), "{key:?}");
        assert_eq!(bars.trades(), 30_000, "{key:?}: every counted trade once");
    }
    drop((at_start, during, after, later));
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A trade re-delivered at the `stored_through` boundary (a snapshot older than the store) is
/// counted by nothing: not the tape, not the tick bars, not a rebuild from the replay's file.
#[test]
fn no_double_count_at_the_stored_boundary() {
    let dir = scratch("boundary");
    let (product, k) = es();
    let cdir = ContractDir::new(&dir, "mock", &product, Some(k));
    let frames = frames();
    let split = 4_001;
    store_frames(&cdir, &frames[..split]);
    let mut cov = Coverage::default();
    cov.trades.add(start().0 - 3_600_000, frames[split].0 - 1);
    cov.save(&cdir.coverage_path()).unwrap();
    // This launch's recording re-sends the last 30 stored frames before the new ones.
    let resent: Vec<_> = frames[split - 30..].to_vec();
    let path = recording(&dir, &resent);

    let s = store();
    s.update(|d| d.tick_source = Some(TickSource::Replay(path)));
    let at_start = s.acquire_ticks(AT_START);
    for h in warm_start(&s, &cdir, |_| {}) {
        h.join().unwrap();
    }
    for (_, e) in &resent[..3_030] {
        s.update(|d| d.apply(&events(e)));
    }
    let mut later = s.acquire_ticks(LATER);
    later.join_rebuild();
    for (_, e) in &resent[3_030..] {
        s.update(|d| d.apply(&events(e)));
    }
    let d = s.read();
    assert_eq!(d.counted_n, 30_000, "the re-sent trades once");
    let total: i64 = frames
        .iter()
        .flat_map(|(_, e)| events(e))
        .map(|e| match e {
            MarketEvent::Trade { qty, .. } => qty.0,
            _ => 0,
        })
        .sum();
    assert_eq!(d.vap.total(), Qty(total));
    for key in [AT_START, LATER] {
        let bars = d.ticks.bars(key).unwrap();
        assert!(bars.same_bars(&built_live(&frames, key)), "{key:?}");
    }
    drop((at_start, later));
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stored day evicted after the warm start: its trades can't be read again, so a rebuild
/// falls back to the tape, labelled.
#[test]
fn an_evicted_stored_day_falls_back_to_the_labelled_tape() {
    let dir = scratch("evicted");
    let (product, k) = es();
    let cdir = ContractDir::new(&dir, "mock", &product, Some(k));
    let frames = frames();
    store_frames(&cdir, &frames[..4_001]);
    let mut cov = Coverage::default();
    cov.trades.add(start().0 - 3_600_000, frames[4_001].0 - 1);
    cov.save(&cdir.coverage_path()).unwrap();
    let s = store();
    for h in warm_start(&s, &cdir, |_| {}) {
        h.join().unwrap();
    }
    for date in cdir.dates() {
        std::fs::remove_file(cdir.segment_path(date)).unwrap();
    }
    let mut lease = s.acquire_ticks(KEY);
    lease.join_rebuild();
    let d = s.read();
    let TickStatus::From(t) = d.ticks.status(KEY, &d.series, &d.product) else {
        panic!("labelled");
    };
    assert_eq!(d.ticks.bars(KEY).unwrap().bars()[0].time, t);
    assert_eq!(d.ticks.bars(KEY).unwrap().trades(), TAPE_PRINTS as u64);
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Trades whose times go backwards (late prints) around the cut: the split is by position, so
/// none is lost or counted twice, from either source.
#[test]
fn out_of_order_trades_are_split_by_position() {
    let dir = scratch("unordered");
    // Every 7th trade is stamped 3 s before its neighbors.
    let frames = frames_with(|i, t| if i % 7 == 3 { t - 3_000 } else { t });
    let path = recording(&dir, &frames);
    let live = live_built(&frames);
    for cut in [5_999, 6_000, 6_001] {
        let (rebuilt, _, counted) =
            rebuilt_mid_feed(&frames, TickSource::Replay(path.clone()), cut);
        assert!(rebuilt.same_bars(&live), "replay, cut {cut}");
        assert_eq!(rebuilt.trades(), counted);
    }
    let cdir = stored_session(&dir, &frames, |cov, open, close| {
        cov.trades.add(open, close - 1)
    });
    for cut in [3_999, 4_000, 4_001] {
        let (rebuilt, _, counted) =
            rebuilt_mid_feed(&frames, TickSource::Stored(cdir.clone()), cut);
        assert!(rebuilt.same_bars(&live), "stored, cut {cut}");
        assert_eq!(rebuilt.trades(), counted);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A builder no chart holds stops taking trades at once; a rebuild nobody wants any more
/// lands nothing.
#[test]
fn released_builders_and_rebuilds_stop() {
    let frames = frames();
    let s = store();
    let lease = s.acquire_ticks(KEY);
    s.update(|d| d.apply(&events(&frames[0].1)));
    assert_eq!(s.read().ticks.slots.len(), 1);
    drop(lease);
    s.update(|d| d.apply(&events(&frames[1].1)));
    assert!(s.read().ticks.slots.is_empty(), "dropped on the next trade");

    // Wanted only while held, not replaced and still rebuilding.
    {
        let mut d = s.read();
        let alive = Arc::new(());
        d.ticks.slots.push(Slot::new(
            KEY,
            Arc::downgrade(&alive),
            7,
            TickBars::new(KEY.n, KEY.reset, None, 16),
            Some(Vec::new()),
            None,
        ));
        assert!(d.ticks.wanted(KEY, 7));
        assert!(!d.ticks.wanted(KEY, 8), "replaced");
        drop(alive);
        assert!(!d.ticks.wanted(KEY, 7), "released");
        d.ticks.slots.clear();
    }

    // A cancelled read gives nothing to land.
    let dir = scratch("cancel");
    let path = recording(&dir, &frames);
    let (product, k) = es();
    let job = Job {
        key: KEY,
        generation: 1,
        stored: None,
        live: Some(TickSource::Replay(path)),
        product,
        contract: Some(k),
        capabilities: Capabilities::default(),
        provider: "lines",
        period_ms: 60_000,
        max_bars: 50_000,
        tape: Vec::new(),
        before_tape: 20_000,
    };
    assert!(rebuild(&job, || true).is_none());
    assert!(rebuild(&job, || false).is_some_and(|b| b.trades() == 20_000));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A replay read again that counts a different set before the tape (a provider session whose
/// start depends on the replay's clock), with the same total: refused, never counted twice.
/// The rebuild must find the tape's first prints right after exactly the trades live counted.
#[test]
fn a_replay_that_counts_a_different_set_is_refused() {
    let dir = scratch("replay-shift");
    let frames = frames();
    let path = recording(&dir, &frames);
    let s = store();
    for (_, e) in &frames[..6_000] {
        s.update(|d| d.apply(&events(e)));
    }
    let d = s.read();
    let tape = tape_prints(&d);
    let before = d.counted_n - tape.len() as u64;
    let (product, k) = es();
    let job = |before_tape| Job {
        key: KEY,
        generation: 1,
        stored: None,
        live: Some(TickSource::Replay(path.clone())),
        product: product.clone(),
        contract: Some(k),
        capabilities: Capabilities::default(),
        provider: "lines",
        period_ms: 60_000,
        max_bars: 50_000,
        tape: tape.clone(),
        before_tape,
    };
    // The right count: read again, every counted trade once.
    let ok = rebuild(&job(before), || false).unwrap();
    assert_eq!(ok.trades(), d.counted_n);
    // One trade more or fewer than the replay gives before the tape: only the tape, labelled.
    for wrong in [before - 1, before + 1] {
        let b = rebuild(&job(wrong), || false).unwrap();
        assert_eq!(b.trades(), tape.len() as u64, "refused at {wrong}");
    }
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// History shows trading before the first counted trade: the chart says from when it has
/// tick bars ("tick bars from 10:15"); once the bars reach back to the session's start, nothing.
#[test]
fn bars_starting_late_are_labelled() {
    let (product, _) = es();
    let s = store();
    let t = |iso: &str| UtcMillis::parse_iso(iso).unwrap();
    // 1-minute history from 15:30 UTC (the session opened the evening before); live trades
    // from 16:15:20.
    let bars: Vec<Bar> = (0..45)
        .map(|i| Bar {
            time: UtcMillis(t("2026-11-05T15:30:00Z").0 + i * 60_000),
            open: Price(600_000),
            high: Price(600_025),
            low: Price(600_000),
            close: Price(600_025),
            volume: Qty(10),
        })
        .collect();
    s.update(|d| {
        d.apply(&[MarketEvent::History {
            bars,
            as_of: Some(t("2026-11-05T16:14:59.999Z")),
        }])
    });
    let lease = s.acquire_ticks(KEY);
    {
        let d = s.read();
        // No tick bars yet: from when the chart asked (the feed's time: the newest bar).
        assert_eq!(
            d.ticks.status(lease.key(), &d.series, &product),
            TickStatus::From(t("2026-11-05T16:14:00Z"))
        );
    }
    for i in 0..10 {
        let e = MarketEvent::Trade {
            time: UtcMillis(t("2026-11-05T16:15:20Z").0 + i * 1000),
            price: Price(600_000),
            qty: Qty(1),
            aggressor: None,
        };
        s.update(|d| d.apply(&[e]));
    }
    let d = s.read();
    let status = d.ticks.status(lease.key(), &d.series, &product);
    assert_eq!(status, TickStatus::From(t("2026-11-05T16:15:20Z")));
    assert_eq!(
        status.label().as_deref(),
        Some("tick bars from 11-05 16:15 UTC")
    );
    assert_eq!(
        TickStatus::Building.label().as_deref(),
        Some("building tick bars")
    );
    assert_eq!(TickStatus::Complete.label(), None);
}

/// Bars from the session open: nothing to say. Continuous bars from the same open say from
/// when they begin when 1-minute bars show an earlier session.
#[test]
fn bars_from_the_open_are_complete_unless_continuous_misses_earlier_sessions() {
    let (product, _) = es();
    let t = |iso: &str| UtcMillis::parse_iso(iso).unwrap();
    // The session opening Wed 2026-11-04 23:00 UTC; 1-minute history of the one before.
    let history: Vec<Bar> = (0..30)
        .map(|i| Bar {
            time: UtcMillis(t("2026-11-04T21:00:00Z").0 + i * 60_000),
            open: Price(600_000),
            high: Price(600_025),
            low: Price(600_000),
            close: Price(600_025),
            volume: Qty(10),
        })
        .collect();
    for (reset, want) in [
        (TickReset::Session, TickStatus::Complete),
        (
            TickReset::Continuous,
            TickStatus::From(t("2026-11-04T23:00:00.200Z")),
        ),
    ] {
        let s = store();
        s.update(|d| {
            d.apply(&[MarketEvent::History {
                bars: history.clone(),
                as_of: Some(t("2026-11-04T21:29:59.999Z")),
            }])
        });
        let key = TickKey { n: 233, reset };
        let _lease = s.acquire_ticks(key);
        for i in 0..500 {
            let e = MarketEvent::Trade {
                time: UtcMillis(t("2026-11-04T23:00:00.200Z").0 + i * 100),
                price: Price(600_000),
                qty: Qty(1),
                aggressor: None,
            };
            s.update(|d| d.apply(&[e]));
        }
        let d = s.read();
        assert_eq!(d.ticks.status(key, &d.series, &product), want, "{reset:?}");
    }
}

/// Charts asking for the same series share one builder; it goes when the last lets go.
#[test]
fn leases_share_and_release() {
    let s = store();
    let a = s.acquire_ticks(KEY);
    let b = s.acquire_ticks(KEY);
    let c = s.acquire_ticks(TickKey {
        n: 500,
        reset: TickReset::Continuous,
    });
    assert_eq!(s.read().ticks.slots.len(), 2);
    drop((a, c));
    let _d = s.acquire_ticks(KEY);
    assert_eq!(
        s.read().ticks.slots.len(),
        1,
        "the 500t builder went with its lease"
    );
    drop(b);
}

/// A thin opening: the first trade minutes after the open, with 1-minute bars from the session
/// before (so they would show earlier trading): nothing to say. A feed with no earlier
/// 1-minute bars (nothing to compare) is labelled past the slack.
#[test]
fn a_thin_opening_is_not_a_late_start() {
    let t = |iso: &str| UtcMillis::parse_iso(iso).unwrap();
    // The session opening Wed 2026-11-04 23:00 UTC; the first trade at 23:05.
    let history: Vec<Bar> = (0..30)
        .map(|i| Bar {
            time: UtcMillis(t("2026-11-04T21:00:00Z").0 + i * 60_000),
            open: Price(600_000),
            high: Price(600_025),
            low: Price(600_000),
            close: Price(600_025),
            volume: Qty(10),
        })
        .collect();
    for (with_history, want) in [
        (true, TickStatus::Complete),
        (false, TickStatus::From(t("2026-11-04T23:05:00Z"))),
    ] {
        let s = store();
        if with_history {
            s.update(|d| {
                d.apply(&[MarketEvent::History {
                    bars: history.clone(),
                    as_of: Some(t("2026-11-04T21:29:59.999Z")),
                }])
            });
        }
        let lease = s.acquire_ticks(KEY);
        for i in 0..50 {
            let e = MarketEvent::Trade {
                time: UtcMillis(t("2026-11-04T23:05:00Z").0 + i * 100),
                price: Price(600_000),
                qty: Qty(1),
                aggressor: None,
            };
            s.update(|d| d.apply(&[e]));
        }
        let d = s.read();
        assert_eq!(
            d.ticks.status(lease.key(), &d.series, &d.product),
            want,
            "history: {with_history}"
        );
    }
}

/// The status is cached: the same answer as computed afresh, recomputed when the bars or the
/// 1-minute bars change.
#[test]
fn the_status_is_cached_per_revision() {
    let s = store();
    let lease = s.acquire_ticks(KEY);
    let frames = frames();
    s.update(|d| d.apply(&events(&frames[0].1)));
    let d = s.read();
    let slot = d.ticks.slot(lease.key()).unwrap();
    let first = d.ticks.status(KEY, &d.series, &d.product);
    assert!(slot.status.get().is_some(), "cached");
    assert_eq!(first, slot_status(slot, KEY, &d.series, &d.product));
    assert_eq!(d.ticks.status(KEY, &d.series, &d.product), first);
    drop(d);
    // A rebuild starting: the cache doesn't hide it.
    s.update(|d| {
        if let Some(sl) = d.ticks.slots.iter_mut().find(|sl| sl.key == KEY) {
            sl.pending = Some(Vec::new());
        }
    });
    let d = s.read();
    assert_eq!(
        d.ticks.status(KEY, &d.series, &d.product),
        TickStatus::Building
    );
}

/// A rebuild whose thread never starts or panics falls back to the tape (labelled) instead of
/// building forever, and stops counting as running.
#[test]
fn a_failed_rebuild_falls_back_to_the_tape() {
    let s = store();
    let frames = frames();
    for (_, e) in &frames[..3] {
        s.update(|d| d.apply(&events(e)));
    }
    let tape = s.read().tape.len();
    assert!(tape > 0);
    let alive = Arc::new(());
    for panics in [false, true] {
        let generation = {
            let mut d = s.read();
            d.ticks.slots.clear();
            d.ticks.generation += 1;
            let g = d.ticks.generation;
            d.ticks.slots.push(Slot::new(
                KEY,
                Arc::downgrade(&alive),
                g,
                TickBars::new(KEY.n, KEY.reset, None, 16),
                Some(Vec::new()),
                None,
            ));
            g
        };
        let guard = Rebuilding::new(Arc::downgrade(&s), KEY, generation);
        if panics {
            let h = std::thread::spawn(move || {
                let _guard = guard;
                panic!("a rebuild bug");
            });
            assert!(h.join().is_err());
        } else {
            drop(guard); // as when the thread can't be spawned
        }
        let d = s.read();
        let bars = d.ticks.bars(KEY).expect("no longer rebuilding");
        assert_eq!(bars.trades(), tape as u64, "panics: {panics}");
        assert_ne!(
            d.ticks.status(KEY, &d.series, &d.product),
            TickStatus::Building
        );
    }
}

/// Landing with many held trades applies them outside the lock in chunks: the same bars as
/// applying them all at once.
#[test]
fn landing_applies_held_trades_in_chunks() {
    let s = store();
    let alive = Arc::new(());
    let prints: Vec<Print> = (0..10_000)
        .map(|i| Print {
            time: UtcMillis(start().0 + i),
            price: Price(600_000 + (i % 7) * 25),
            qty: Qty(1),
            aggressor: None,
        })
        .collect();
    {
        let mut d = s.read();
        d.ticks.slots.push(Slot::new(
            KEY,
            Arc::downgrade(&alive),
            3,
            TickBars::new(KEY.n, KEY.reset, None, 1000),
            Some(prints.clone()),
            None,
        ));
    }
    // Over the limit: handed out, still rebuilding.
    let mut b = TickBars::new(KEY.n, KEY.reset, None, 1000);
    let chunk = s.update(|d| take_held(d, KEY, 3, &mut b, 100));
    assert!(matches!(&chunk, Held::Chunk(v) if v.len() == prints.len()));
    assert!(s.read().ticks.bars(KEY).is_none());
    s.update(|d| {
        d.ticks.slots[0].pending = Some(prints.clone());
    });
    land(&s, KEY, 3, TickBars::new(KEY.n, KEY.reset, None, 1000));
    let mut want = TickBars::new(KEY.n, KEY.reset, None, 1000);
    for p in &prints {
        want.push(p.time, p.price, p.qty, p.aggressor);
    }
    let d = s.read();
    assert!(d.ticks.bars(KEY).unwrap().same_bars(&want));
    drop(d);
    // Replaced meanwhile: nothing lands.
    let mut b = TickBars::new(KEY.n, KEY.reset, None, 16);
    assert!(matches!(
        s.update(|d| take_held(d, KEY, 4, &mut b, 100)),
        Held::Gone
    ));
}
