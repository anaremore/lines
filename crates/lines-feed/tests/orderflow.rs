//! Stored order flow at feed startup (decision 0019, `disk::orderflow`) through the real feed
//! driver: a session stored by one launch replays, at the next launch, into the tape, the VAP
//! and the profiles (footprint included) exactly as the same trades build them live; damaged
//! segments give up their coverage after the last good block; a replay without the store is
//! unchanged.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis, sessions::SessionKind};
use lines_feed::disk::{ContractDir, StoreConfig, orderflow};
use lines_feed::native::{self, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::store::ChartStore;
use lines_feed::{FeedConfig, Source, catalog, new_store, run_feed};
use tokio::sync::watch;

fn temp(name: &str) -> lines_core::scratch::Scratch {
    lines_core::scratch::Scratch::new(&format!("of-{name}"))
}

fn at(s: &str) -> i64 {
    UtcMillis::parse_iso(s).unwrap().0
}

fn contract() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

/// ES sessions: Monday 2026-11-10 (23:00Z the evening before to 22:00Z), then Tuesday.
const DAYS: [(&str, &str); 2] = [
    ("2026-11-09T23:00:00Z", "2026-11-10T22:00:00Z"),
    ("2026-11-10T23:00:00Z", "2026-11-11T22:00:00Z"),
];

/// A deterministic trade every ~20 s of a session, both sides and side-less.
fn trades(day: usize) -> Vec<(i64, i64, i64, native::Aggressor)> {
    let (open, close) = (at(DAYS[day].0), at(DAYS[day].1));
    let mut x: u64 = 0x5eed + day as u64;
    let mut t = open + 250;
    let mut v = Vec::new();
    while t < close {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let aggr = match x % 5 {
            0 => native::Aggressor::Unknown,
            1 | 2 => native::Aggressor::Sell,
            _ => native::Aggressor::Buy,
        };
        v.push((t, 600_000 + (x % 40) as i64 * 25, 1 + (x % 4) as i64, aggr));
        t += 15_000 + (x % 10_000) as i64;
    }
    v
}

/// A `lines` recording of these days' trades, a quote at each close (a live feed's last
/// frame), first frame at the first day's open.
fn record(path: &Path, days: &[usize]) {
    let start = at(DAYS[days[0]].0);
    let mut w = Writer::create(
        path,
        &Header {
            start_wall_ms: start,
            provider: native::PROVIDER.into(),
            protocol: native::PROTOCOL.into(),
            product: "ES".into(),
            contract: Some(contract()),
            version: FORMAT_VERSION,
        },
    )
    .unwrap();
    let mut frame = |t: i64, e: NativeEvent| {
        let mut buf = Vec::new();
        native::encode(&e, &mut buf);
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - start) as u64),
            &Frame::binary(buf),
        )
        .unwrap();
    };
    for &d in days {
        for (t, p, q, a) in trades(d) {
            frame(
                t,
                NativeEvent::Trade {
                    time_us: t * 1000,
                    price: Price(p),
                    qty: Qty(q),
                    aggressor: a,
                },
            );
        }
        let close = at(DAYS[d].1);
        frame(
            close,
            NativeEvent::Bbo {
                time_us: close * 1000,
                bid: Some(Price(600_000)),
                ask: Some(Price(600_025)),
            },
        );
    }
    w.finish().unwrap();
}

/// One launch: the recording through `run_feed`, with the store at `root` (if any).
async fn launch(path: &Path, root: Option<&Path>, sessions: u32) -> Arc<ChartStore> {
    let mut config = FeedConfig::new(Source::Replay {
        path: path.to_path_buf(),
        speed: 0.0,
        stop_at: None,
    });
    config.store = root.map(|r| StoreConfig::new(r.to_path_buf(), "test"));
    config.orderflow_sessions = sessions;
    let store = new_store(&config).unwrap();
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .unwrap();
    store
}

fn es_dir(root: &Path) -> ContractDir {
    ContractDir::new(
        root,
        "test",
        &catalog::get(catalog::ES).unwrap(),
        Some(contract()),
    )
}

