//! The VWAP and cumulative delta chart geometry runs in paint: it must not allocate
//! (counting allocator).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lines_core::{Bar, BarSeries, BarsView, Derived, Price, Qty, Timeframe, UtcMillis, Viewport};
use lines_indicators::render::{Column, build_columns};
use lines_indicators::{Bands, Point};

struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn vwap_columns_do_not_allocate() {
    let bars: Vec<Bar> = (0..5_000i64)
        .map(|i| Bar {
            time: UtcMillis(i * 60_000),
            open: Price(600_000),
            high: Price(600_100),
            low: Price(599_900),
            close: Price(600_000 + i % 7),
            volume: Qty(10),
        })
        .collect();
    let points: Vec<Point> = bars
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 13 != 0) // some bars have no value (e.g. outside RTH)
        .map(|(i, b)| Point {
            time: b.time,
            vwap: 600_000.0 + (i % 50) as f64,
            sd: 12.5,
            approx: i < 100,
            period: (i / 1380) as i64,
            forming: false,
        })
        .collect();
    let mut vp = Viewport::default();
    vp.set_size(1600.0, 900.0);
    vp.price_lo = 599_000.0;
    vp.price_hi = 601_000.0;
    let mut out: Vec<Column> = Vec::with_capacity(4096);
    let view = BarsView::fixed(&bars, 60_000);
    // A higher timeframe samples the bucket's last point (roadmap 9): no allocation either.
    let mut series = BarSeries::new(60_000, 10_000);
    for b in &bars {
        series.upsert(*b);
    }
    let mut derived = Derived::new(Timeframe::Minutes(15), series.capacity());
    derived.update(&series, None);
    let before = ALLOCS.with(|c| c.get());
    for frame in 0..100 {
        vp.right_index = 330.0 - (frame % 30) as f64;
        build_columns(&points, derived.view(), &vp, Bands(0b111), &mut out);
    }
    assert_eq!(
        ALLOCS.with(|c| c.get()) - before,
        0,
        "15m VWAP geometry allocated"
    );
    let before = ALLOCS.with(|c| c.get());
    let mut drawn = 0;
    for frame in 0..500 {
        vp.right_index = 4_999.0 - (frame % 300) as f64;
        vp.bar_px = [2.0, 5.0, 9.5][frame % 3];
        build_columns(&points, view, &vp, Bands(0b111), &mut out);
        drawn += out.len();
    }
    let n = ALLOCS.with(|c| c.get()) - before;
    assert!(drawn > 0);
    assert_eq!(n, 0, "VWAP geometry allocated {n} times");
    // Columns break at reset periods and at bars without a value.
    build_columns(&points, view, &vp, Bands::NONE, &mut out);
    assert!(out.iter().any(|c| !c.joins));
    assert!(out.iter().any(|c| c.joins));
}

