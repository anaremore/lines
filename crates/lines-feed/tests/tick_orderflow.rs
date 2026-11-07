//! Tick bars and the stored order flow agree (roadmap 9b, decision 0019), through the real
//! feed driver: a session stored by one launch replays at the next launch's warm start into
//! the tick-bar builders exactly as into the tape and VAP, so a tick chart open at startup, and
//! one asked for later (rebuilt from the warm start's stored trades, then the replay's file or
//! the local store's live segments), equal bars built from both days in one run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, TickReset, UtcMillis};
use lines_feed::disk::{ContractDir, StoreConfig};
use lines_feed::native::{self, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::store::{ChartStore, TAPE_PRINTS};
use lines_feed::ticks::{TickKey, TickLease, TickSource};
use lines_feed::{FeedConfig, Source, catalog, new_store, run_feed};
use tokio::sync::watch;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lines-tof-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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

/// A deterministic trade every ~4 s of a session (about 20 000 a day, more than the tape
/// holds), both sides and side-less, some sharing a millisecond.
fn trades(day: usize) -> Vec<(i64, i64, i64, native::Aggressor)> {
    let (open, close) = (at(DAYS[day].0), at(DAYS[day].1));
    let mut x: u64 = 0x7ade + day as u64;
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
        if !x.is_multiple_of(9) {
            t += 1_000 + (x % 6_000) as i64;
        }
    }
    v
}

/// A `lines` recording of these days' trades, a quote at each close (a live feed's last
/// frame), first frame at the first day's open. `resend`: the previous day's last trades sent
/// again first (as a reconnect's snapshot re-delivers them), received at the open.
fn record(path: &Path, days: &[usize], resend: usize) {
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
    if let Some(prev) = days[0].checked_sub(1) {
        let old = trades(prev);
        for &(t, p, q, a) in &old[old.len() - resend..] {
            let mut buf = Vec::new();
            native::encode(
                &NativeEvent::Trade {
                    time_us: t * 1000,
                    price: Price(p),
                    qty: Qty(q),
                    aggressor: a,
                },
                &mut buf,
            );
            w.write(Direction::FromServer, Duration::ZERO, &Frame::binary(buf))
                .unwrap();
        }
    }
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

/// One launch: the recording through `run_feed` with the store at `root` (if any), charts on
/// `keys` open from before the feed starts (restored with the workspace).
async fn launch(
    path: &Path,
    root: Option<&Path>,
    keys: &[TickKey],
) -> (Arc<ChartStore>, Vec<TickLease>) {
    let mut config = FeedConfig::new(Source::Replay {
        path: path.to_path_buf(),
        speed: 0.0,
        stop_at: None,
    });
    config.store = root.map(|r| StoreConfig::new(r.to_path_buf(), "test"));
    let store = new_store(&config).unwrap();
    let leases = keys.iter().map(|k| store.acquire_ticks(*k)).collect();
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(120),
        run_feed(config, store.clone(), rx),
    )
    .await
    .unwrap();
    (store, leases)
}

const AT_START: TickKey = TickKey {
    n: 233,
    reset: TickReset::Session,
};
const LATER: TickKey = TickKey {
    n: 500,
    reset: TickReset::Continuous,
};
const FROM_STORE: TickKey = TickKey {
    n: 89,
    reset: TickReset::Session,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick_bars_agree_with_the_stored_order_flow_across_a_restart() {
    let dir = temp("restart");
    let (mon, tue, both) = (
        dir.join("mon.lfr"),
        dir.join("tue.lfr"),
        dir.join("both.lfr"),
    );
    record(&mon, &[0], 0);
    // Tuesday's recording starts by re-sending Monday's last trades: already stored and
    // replayed at the warm start (at or before `stored_through`), so live drops them and the
    // writer doesn't store them (`frame_counted`); a stored-source rebuild reads them once.
    record(&tue, &[1], 25);
    record(&both, &[0, 1], 0);
    let root = dir.join("store");
    launch(&mon, Some(&root), &[]).await;
    // Tuesday's launch: a tick chart open from startup, Monday from the store's warm start.
    let (warm, _at_start) = launch(&tue, Some(&root), &[AT_START]).await;
    let (reference, _r) = launch(&both, None, &[AT_START, LATER, FROM_STORE]).await;
    let all = (trades(0).len() + trades(1).len()) as u64;

    // Asked once the feed ran: Monday read again by the warm start's record, Tuesday from
    // the replay's file, then the tape.
    let mut later = warm.acquire_ticks(LATER);
    later.join_rebuild();
    // The same, with Tuesday read from the local store's segments (as a live feed does): the
    // writer stored exactly the trades counted.
    warm.update(|d| d.tick_source = Some(TickSource::Stored(es_dir(&root))));
    let mut from_store = warm.acquire_ticks(FROM_STORE);
    from_store.join_rebuild();

    let (w, r) = (warm.read(), reference.read());
    assert!(w.counted_n > TAPE_PRINTS as u64, "a real rebuild");
    assert_eq!(
        w.counted_n, all,
        "Monday stored, Tuesday live: each trade once"
    );
    assert_eq!(w.vap, r.vap);
    for key in [AT_START, LATER, FROM_STORE] {
        let (wb, rb) = (w.ticks.bars(key).unwrap(), r.ticks.bars(key).unwrap());
        assert_eq!(
            wb.trades(),
            all,
            "{key:?}: the same set as the tape and VAP"
        );
        assert!(wb.same_bars(rb), "{key:?}: equals both days built live");
        assert_eq!(
            w.ticks.status(key, &w.series, &w.product),
            r.ticks.status(key, &r.series, &r.product),
            "{key:?}: the same label"
        );
    }
    drop((w, r));
    let _ = std::fs::remove_dir_all(&dir);
}

fn es_dir(root: &Path) -> ContractDir {
    ContractDir::new(
        root,
        "test",
        &catalog::get(catalog::ES).unwrap(),
        Some(contract()),
    )
}
