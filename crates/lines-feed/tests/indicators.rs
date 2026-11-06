//! The indicator runtime end to end: a replay through the feed driver taps the engine; VWAP
//! handles on the feed fill from the indicator thread; the same replay gives identical outputs
//! (decision 0017: deterministic); two handles on one spec share one node.

use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::indicators::{FeedInfo, engine};
use lines_feed::native::{self, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::{FeedConfig, Source, catalog, new_store, run_feed};
use lines_indicators::{Bands, Point, Reset, Spec, VwapSpec};
use tokio::sync::watch;

fn at(iso: &str) -> UtcMillis {
    UtcMillis::parse_iso(iso).unwrap()
}

/// NQ trades from Tuesday 13:00 to Wednesday 01:00 UTC: across Tuesday's 16:00 CT close and
/// Wednesday's 17:00 CT open.
fn recording() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-ind-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nq.lfr");
    let nq = catalog::get(catalog::NQ).unwrap();
    let start = at("2026-11-10T13:00:00Z");
    let header = Header {
        start_wall_ms: start.0,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: "NQ".into(),
        contract: Some(ContractKey::monthly(nq.id, 2026, Month::new(12).unwrap())),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    let mut x: u64 = 7;
    for i in 0..4_000i64 {
        let t = start.0 + i * 10_800; // every 10.8 s for 12 hours
        if (at("2026-11-10T22:00:00Z").0..at("2026-11-10T23:00:00Z").0).contains(&t) {
            continue; // the daily halt
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let mut buf = Vec::new();
        native::encode(
            &NativeEvent::Trade {
                time_us: t * 1000,
                price: Price(2_500_000 + (x % 200) as i64 * 25),
                qty: Qty(1 + (x % 5) as i64),
                aggressor: native::Aggressor::Unknown,
            },
            &mut buf,
        );
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - start.0) as u64),
            &Frame::binary(buf),
        )
        .unwrap();
    }
    w.finish().unwrap();
    path
}

