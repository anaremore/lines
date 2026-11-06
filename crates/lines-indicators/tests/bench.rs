//! Indicator CPU per tick (decision 0017's tracked benchmark): 6 charts, each its own contract,
//! each with 2 VWAPs (ETH and RTH, bands on), after 3 days of 1-minute history.
//! Run: `cargo test --release -p lines-indicators --test bench -- --ignored --nocapture`

use std::time::{Duration, Instant};

use lines_core::sessions::{
    ExchangeHours, HolidayTable, Hours, LocalTime, SessionCalendar, SessionSpec,
};
use lines_core::{Bar, Price, Qty, UtcMillis};
use lines_indicators::graph::make_builtin;
use lines_indicators::{Bands, Graph, Input, Reset, SeriesKey, Spec, VwapSpec};

fn es() -> &'static SessionCalendar {
    let table = HolidayTable::parse(
        "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 unverified\nsource t\n",
    )
    .unwrap();
    let spec = SessionSpec {
        time_zone: "America/Chicago",
        hours: Hours::Exchange(ExchangeHours {
            eth_open: LocalTime::hm(17, 0),
            eth_close: LocalTime::hm(16, 0),
            rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
        }),
        holidays: Some("c"),
    };
    Box::leak(Box::new(SessionCalendar::new(spec, Some(&table)).unwrap()))
}

#[test]
#[ignore = "benchmark; run with --release"]
fn indicator_cpu_per_tick_6_charts_2_vwaps() {
    const FEEDS: u64 = 6;
    const TICKS: usize = 200_000;
    let cal = es();
    let mut g = Graph::new();
    let mut make = |k: &SeriesKey| make_builtin(k, Some(cal), 50_000);
    let start = UtcMillis::parse_iso("2026-11-09T14:00:00Z").unwrap().0; // Monday
    for feed in 1..=FEEDS {
        for reset in [Reset::Eth, Reset::Rth] {
            g.acquire(
                SeriesKey {
                    feed,
                    spec: Spec::Vwap {
                        period_ms: 60_000,
                        vwap: VwapSpec {
                            reset,
                            bands: Bands::ALL,
                        },
                    },
                },
                &mut make,
            )
            .unwrap();
        }
        // 3 days of history, then live.
        let bars: Vec<Bar> = (0..3 * 1440)
            .map(|i| Bar {
                time: UtcMillis(start - (3 * 1440 - i) * 60_000),
                open: Price(600_000),
                high: Price(600_100),
                low: Price(599_900),
                close: Price(600_000 + (i % 40) * 25),
                volume: Qty(100),
            })
            .collect();
        g.push(
            feed,
            &[
                Input::Connect {
                    expect_history: true,
                },
                Input::History {
                    bars,
                    as_of: Some(UtcMillis(start - 1)),
                },
                Input::HistoryComplete,
            ],
        );
    }
    assert_eq!(
        g.len(),
        (FEEDS * 3) as usize,
        "per feed: the market series and 2 VWAPs"
    );
    let mut times = Vec::with_capacity(TICKS);
    let mut x: u64 = 1;
    for k in 0..TICKS {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let feed = 1 + (k as u64 % FEEDS);
        let t = start + (k as i64) * 13; // ~77 ticks/s per contract over ~43 minutes
        let input = [Input::Trade {
            time: UtcMillis(t),
            price: Price(600_000 + ((x >> 33) % 80) as i64 * 25),
            qty: Qty(1 + ((x >> 40) % 9) as i64),
        }];
        let t0 = Instant::now();
        g.push(feed, &input);
        times.push(t0.elapsed());
    }
    times.sort();
    let mean = times.iter().sum::<Duration>() / TICKS as u32;
    let pct = |p: f64| times[((TICKS as f64 * p) as usize).min(TICKS - 1)];
    println!(
        "indicator CPU per tick (6 contracts x 2 VWAPs with bands, 3 days of 1-min history): \
         mean {mean:?}, p50 {:?}, p99 {:?}, max {:?} -> {:.1} us per tick per VWAP",
        pct(0.5),
        pct(0.99),
        times[TICKS - 1],
        mean.as_secs_f64() * 1e6 / 2.0
    );
}