/// Monday stored by a first launch; Tuesday live at the next one. Everything that counts
/// trades equals one launch that saw both days live: nothing missing, nothing twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_order_flow_equals_live_built() {
    let dir = temp("equal");
    let (mon, tue, both) = (
        dir.join("mon.lfr"),
        dir.join("tue.lfr"),
        dir.join("both.lfr"),
    );
    record(&mon, &[0]);
    record(&tue, &[1]);
    record(&both, &[0, 1]);
    let root = dir.join("store");
    launch(&mon, Some(&root), 2).await;
    let warm = launch(&tue, Some(&root), 2).await;
    let reference = launch(&both, None, 2).await;
    let warm_bars = {
        let (w, r) = (warm.read(), reference.read());
        assert!(!w.orderflow_loading);
        assert_eq!(w.vap, r.vap);
        let total: i64 = trades(0).iter().chain(&trades(1)).map(|t| t.2).sum();
        assert_eq!(w.vap.total(), Qty(total));
        for kind in [SessionKind::Eth, SessionKind::Rth] {
            assert_eq!(
                w.profiles.sessions(kind),
                r.profiles.sessions(kind),
                "{kind:?}"
            );
        }
        let all = (UtcMillis(at(DAYS[0].0)), UtcMillis(at(DAYS[1].1)));
        let fp = |s: &lines_feed::ChartData| -> Vec<_> {
            s.profiles.footprint().range(all.0, all.1).collect()
        };
        assert_eq!(fp(&w), fp(&r));
        let tape =
            |s: &lines_feed::ChartData| -> Vec<_> { s.tape.iter_newest().copied().collect() };
        assert_eq!(tape(&w), tape(&r));
        // Monday is counted from its first stored frame (the recording's first trade) to the
        // close: no gap; `partial from` only for the 250 ms before that first frame.
        let g = w.counted_gaps(at(DAYS[0].0), at(DAYS[0].1) - 1);
        assert!(g.any && g.first_gap.is_none(), "{g:?}");
        assert_eq!(g.data_from, Some(trades(0)[0].0));
        w.series.len()
    };
    // Lookback 0: Monday's bars still show (the cache), but no stored order flow.
    let cold_root = dir.join("store-cold");
    launch(&mon, Some(&cold_root), 0).await;
    let cold = launch(&tue, Some(&cold_root), 0).await;
    let c = cold.read();
    let tuesday: i64 = trades(1).iter().map(|t| t.2).sum();
    assert_eq!(
        c.vap.total(),
        Qty(tuesday),
        "lookback 0: no stored order flow"
    );
    assert!(c.series.len() > warm_bars / 2, "cached bars still shown");
}

/// A segment that ends corrupt or torn gives up its coverage after the last good block: the
/// trades before it replay, the rest of that day stays uncovered (approximate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn damaged_segments_withdraw_coverage_after_the_last_good_block() {
    let dir = temp("damaged");
    let mon = dir.join("mon.lfr");
    record(&mon, &[0]);
    let root = dir.join("store");
    launch(&mon, Some(&root), 2).await;
    let d = es_dir(&root);
    let seg = d.segment_path(lines_core::CivilDate::new(2026, 11, 10).unwrap());
    let bytes = std::fs::read(&seg).unwrap();
    let n = trades(0).len();
    let close = at(DAYS[0].1);
    for (case, damaged) in [
        ("corrupt", {
            let mut b = bytes.clone();
            let i = b.len() / 2;
            b[i] ^= 0x5a;
            b
        }),
        ("torn", bytes[..bytes.len() / 2].to_vec()),
    ] {
        std::fs::write(&seg, &damaged).unwrap();
        let mut got = Vec::new();
        let mut covered = Vec::new();
        let s = orderflow::read(&d, i64::MIN, |c| {
            got.extend(c.trades);
            covered.extend(c.covered);
        })
        .unwrap();
        assert_eq!(s.cut, 1, "{case}");
        assert!(
            !got.is_empty() && got.len() < n,
            "{case}: {} of {n}",
            got.len()
        );
        let end = covered.last().unwrap().1;
        assert!(
            end < close - 60_000,
            "{case}: coverage withdrawn after the cut"
        );
        assert!(got.iter().all(|p| p.time.0 <= end), "{case}");
        // Exactly the trades up to the cut, in order.
        let expect: Vec<i64> = trades(0)
            .iter()
            .map(|t| t.0)
            .filter(|t| *t <= end)
            .collect();
        assert_eq!(
            got.iter().map(|p| p.time.0).collect::<Vec<_>>(),
            expect,
            "{case}"
        );
    }
    std::fs::write(&seg, &bytes).unwrap();
}

