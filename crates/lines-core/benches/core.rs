//! Hot paths of `lines-core` (docs/benchmarks.md): order book apply, 1-minute bars through the
//! stitch, a 5-minute derived timeframe, footprint/profile aggregation, the tick, volume and
//! range bar builders, and one chart frame's geometry and layout (no GPU).
//!
//! Run: `cargo bench -p lines-core --bench core`. Inputs are synthetic and deterministic.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use lines_core::footprint::{FootprintFrame, ImbalanceRule};
use lines_core::geometry::{CrosshairMode, crosshair, write_ohlcv};
use lines_core::ladder::{LadderView, build_ladder, inside_row};
use lines_core::sessions::{
    ExchangeHours, HolidayTable, Hours, LocalTime, SessionCalendar, SessionSpec,
};
use lines_core::{
    Aggressor, BarKind, BarScale, BarSeries, BarsView, Derived, DisplayStyle, FixedStr,
    OrderBook, Price, PriceFormat, PriceScale, ProfileRows, Qty, QtyFormat, Scene, SessionProfiles,
    Side, Stitcher, TickBars, TickReset, Timeframe, UtcMillis, Viewport, VolumeAtPrice,
};

/// Trades per measured batch.
const TRADES: usize = 10_000;
/// ES: 1/100 price units, tick 25.
const TICK: i64 = 25;
/// Monday 2026-11-09 14:00Z, inside the ES RTH session.
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

