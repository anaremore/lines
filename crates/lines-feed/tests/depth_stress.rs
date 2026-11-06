//! Depth stress (roadmap 5): replay a deep, fast-changing synthetic book at maximum speed while
//! a "UI" thread samples the store every 8 ms, exactly as the DOM pane does (revision check,
//! `try_read`, build the visible ladder rows into a fixed buffer).
//!
//! Metric: **book + volume levels visited per ladder build** (`LadderStats::levels_visited`),
//! the DOM's per-frame work. It must stay within `3 × rows × grouping` (bid, ask and volume
//! levels inside the visible price range) however deep the book is and however fast it
//! changes; and builds are paced by the UI's 8 ms tick, never by the feed's frame rate (depth
//! updates coalesce per frame, decision 0001). Wall time per build is reported, and checked
//! only against a generous bound (CI machines vary).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lines_core::ladder::{build_ladder, inside_row};
use lines_core::{ContractKey, Month, Price, Qty, Side};
use lines_feed::catalog;
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::{FeedConfig, Source, new_store, run_feed};
use tokio::sync::watch;

const LEVELS: i64 = 4_000; // per side
const FRAMES: usize = 40_000;
const CHANGES_PER_FRAME: i64 = 25;
const TICK: i64 = 25; // NQ: 1/100 units, tick 0.25

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn recording() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-depth-stress-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("deep-book.lfr");
    let nq = catalog::get(catalog::NQ).unwrap();
    let header = Header {
        start_wall_ms: 1_793_973_600_000,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: "NQ".into(),
        contract: Some(ContractKey::monthly(nq.id, 2026, Month::new(12).unwrap())),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut bid = 2_500_000i64; // 25000.00
    let mut buf = Vec::new();
    // Snapshot: a deep book on both sides.
    native::encode(&NativeEvent::BookClear { time_us: 0 }, &mut buf);
    for k in 0..LEVELS {
        for (side, p) in [
            (Side::Bid, bid - k * TICK),
            (Side::Ask, bid + (k + 1) * TICK),
        ] {
            native::encode(
                &NativeEvent::BookLevel {
                    time_us: 0,
                    side,
                    price: Price(p),
                    qty: Qty(1 + (k % 50)),
                    orders: 0,
                },
                &mut buf,
            );
        }
    }
    w.write(
        Direction::FromServer,
        Duration::ZERO,
        &Frame::binary(buf.clone()),
    )
    .unwrap();
    for f in 1..=FRAMES {
        buf.clear();
        let t = f as i64 * 1_000; // 1 ms apart; replayed at max speed anyway
        if rng.below(20) == 0 {
            // The inside moves a tick: the old best on one side clears, the other side gains.
            let up = rng.below(2) == 0;
            let (gone, side, at) = if up {
                (Side::Ask, Side::Bid, bid + TICK)
            } else {
                (Side::Bid, Side::Ask, bid)
            };
            native::encode(&lvl(t, gone, at, 0), &mut buf);
            native::encode(&lvl(t, side, at, 1 + rng.below(30) as i64), &mut buf);
            bid += if up { TICK } else { -TICK };
        }
        for _ in 0..CHANGES_PER_FRAME {
            let k = rng.below(200) as i64;
            let (side, p) = if rng.below(2) == 0 {
                (Side::Bid, bid - k * TICK)
            } else {
                (Side::Ask, bid + (k + 1) * TICK)
            };
            native::encode(&lvl(t, side, p, 1 + rng.below(80) as i64), &mut buf);
        }
        let buy = rng.below(2) == 0;
        native::encode(
            &NativeEvent::Trade {
                time_us: t,
                price: Price(if buy { bid + TICK } else { bid }),
                qty: Qty(1 + rng.below(5) as i64),
                aggressor: if buy { Aggressor::Buy } else { Aggressor::Sell },
            },
            &mut buf,
        );
        w.write(
            Direction::FromServer,
            Duration::from_millis(f as u64),
            &Frame::binary(buf.clone()),
        )
        .unwrap();
    }
    w.finish().unwrap();
    path
}