/// Replays and screenshots run without the store: nothing of this applies, and the profile's
/// `partial from` is where the replay's trades start, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_without_the_store_is_unchanged() {
    let dir = temp("plain");
    let tue = dir.join("tue.lfr");
    record(&tue, &[1]);
    let s = launch(&tue, None, 2).await;
    let d = s.read();
    assert!(!d.orderflow_loading);
    let first = trades(1)[0].0;
    assert_eq!(d.profiles.data_start(), Some(UtcMillis(first)));
    let g = d.counted_gaps(at(DAYS[1].0), at(DAYS[1].1) - 1);
    assert_eq!(
        g.data_from,
        Some(first),
        "partial from the first trade, as before"
    );
    assert_eq!(g.first_gap, None);
}

/// Timing (not a test of speed; run by hand, release build):
/// `cargo test --release -p lines-feed --test orderflow -- --ignored --nocapture`.
/// Three busy synthetic ES trade dates (a trade every ~200 ms, about 430k a day) written
/// straight into a store; then the startup replay for 1, 2 and 3 sessions: the read, the
/// chart store's counting, and the indicator engine's fold.
#[test]
#[ignore]
fn perf_stored_order_flow_startup() {
    use lines_feed::disk::Coverage;
    use lines_feed::disk::segment::{ENCODING, FlushPolicy, SegmentHeader, SegmentWriter};
    use lines_indicators::series::{Input, MarketSeries};
    let root = temp("perf");
    let d = es_dir(&root);
    let dates: Vec<lines_core::CivilDate> = (10..=12)
        .map(|day| lines_core::CivilDate::new(2026, 11, day).unwrap())
        .collect();
    let mut cov = Coverage::default();
    let mut x: u64 = 0xfeed;
    let mut total = 0u64;
    for &civil in &dates {
        let (open, close) = lines_feed::disk::partition::trade_date_span(&d.product, civil);
        let header = SegmentHeader {
            created_wall_ms: 0,
            source: "test".into(),
            encoding: ENCODING.into(),
            product: "ES".into(),
            contract: Some(contract()),
            date: civil,
        };
        let (mut w, _) =
            SegmentWriter::open(&d.segment_path(civil), &header, FlushPolicy::default()).unwrap();
        let mut t = open.0;
        let mut buf = Vec::new();
        while t < close.0 {
            buf.clear();
            // A frame of 1-4 trades, as a busy feed batches them.
            for _ in 0..1 + x % 4 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                native::encode(
                    &NativeEvent::Trade {
                        time_us: t * 1000,
                        price: Price(600_000 + (x % 80) as i64 * 25),
                        qty: Qty(1 + (x % 6) as i64),
                        aggressor: if x.is_multiple_of(2) {
                            native::Aggressor::Buy
                        } else {
                            native::Aggressor::Sell
                        },
                    },
                    &mut buf,
                );
                total += 1;
            }
            w.append(t * 1000, &buf).unwrap();
            t += 200 + (x % 300) as i64;
        }
        w.close().unwrap();
        cov.trades.add(open.0, close.0 - 1);
    }
    cov.save(&d.coverage_path()).unwrap();
    let bytes: u64 = dates
        .iter()
        .map(|c| std::fs::metadata(d.segment_path(*c)).unwrap().len())
        .sum();
    println!(
        "store: {total} trades over {} trade dates, {:.1} MB of segments",
        dates.len(),
        bytes as f64 / 1e6
    );
    let newest = UtcMillis(
        lines_feed::disk::partition::trade_date_span(&d.product, dates[2])
            .1
            .0
            - 1,
    );
    for sessions in [1u32, 2, 3] {
        let from = orderflow::lookback_start(&d, newest, sessions).unwrap();
        let chart = ChartStore::new(
            catalog::get(catalog::ES).unwrap(),
            Some(contract()),
            lines_core::Capabilities::default(),
            "test",
            60_000,
            50_000,
        );
        let mut series = MarketSeries::new(60_000, 50_000);
        let (mut apply, mut engine) = (Duration::ZERO, Duration::ZERO);
        let t0 = std::time::Instant::now();
        let s = orderflow::read(&d, from, |c| {
            let a = std::time::Instant::now();
            chart.update(|cd| cd.apply_stored(&c.trades, &c.covered));
            apply += a.elapsed();
            let e = std::time::Instant::now();
            series.apply(&[Input::Stored {
                trades: c.trades,
                covered: c.covered,
            }]);
            engine += e.elapsed();
        })
        .unwrap();
        let all = t0.elapsed();
        println!(
            "lookback {sessions}: {} trades in {all:?} (read+decode {:?}, chart store {apply:?}, \
             engine {engine:?})",
            s.replayed,
            all.saturating_sub(apply + engine),
        );
    }
}

