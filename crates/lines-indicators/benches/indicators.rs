//! Indicator updates per trade (docs/benchmarks.md): one feed with an ETH VWAP (±1/2/3σ bands)
//! after 3 days of 1-minute history, and the same feed with ETH + RTH VWAPs and cumulative
//! delta. Each iteration pushes the next 1,000 live trades (time keeps moving forward).
//!
//! Run: `cargo bench -p lines-indicators --bench indicators`. Synthetic, deterministic.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use lines_core::sessions::{
    ExchangeHours, HolidayTable, Hours, LocalTime, SessionCalendar, SessionSpec,
};
use lines_core::{Aggressor, Bar, Price, Qty, UtcMillis};
use lines_indicators::graph::make_builtin;
use lines_indicators::{
    Bands, DeltaReset, DeltaSpec, Graph, Input, Reset, SeriesKey, Spec, VwapSpec,
};

const BATCH: usize = 1_000;
/// Monday 2026-11-09 14:00Z.
const START: i64 = 1_794_232_800_000;

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

fn graph(specs: &[Spec]) -> Graph {
    let cal = es();
    let mut g = Graph::new();
    let mut make = |k: &SeriesKey| make_builtin(k, Some(cal), 50_000);
    for spec in specs {
        g.acquire(SeriesKey { feed: 1, spec: spec.clone() }, &mut make).unwrap();
    }
    let bars: Vec<Bar> = (0..3 * 1440)
        .map(|i| Bar {
            time: UtcMillis(START - (3 * 1440 - i) * 60_000),
            open: Price(600_000),
            high: Price(600_100),
            low: Price(599_900),
            close: Price(600_000 + (i % 40) * 25),
            volume: Qty(100),
        })
        .collect();
    g.push(
        1,
        &[
            Input::Connect {
                expect_history: true,
            },
            Input::History {
                bars,
                as_of: Some(UtcMillis(START - 1)),
            },
            Input::HistoryComplete,
        ],
    );
    g
}

fn vwap(reset: Reset) -> Spec {
    Spec::Vwap {
        period_ms: 60_000,
        vwap: VwapSpec {
            reset,
            bands: Bands::ALL,
        },
    }
}

fn updates(c: &mut Criterion) {
    let delta = Spec::CumDelta {
        period_ms: 60_000,
        delta: DeltaSpec {
            reset: DeltaReset::Eth,
        },
    };
    let mut g = c.benchmark_group("indicators");
    g.throughput(Throughput::Elements(BATCH as u64));
    for (name, specs) in [
        ("vwap_eth_bands", vec![vwap(Reset::Eth)]),
        (
            "vwap_eth_rth_plus_delta",
            vec![vwap(Reset::Eth), vwap(Reset::Rth), delta],
        ),
    ] {
        let mut graph = graph(&specs);
        let mut k: u64 = 0;
        let mut x: u64 = 1;
        let mut batch = Vec::with_capacity(1);
        g.bench_function(name, |b| {
            b.iter(|| {
                for _ in 0..BATCH {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    k += 1;
                    batch.clear();
                    batch.push(Input::Trade {
                        time: UtcMillis(START + k as i64 * 50),
                        price: Price(600_000 + ((x >> 33) % 80) as i64 * 25),
                        qty: Qty(1 + ((x >> 40) % 9) as i64),
                        aggressor: Some(if x >> 50 & 1 == 0 {
                            Aggressor::Buy
                        } else {
                            Aggressor::Sell
                        }),
                    });
                    graph.push(1, &batch);
                }
                black_box(graph.len());
            })
        });
    }
    g.finish();
}

criterion_group!(benches, updates);
criterion_main!(benches);