fn lvl(time_us: i64, side: Side, p: i64, q: i64) -> NativeEvent {
    NativeEvent::BookLevel {
        time_us,
        side,
        price: Price(p),
        qty: Qty(q),
        orders: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_book_at_max_speed_keeps_dom_work_bounded() {
    let path = recording();
    let config = FeedConfig::new(Source::Replay {
        path: path.clone(),
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&config).unwrap();
    let (_tx, rx) = watch::channel(false);
    let done = Arc::new(AtomicBool::new(false));

    // The "UI": every 8 ms, if the revision moved, try_read and build the ladder.
    let sampler = {
        let (store, done) = (store.clone(), done.clone());
        std::thread::spawn(move || {
            const ROWS: usize = 60;
            let mut out = Vec::with_capacity(ROWS);
            let (mut builds, mut max_visited, mut max_build) = (0usize, 0usize, Duration::ZERO);
            let mut seen = u64::MAX;
            let mut grouping = 1i64;
            let mut worst_ratio = 0.0f64;
            while !done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(8));
                let rev = store.revision();
                if rev == seen {
                    continue;
                }
                seen = rev;
                let Some(d) = store.try_read() else { continue };
                let units = grouping * TICK;
                let Some(centre) = inside_row(&d.book, d.last_trade, units) else {
                    continue;
                };
                let t0 = Instant::now();
                let st = build_ladder(
                    &d.book,
                    &d.vap,
                    d.last_trade,
                    units,
                    centre + ROWS as i64 / 2,
                    ROWS,
                    &mut out,
                );
                let dt = t0.elapsed();
                drop(d);
                let bound = 3 * ROWS * grouping as usize;
                assert!(
                    st.levels_visited <= bound,
                    "visited {} > {bound} (grouping {grouping})",
                    st.levels_visited
                );
                worst_ratio = worst_ratio.max(st.levels_visited as f64 / bound as f64);
                max_visited = max_visited.max(st.levels_visited);
                max_build = max_build.max(dt);
                builds += 1;
                // Alternate groupings so both the 1-tick and the grouped ladder are measured.
                grouping = if grouping == 1 { 25 } else { 1 };
            }
            (builds, max_visited, max_build, worst_ratio)
        })
    };

    let t0 = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(120),
        run_feed(config, store.clone(), rx),
    )
    .await
    .expect("replay finishes");
    let replay_time = t0.elapsed();
    done.store(true, Ordering::Release);
    let (builds, max_visited, max_build, worst_ratio) = sampler.join().unwrap();

    let d = store.read();
    let frames = d.stats.frames_in;
    println!(
        "depth stress: {frames} frames ({} book levels) replayed in {replay_time:.2?}; \
         {builds} ladder builds (one per 8 ms tick at most); max levels visited {max_visited} \
         ({:.0}% of the 3 x rows x grouping bound); worst build {max_build:.2?}",
        d.book.len(Side::Bid) + d.book.len(Side::Ask),
        worst_ratio * 100.0
    );
    assert_eq!(frames as usize, FRAMES + 1);
    assert!(
        d.book.len(Side::Bid) >= LEVELS as usize - 100,
        "deep book kept"
    );
    assert_eq!(d.stats.book_crossed, 0);
    assert_eq!(d.tape.pushed(), FRAMES as u64);
    // Coalescing: the UI built at most once per tick, far fewer times than frames arrived.
    let ticks = replay_time.as_millis() as usize / 8 + 2;
    assert!(builds <= ticks, "{builds} builds in {ticks} ticks");
    assert!(
        builds < FRAMES / 10,
        "builds track the UI clock, not the feed"
    );
    assert!(
        max_build < Duration::from_millis(4),
        "a ladder build took {max_build:?} (budget: well under one 8 ms frame)"
    );
    let _ = std::fs::remove_file(&path);
}