async fn run_once(path: std::path::PathBuf) -> (Vec<Point>, Vec<Point>, Vec<Point>) {
    let config = FeedConfig::new(Source::Replay {
        path,
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&config).unwrap();
    let info = FeedInfo {
        feed: store.feed_id(),
        product: catalog::NQ,
        period_ms: config.bar_period_ms,
        max_bars: config.max_bars,
    };
    let spec = |reset| Spec::Vwap {
        period_ms: 60_000,
        vwap: VwapSpec {
            reset,
            bands: Bands::ALL,
        },
    };
    // Two charts asking for the same VWAP, one for RTH.
    let a = engine().acquire(info, spec(Reset::Eth));
    let b = engine().acquire(info, spec(Reset::Eth));
    let rth = engine().acquire(info, spec(Reset::Rth));
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .unwrap();
    engine().sync();
    let read = |h: &lines_feed::indicators::IndicatorHandle| {
        let d = h.cell().read();
        assert!(d.error.is_none(), "{:?}", d.error);
        d.points.clone()
    };
    let (pa, pb, pr) = (read(&a), read(&b), read(&rth));
    assert_eq!(pa, pb, "one node, two consumers, the same series");
    // A chart that adds a VWAP after the replay has finished (its data is still on screen)
    // gets the same series: the feed's base outlives the feed task.
    let late = engine().acquire(info, spec(Reset::Eth));
    engine().sync();
    assert_eq!(
        read(&late),
        pa,
        "added after the feed ended: identical, flags included"
    );
    assert_eq!(
        pa.iter().filter(|p| p.forming).count(),
        1,
        "only the newest point is forming"
    );
    (pa, pb, pr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_drives_shared_deterministic_vwaps() {
    let path = recording();
    let (eth1, _, rth1) = run_once(path.clone()).await;
    let (eth2, _, rth2) = run_once(path).await;
    assert!(
        eth1.len() > 500 && rth1.len() > 100,
        "{} {}",
        eth1.len(),
        rth1.len()
    );
    assert_eq!(
        eth1, eth2,
        "replaying the same recording gives identical outputs"
    );
    assert_eq!(rth1, rth2);
    // Tuesday's session and Wednesday's (from 23:00Z) are separate periods.
    let wed = at("2026-11-10T23:00:00Z");
    let before = eth1.iter().rfind(|p| p.time < wed).unwrap();
    let after = eth1.iter().find(|p| p.time >= wed).unwrap();
    assert_ne!(
        before.period, after.period,
        "ETH reset at the 17:00 CT open"
    );
    // RTH (08:30–15:00 CT = 14:30–21:00Z) has no values outside it.
    assert!(rth1.iter().all(|p| {
        (at("2026-11-10T14:30:00Z").0..at("2026-11-10T21:00:00Z").0).contains(&p.time.0)
    }));
    // A replay has no history: every value comes from trades, after the first partial minute.
    assert!(eth1[1..].iter().all(|p| p.approx == eth1[1].approx));
}

/// A replay that starts at the session open holds every trade of the session, so its ETH VWAP
/// is exact from the first bucket on: Σpv/Σv of the trades, never "≈ from bars". The first
/// frame is a quote at the open; the first trade follows 250 ms later (before the fix, that
/// trade started the "complete" run, so the open's minute and with it the whole session were
/// approximate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_from_the_session_open_is_exact() {
    let dir = std::env::temp_dir().join(format!("lines-ind-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("es-open.lfr");
    let es = catalog::get(catalog::ES).unwrap();
    // Trade date 2026-11-05 (Thursday, CST): ETH opens 17:00 CT = 23:00 UTC the day before.
    let open = at("2026-11-04T23:00:00Z");
    let header = Header {
        start_wall_ms: open.0,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: "ES".into(),
        contract: Some(ContractKey::monthly(es.id, 2026, Month::new(12).unwrap())),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    let mut frame = |t: i64, e: NativeEvent| {
        let mut buf = Vec::new();
        native::encode(&e, &mut buf);
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - open.0) as u64),
            &Frame::binary(buf),
        )
        .unwrap();
    };
    frame(
        open.0,
        NativeEvent::Bbo {
            time_us: open.0 * 1000,
            bid: Some(Price(602_000)),
            ask: Some(Price(602_025)),
        },
    );
    // (ms after the open, price, qty) over three minutes.
    let trades = [
        (250, 602_000, 3),
        (20_000, 602_025, 1),
        (59_999, 602_050, 4),
        (60_000, 601_975, 2),
        (95_500, 602_100, 5),
        (130_000, 602_000, 7),
    ];
    for (dt, p, q) in trades {
        let t = open.0 + dt;
        frame(
            t,
            NativeEvent::Trade {
                time_us: t * 1000,
                price: Price(p),
                qty: Qty(q),
                aggressor: native::Aggressor::Unknown,
            },
        );
    }
    w.finish().unwrap();

    let config = FeedConfig::new(Source::Replay {
        path,
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&config).unwrap();
    let info = FeedInfo {
        feed: store.feed_id(),
        product: catalog::ES,
        period_ms: config.bar_period_ms,
        max_bars: config.max_bars,
    };
    let h = engine().acquire(
        info,
        Spec::Vwap {
            period_ms: 60_000,
            vwap: VwapSpec {
                reset: Reset::Eth,
                bands: Bands::ALL,
            },
        },
    );
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .unwrap();
    engine().sync();
    let points = h.cell().read().points.clone();
    assert_eq!(points.len(), 3, "{points:?}");
    // Hand-computed: Σpv / Σv over the trades up to each bucket's end.
    let vwap_through = |end_ms: i64| {
        let (pv, v) = trades
            .iter()
            .filter(|t| t.0 < end_ms)
            .fold((0i64, 0i64), |(pv, v), t| (pv + t.1 * t.2, v + t.2));
        pv as f64 / v as f64
    };
    for (i, p) in points.iter().enumerate() {
        assert!(!p.approx, "bucket {i} is exact (no ≈): {p:?}");
        let want = vwap_through((i as i64 + 1) * 60_000);
        assert!(
            (p.vwap - want).abs() < 1e-9,
            "bucket {i}: {} vs {want}",
            p.vwap
        );
    }
    // The last one, spelled out: (6020.00×3 + 6020.25×1 + 6020.50×4 + 6019.75×2 + 6021.00×5
    // + 6020.00×7) / 22 contracts
    // = 13_244_675 / 22 units = 6020.31 (price units are hundredths).
    assert!((points[2].vwap - 13_244_675.0 / 22.0).abs() < 1e-9);
}