/// Cumulative delta candles (roadmap 10e): built per frame for the visible bars, on 1-minute
/// and higher timeframes, from the engine's points (the engine itself runs on its own thread,
/// not in paint).
#[test]
fn delta_candles_do_not_allocate() {
    use lines_core::Aggressor;
    use lines_indicators::render::{DeltaCandle, build_delta_candles};
    use lines_indicators::{CumDelta, DeltaReset, DeltaSpec, Input, MarketSeries};

    let mut base = MarketSeries::new(60_000, 10_000);
    base.apply(&[
        Input::Connect {
            expect_history: false,
        },
        Input::TradesFrom(UtcMillis(0)),
    ]);
    let trades: Vec<Input> = (0..20_000i64)
        .map(|k| Input::Trade {
            time: UtcMillis(k * 1_000),
            price: Price(600_000 + (k % 40) * 25),
            qty: Qty(k % 5 + 1),
            aggressor: Some(if k % 3 == 0 {
                Aggressor::Sell
            } else {
                Aggressor::Buy
            }),
        })
        .collect();
    base.apply(&trades);
    let mut delta = CumDelta::new(
        DeltaSpec {
            reset: DeltaReset::None,
        },
        60_000,
        None,
    );
    delta.update(&base, UtcMillis(i64::MIN));
    let bars: Vec<Bar> = base.buckets().iter().map(|b| b.bar).collect();
    let mut series = BarSeries::new(60_000, 10_000);
    for b in &bars {
        series.upsert(*b);
    }
    let mut derived = Derived::new(Timeframe::Minutes(5), series.capacity());
    derived.update(&series, None);
    let view = BarsView::fixed(&bars, 60_000);
    let mut vp = Viewport::default();
    vp.set_size(1600.0, 900.0);
    let mut out: Vec<DeltaCandle> = Vec::with_capacity(4096);
    let before = ALLOCS.with(|c| c.get());
    let mut drawn = 0;
    for frame in 0..300usize {
        vp.right_index = 330.0 - (frame % 30) as f64;
        vp.bar_px = [3.0, 8.0, 20.0][frame % 3];
        let v = if frame % 2 == 0 { view } else { derived.view() };
        build_delta_candles(delta.points(), v, vp.visible_range(v.len()), &mut out);
        drawn += out.len();
    }
    let n = ALLOCS.with(|c| c.get()) - before;
    assert!(drawn > 0);
    assert_eq!(n, 0, "delta geometry allocated {n} times");
}

/// Averages and Bollinger Bands (roadmap 10a): one point per chart bar, paired by start time;
/// trade-built bars that share a start millisecond pair one to one. No allocation in paint.
#[test]
fn bar_study_columns_do_not_allocate() {
    use lines_indicators::render::build_bar_columns;
    // Pairs of bars sharing a start time, as fast tick bars can.
    let bars: Vec<Bar> = (0..6_000i64)
        .map(|i| Bar {
            time: UtcMillis((i / 2) * 1_000),
            open: Price(600_000),
            high: Price(600_100),
            low: Price(599_900),
            close: Price(600_000 + i % 9),
            volume: Qty(5),
        })
        .collect();
    let points: Vec<Point> = bars
        .iter()
        .enumerate()
        .skip(19) // the window fills at the 20th bar
        .map(|(i, b)| Point {
            time: b.time,
            vwap: 600_000.0 + i as f64,
            sd: 40.0,
            approx: false,
            period: 0,
            forming: false,
        })
        .collect();
    let mut vp = Viewport::default();
    vp.set_size(1600.0, 900.0);
    vp.price_lo = 590_000.0;
    vp.price_hi = 610_000.0;
    let view = BarsView::fixed(&bars, 1_000);
    let mut out: Vec<Column> = Vec::with_capacity(4096);
    let before = ALLOCS.with(|c| c.get());
    let mut drawn = 0;
    for frame in 0..300 {
        vp.right_index = 5_999.0 - (frame % 200) as f64;
        vp.bar_px = [2.0, 5.0, 9.5][frame % 3];
        build_bar_columns(&points, view, &vp, Bands(0b001), &mut out);
        drawn += out.len();
    }
    assert_eq!(
        ALLOCS.with(|c| c.get()) - before,
        0,
        "bar study geometry allocated"
    );
    assert!(drawn > 0);
    // Each column is its own bar's point (bars i and i + 1 share a time, values differ).
    vp.right_index = 40.0;
    vp.bar_px = 10.0;
    build_bar_columns(&points, view, &vp, Bands::NONE, &mut out);
    let range = vp.visible_range(bars.len());
    let first = range.start.max(19);
    assert_eq!(out.len(), range.end - first);
    for (c, i) in out.iter().zip(first..) {
        assert_eq!(c.y, vp.price_to_y(600_000.0 + i as f64) as f32, "bar {i}");
    }
    let joins: Vec<bool> = out.iter().map(|c| c.joins).collect();
    assert!(joins[1..].iter().all(|j| *j), "{joins:?} {range:?}");
}