/// The store holds exactly the trades live counted: the writer stores the trades the chart
/// counted in each frame (`ChartData::frame_counted`). A connection's late trades (a REST fill
/// older than newer streamed ones) are stored, as live counts them; a reconnect's re-sent
/// trades the new snapshot covers are not; so the stored session replays to what live built.
#[test]
fn the_store_holds_exactly_the_counted_trades() {
    use lines_feed::connection::EventSink;
    use lines_feed::disk::ContractWriter;
    use lines_feed::provider::MarketEvent;
    let root = temp("writer-counted");
    let es = catalog::get(catalog::ES).unwrap();
    let caps = lines_core::Capabilities {
        history: true,
        ..lines_core::Capabilities::default()
    };
    let store = Arc::new(ChartStore::new(
        es.clone(),
        Some(contract()),
        caps,
        "test",
        60_000,
        1000,
    ));
    let cfg = StoreConfig::new(root.to_path_buf(), "test");
    let (mut w, _) = ContractWriter::open(&cfg, &es, Some(contract()), store.clone()).unwrap();
    let t0 = at("2026-11-10T15:00:00Z");
    let ms = |s: f64| t0 + (s * 1000.0) as i64;
    let trade = |t: i64| MarketEvent::Trade {
        time: UtcMillis(t),
        price: Price(600_000 + (t % 7) * 25),
        qty: Qty(1 + t % 3),
        aggressor: None,
    };
    let snapshot = |as_of: i64| {
        vec![
            MarketEvent::History {
                bars: vec![],
                as_of: Some(UtcMillis(as_of)),
            },
            MarketEvent::HistoryComplete,
        ]
    };
    // As the feed does: the chart applies the frame, then the writer stores it.
    let frame = |w: &mut ContractWriter, events: Vec<MarketEvent>, now: i64| {
        store.update_frame(|d| d.apply(&events));
        w.frame(&events, UtcMillis(now));
    };
    // Connection 1: a snapshot to t0, trades 1-5 s, then a late one at 2.5 s.
    store.update(|d| d.begin_connection());
    w.begin(None, UtcMillis(t0));
    frame(&mut w, snapshot(t0), t0);
    for s in [1.0, 2.0, 3.0, 4.0, 5.0, 2.5] {
        frame(&mut w, vec![trade(ms(s))], ms(5.0).max(ms(s)));
    }
    w.end(UtcMillis(ms(5.0)));
    // Connection 2: a snapshot to 5 s, then 3-5 s re-sent (covered by it), then 6 and 7 s.
    store.update(|d| d.begin_connection());
    w.begin(None, UtcMillis(ms(5.0)));
    frame(&mut w, snapshot(ms(5.0)), ms(5.5));
    frame(
        &mut w,
        vec![
            trade(ms(3.0)),
            trade(ms(4.0)),
            trade(ms(5.0)),
            trade(ms(6.0)),
        ],
        ms(6.0),
    );
    frame(&mut w, vec![trade(ms(7.0))], ms(7.0));
    w.end(UtcMillis(ms(7.0)));
    drop(w);
    let mut got = Vec::new();
    orderflow::read(&es_dir(&root), i64::MIN, |c| got.extend(c.trades)).unwrap();
    let times: Vec<i64> = got.iter().map(|p| p.time.0).collect();
    let expect: Vec<i64> = [1.0, 2.0, 3.0, 4.0, 5.0, 2.5, 6.0, 7.0]
        .into_iter()
        .map(ms)
        .collect();
    assert_eq!(times, expect);
    // Replayed into a fresh chart: what live counted, exactly.
    let fresh = ChartStore::new(es, Some(contract()), caps, "test", 60_000, 1000);
    fresh.update(|d| d.apply_stored(&got, &[]));
    assert_eq!(fresh.read().vap, store.read().vap);
}