#[derive(Copy, Clone)]
struct Trade {
    time: UtcMillis,
    price: Price,
    qty: Qty,
    aggressor: Option<Aggressor>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A random walk on the tick grid, ~20 trades/s, starting `from`.
fn trades(n: usize, from: i64, seed: u64) -> Vec<Trade> {
    let mut rng = Rng(seed);
    let mut p = 600_000i64;
    (0..n)
        .map(|i| {
            let r = rng.next();
            p += [-TICK, 0, 0, TICK][(r % 4) as usize];
            Trade {
                time: UtcMillis(from + i as i64 * 50),
                price: Price(p),
                qty: Qty(1 + (r >> 8) as i64 % 9),
                aggressor: Some(if r >> 20 & 1 == 0 {
                    Aggressor::Buy
                } else {
                    Aggressor::Sell
                }),
            }
        })
        .collect()
}

fn book_apply(c: &mut Criterion) {
    // A 2,000-level book each side, then single-level updates near the touch (the common
    // depth message), including removals and re-adds.
    let mut book = OrderBook::default();
    for k in 0..2_000i64 {
        book.set(Side::Bid, Price(600_000 - k * TICK), Qty(1 + k % 37), 0);
        book.set(Side::Ask, Price(600_025 + k * TICK), Qty(1 + k % 41), 0);
    }
    let mut rng = Rng(7);
    let updates: Vec<(Side, Price, Qty)> = (0..TRADES)
        .map(|_| {
            let r = rng.next();
            let side = if r & 1 == 0 { Side::Bid } else { Side::Ask };
            let depth = (r >> 4) as i64 % 20;
            let price = match side {
                Side::Bid => 600_000 - depth * TICK,
                Side::Ask => 600_025 + depth * TICK,
            };
            (side, Price(price), Qty((r >> 12) as i64 % 50))
        })
        .collect();
    let mut g = c.benchmark_group("book");
    g.throughput(Throughput::Elements(updates.len() as u64));
    g.bench_function("apply_level_2000_deep", |b| {
        b.iter(|| {
            for &(side, price, qty) in &updates {
                book.set(side, price, qty, 1);
            }
            black_box((book.best_bid(), book.best_ask(), book.is_crossed()));
        })
    });
    g.finish();
}

fn bars(c: &mut Criterion) {
    let cal = es();
    let ts = trades(TRADES, START, 11);
    let mut g = c.benchmark_group("bars");
    g.throughput(Throughput::Elements(ts.len() as u64));
    // Live trades into 1-minute bars through the history stitch (what the store does).
    g.bench_function("stitch_1m", |b| {
        b.iter_batched_ref(
            || {
                let mut s = Stitcher::new(60_000, 500_000);
                s.begin_connection(false);
                (s, BarSeries::new(60_000, 20_000))
            },
            |(s, series)| {
                for t in &ts {
                    s.trade(series, t.time, t.price, t.qty);
                }
            },
            BatchSize::LargeInput,
        )
    });
    // The same, plus a 5-minute chart brought up to date after every trade (the chart's
    // per-frame `Derived::update`, at its worst: one update per trade).
    g.bench_function("stitch_1m_plus_derived_5m", |b| {
        b.iter_batched_ref(
            || {
                let mut s = Stitcher::new(60_000, 500_000);
                s.begin_connection(false);
                (
                    s,
                    BarSeries::new(60_000, 20_000),
                    Derived::new(Timeframe::Minutes(5), 20_000),
                )
            },
            |(s, series, d)| {
                for t in &ts {
                    s.trade(series, t.time, t.price, t.qty);
                    d.update(series, Some(cal));
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

fn profiles(c: &mut Criterion) {
    // ETH + RTH session profiles and the 1-minute footprint, per counted trade.
    let cal = es();
    let ts = trades(TRADES, START, 13);
    let mut g = c.benchmark_group("profile");
    g.throughput(Throughput::Elements(ts.len() as u64));
    g.bench_function("sessions_and_footprint_add", |b| {
        b.iter_batched_ref(
            || SessionProfiles::new(Some(cal), 60_000, 20_000),
            |p| {
                for t in &ts {
                    p.add(t.time, t.price, t.qty, t.aggressor);
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.bench_function("vap_add", |b| {
        b.iter_batched_ref(
            VolumeAtPrice::default,
            |v| {
                for t in &ts {
                    v.add(t.price, t.qty);
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

fn tick_builders(c: &mut Criterion) {
    let cal = es();
    let ts = trades(TRADES, START, 17);
    let scale = BarScale {
        tick: TICK,
        qty_unit: 1,
    };
    let mut g = c.benchmark_group("tick_bars");
    g.throughput(Throughput::Elements(ts.len() as u64));
    for (name, kind) in [
        ("ticks_500", BarKind::Ticks(500)),
        ("volume_500", BarKind::Volume(500)),
        ("range_10", BarKind::Range(10)),
        ("seconds_30", BarKind::Seconds(30)),
    ] {
        g.bench_function(name, |b| {
            b.iter_batched_ref(
                || TickBars::with_kind(kind, TickReset::Session, Some(cal), scale, 20_000),
                |tb| {
                    for t in &ts {
                        tb.push(t.time, t.price, t.qty, t.aggressor);
                    }
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

/// One chart frame's CPU work, without GPUI: the viewport follows the data, candle and grid
/// geometry, axis labels, a crosshair readout, the footprint cells of the visible bars, the
/// visible-range volume profile, and a DOM ladder beside it. 1920x1000 px, 1-minute bars.
fn chart_frame(c: &mut Criterion) {
    let cal = es();
    let fmt = PriceFormat::new(
        PriceScale::decimal(2),
        DisplayStyle::Decimal { digits: 2 },
        TICK,
    );
    // Six hours of trades: bars, profiles/footprint, VAP and a book.
    let ts = trades(6 * 3600 * 20, START - 6 * 3_600_000, 19);
    let mut stitch = Stitcher::new(60_000, 500_000);
    stitch.begin_connection(false);
    let mut series = BarSeries::new(60_000, 20_000);
    let mut profiles = SessionProfiles::new(Some(cal), 60_000, 20_000);
    let mut vap = VolumeAtPrice::default();
    for t in &ts {
        stitch.trade(&mut series, t.time, t.price, t.qty);
        profiles.add(t.time, t.price, t.qty, t.aggressor);
        vap.add(t.price, t.qty);
    }
    let last = ts.last().unwrap().price;
    let mut book = OrderBook::default();
    for k in 0..500i64 {
        book.set(Side::Bid, Price(last.0 - k * TICK), Qty(1 + k % 37), 0);
        book.set(Side::Ask, Price(last.0 + (k + 1) * TICK), Qty(1 + k % 41), 0);
    }

    for (name, width, bar_px) in [("chart_1m_wide", 1920.0, 8.0), ("chart_1m_footprint", 1920.0, 60.0)] {
        let mut vp = Viewport::default();
        vp.set_size(width, 1000.0);
        vp.bar_px = bar_px;
        vp.right_index = series.len() as f64 - 1.0 + vp.live_margin;
        let mut scene = Scene::with_capacity(0);
        scene.ensure_capacity(vp.width);
        let mut fp = FootprintFrame::with_capacity(512, 32_768, 1_024);
        let mut rows = ProfileRows::with_capacity(4_096);
        let mut ladder = Vec::with_capacity(256);
        let mut lview = LadderView::default();
        let mut label = FixedStr::<32>::new();
        let mut readout = FixedStr::<128>::new();
        let mut frame = 0u64;
        c.bench_function(&format!("frame/{name}"), |b| {
            b.iter(|| {
                frame += 1;
                vp.on_data(series.bars(), TICK);
                scene.build(series.bars(), &vp, &fmt, 1.25, series.period_ms());
                for line in &scene.price_lines {
                    label.clear();
                    fmt.write(line.price, &mut label).unwrap();
                }
                for line in &scene.time_lines {
                    label.clear();
                    line.label.write(line.time, &mut label).unwrap();
                }
                let x = vp.width * 0.6 + (frame % 7) as f64;
                if let Some(ch) = crosshair(
                    series.bars(),
                    &vp,
                    TICK,
                    series.period_ms(),
                    1.25,
                    CrosshairMode::Normal,
                    x,
                    vp.height * 0.5,
                ) && let Some(i) = ch.bar
                {
                    readout.clear();
                    write_ohlcv(&series.bars()[i], &fmt, QtyFormat::WHOLE, &mut readout).unwrap();
                }
                let range = vp.visible_range(series.len());
                let view = BarsView::fixed(series.bars(), series.period_ms());
                fp.build(
                    profiles.footprint(),
                    view,
                    range.clone(),
                    TICK,
                    ImbalanceRule::default(),
                );
                let (from, to) = (
                    series.bars()[range.start].time,
                    UtcMillis(series.bars()[range.end - 1].time.0 + 60_000),
                );
                rows.build(TICK, 70, || profiles.footprint().range(from, to));
                let inside = inside_row(&book, Some(last), TICK);
                lview.follow(inside, 60, false);
                let top = lview.top_row(60).unwrap();
                let st = build_ladder(&book, &vap, Some(last), TICK, top, 60, &mut ladder);
                black_box((&scene, &fp, &rows, &ladder, st, &label, &readout));
            })
        });
    }
}

criterion_group!(
    benches,
    book_apply,
    bars,
    profiles,
    tick_builders,
    chart_frame
);
criterion_main!(benches);
