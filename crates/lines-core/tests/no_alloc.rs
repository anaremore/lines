//! Enforces "no heap allocation in the paint path" for everything Lines controls:
//! viewport updates (pan, wheel zoom, drag scaling), candle/grid geometry, crosshair hit-testing, and label formatting
//! (axis labels, crosshair price/time tags, OHLCV readout).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lines_core::geometry::{CrosshairMode, crosshair, write_ohlcv};
use lines_core::viewport::{ScaleDrag, ScaleMode};
use lines_core::{
    Bar, BarSeries, DisplayStyle, FixedStr, Price, PriceFormat, PriceScale, Qty, QtyFormat, Scene,
    UtcMillis, Viewport,
};

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

fn allocs_during(f: impl FnOnce()) -> usize {
    let before = ALLOCS.with(|c| c.get());
    f();
    ALLOCS.with(|c| c.get()) - before
}

#[test]
fn paint_path_does_not_allocate() {
    let fmt = PriceFormat::new(
        PriceScale::decimal(2),
        DisplayStyle::Decimal { digits: 2 },
        25,
    );
    let mut series = BarSeries::new(60_000, 20_000);
    for i in 0..10_000i64 {
        let p = 500_000 + (i * 37 % 400) * 25;
        series.upsert(Bar {
            time: UtcMillis(i * 60_000),
            open: Price(p),
            high: Price(p + 100),
            low: Price(p - 100),
            close: Price(p + 25),
            volume: Qty(3),
        });
    }

    let mut vp = Viewport::default();
    vp.set_size(1920.0, 1000.0);
    let mut scene = Scene::with_capacity(0);
    scene.ensure_capacity(vp.width); // done on resize, outside the paint path

    let (mut hits, mut bar_hits) = (0usize, 0usize);
    let n = allocs_during(|| {
        for frame in 0..500 {
            // Feed side: a live trade into the forming bar.
            series.apply_trade(
                UtcMillis(9_999 * 60_000 + frame),
                Price(500_000 + frame % 8 * 25),
                Qty(1),
            );
            // Interaction.
            vp.zoom_at(
                900.0,
                if frame % 2 == 0 { 1.1 } else { 1.0 / 1.1 },
                series.len(),
            );
            vp.pan(if frame % 3 == 0 { -5.0 } else { 5.0 }, 0.0, series.len());
            // Wheel zoom pinned to the right edge, drag scaling, auto-scale restore.
            let anchor = vp.time_zoom_anchor(700.0, series.len());
            vp.zoom_at(
                anchor,
                if frame % 2 == 0 { 1.05 } else { 1.0 / 1.05 },
                series.len(),
            );
            let mode = [ScaleMode::Free, ScaleMode::Time, ScaleMode::Price][frame as usize % 3];
            let mut drag = ScaleDrag::new(&vp, mode, 600.0, 300.0);
            for step in 0..4 {
                let d = (frame % 7 + step * 3) as f64;
                drag.update(&mut vp, 600.0 + d, 300.0 - d * 0.5, series.len(), fmt.tick);
            }
            if frame % 50 == 0 {
                vp.restore_auto_price();
            }
            // Frame.
            vp.on_data(series.bars(), fmt.tick);
            scene.build(series.bars(), &vp, &fmt, 1.25, series.period_ms());
            let mut label = FixedStr::<32>::new();
            for line in &scene.price_lines {
                label.clear();
                fmt.write(line.price, &mut label).unwrap();
            }
            for line in &scene.time_lines {
                label.clear();
                line.label.write(line.time, &mut label).unwrap();
            }
            std::hint::black_box(&label);
            // Crosshair: sweep the pointer across the plot (including outside it and past
            // both ends of the data) at fractional and integer DPI scales.
            let mut readout = FixedStr::<128>::new();
            for k in 0..40 {
                let x = -50.0 + k as f64 * (vp.width + 100.0) / 40.0;
                let y = -20.0 + k as f64 * (vp.height + 40.0) / 40.0;
                let scale = [1.0, 1.5, 2.0][k % 3];
                let bars = series.bars();
                let Some(ch) = crosshair(
                    bars,
                    &vp,
                    fmt.tick,
                    series.period_ms(),
                    scale,
                    [CrosshairMode::Normal, CrosshairMode::Magnet][k % 2],
                    x,
                    y,
                ) else {
                    continue;
                };
                hits += 1;
                label.clear();
                fmt.write(ch.price, &mut label).unwrap();
                if let Some(t) = ch.time {
                    label.clear();
                    t.write_crosshair_label(series.period_ms(), &mut label)
                        .unwrap();
                }
                if let Some(i) = ch.bar {
                    bar_hits += 1;
                    readout.clear();
                    write_ohlcv(&bars[i], &fmt, QtyFormat::WHOLE, &mut readout).unwrap();
                }
                std::hint::black_box((&ch, &label, &readout));
            }
        }
    });
    assert!(hits > 0 && bar_hits > 0, "crosshair never hit a bar");
    assert!(!scene.candles.is_empty());
    assert!(!scene.price_lines.is_empty());
    assert_eq!(n, 0, "paint path allocated {n} times");
}

#[test]
fn counter_detects_allocations() {
    let n = allocs_during(|| {
        std::hint::black_box(Box::new([0u8; 64]));
    });
    assert!(n >= 1, "allocation counter is not working");
}

/// The DOM and time & sales paint paths: ladder rows (grouping, centring, volume), the tape's
/// visible rows, and their number/time formatting. The book, tape and volume are set up first
/// (that is the feed side); every frame after that must not allocate.
#[test]
fn dom_and_tape_paint_paths_do_not_allocate() {
    use lines_core::ladder::{LadderView, build_ladder, inside_row};
    use lines_core::tape::write_tape_time;
    use lines_core::{Aggressor, OrderBook, Print, Side, Tape, VolumeAtPrice};

    let fmt = PriceFormat::new(
        PriceScale::decimal(2),
        DisplayStyle::Decimal { digits: 2 },
        25,
    );
    let qf = QtyFormat { decimals: 8 };
    let mut book = OrderBook::default();
    let mut vap = VolumeAtPrice::default();
    let mut tape = Tape::new(4096);
    for k in 0..2_000i64 {
        book.set(Side::Bid, Price(600_000 - k * 25), Qty(1 + k % 37), 0);
        book.set(Side::Ask, Price(600_025 + k * 25), Qty(1 + k % 41), 0);
        vap.add(Price(600_000 + (k % 80 - 40) * 25), Qty(k + 1));
    }
    for i in 0..5_000i64 {
        tape.push(Print {
            time: UtcMillis(i),
            price: Price(600_000),
            qty: Qty(i % 9 + 1),
            aggressor: Some(Aggressor::Buy),
        });
    }
    let mut rows = Vec::with_capacity(256);
    let mut visible = Vec::with_capacity(256);
    let mut view = LadderView::default();
    let mut s = FixedStr::<48>::new();

    let n = allocs_during(|| {
        for frame in 0..500i64 {
            // Feed-side mutations of existing levels (no new map nodes) between frames.
            book.set(
                Side::Bid,
                Price(600_000 - (frame % 50) * 25),
                Qty(frame + 1),
                0,
            );
            let grouping = [1i64, 2, 5, 10, 25][frame as usize % 5];
            let units = grouping * 25;
            let n_rows = 40 + (frame as usize % 200);
            let inside = inside_row(&book, Some(Price(600_000)), units);
            view.follow(inside, n_rows, frame % 3 == 0);
            if frame % 7 == 0 {
                view.scroll(if frame % 2 == 0 { 3 } else { -3 });
            }
            let top = view.top_row(n_rows).unwrap();
            let st = build_ladder(
                &book,
                &vap,
                Some(Price(600_000)),
                units,
                top,
                n_rows,
                &mut rows,
            );
            for r in &rows {
                for q in [r.bid, r.ask, r.volume] {
                    s.clear();
                    qf.write(q, &mut s).unwrap();
                }
                s.clear();
                fmt.write(r.price, &mut s).unwrap();
            }
            visible.clear();
            for p in tape.iter_newest().take(n_rows) {
                visible.push(*p);
            }
            for p in &visible {
                s.clear();
                write_tape_time(p.time, &mut s).unwrap();
            }
            std::hint::black_box((&st, &s, &visible));
        }
    });
    assert!(!rows.is_empty() && !visible.is_empty());
    assert_eq!(n, 0, "DOM/T&S paint path allocated {n} times");
}

/// The volume profile's per-frame work: grouping a session, a composite of sessions and a
/// visible range of bars into rows with POC and value area, into a reused buffer.
#[test]
fn volume_profile_rows_do_not_allocate() {
    use lines_core::profile::{Footprint, PriceVolume};
    use lines_core::{Aggressor, ProfileRows};

    let mut sessions: Vec<PriceVolume> = (0..5).map(|_| PriceVolume::default()).collect();
    let mut fp = Footprint::new(60_000, 10_000);
    for k in 0..50_000i64 {
        let price = Price(600_000 + (k * 37 % 400 - 200) * 25);
        let side = if k % 2 == 0 {
            Some(Aggressor::Buy)
        } else {
            Some(Aggressor::Sell)
        };
        sessions[(k % 5) as usize].add(price, Qty(k % 7 + 1), side);
        fp.add(UtcMillis(k * 1_000), price, Qty(1), side);
    }
    let mut rows = ProfileRows::with_capacity(4096);
    // Warm up (the buffer is already large enough; this only exercises the code once).
    rows.build(25, 70, || sessions[4].iter());
    let n = allocs_during(|| {
        for g in [1i64, 2, 5, 10, 25] {
            rows.build(g * 25, 70, || sessions[4].iter());
            assert!(rows.poc.is_some());
            rows.build(g * 25, 68, || sessions.iter().flat_map(|s| s.iter()));
            assert!(rows.value_area.is_some());
            rows.build(g * 25, 70, || {
                fp.range(UtcMillis(1_000_000), UtcMillis(20_000_000))
            });
            assert!(rows.total.0 > 0);
        }
    });
    assert_eq!(n, 0, "volume profile rows allocated {n} times");
}

#[test]
fn column_layout_and_resizing_do_not_allocate() {
    use lines_core::columns::{Cell, ColumnSet, ColumnSpec, MAX_COLS};
    let spec = |min_w, default_w, hideable| ColumnSpec {
        min_w,
        default_w,
        hideable,
        shown_by_default: true,
    };
    let specs = [
        spec(16.0, 22.0, true),
        spec(36.0, 64.0, true),
        spec(48.0, 76.0, false),
        spec(36.0, 64.0, true),
        spec(36.0, 64.0, true),
    ];
    let mut set = ColumnSet::new(&specs, 4);
    let mut cells = [Cell::default(); MAX_COLS];
    let mut needed = [0.0f32; MAX_COLS];
    let n = allocs_during(|| {
        // What a DOM does every frame while the trader drags a divider over live data.
        for frame in 0..10_000 {
            needed[2] = 60.0 + (frame % 7) as f32;
            needed[1] = 30.0 + (frame % 11) as f32;
            set.layout(&specs, &needed, 420.0, &mut cells);
            if let Some(d) = set.divider_at(&cells, cells[1].right() + 1.0, 3.0) {
                set.drag(
                    &specs,
                    &cells,
                    d,
                    if frame % 2 == 0 { 3.0 } else { -2.0 },
                    &needed,
                );
            }
            if frame % 500 == 0 {
                set.auto_fit(&specs, 3, &needed);
            }
            std::hint::black_box(&cells);
        }
    });
    assert_eq!(n, 0, "column layout allocated {n} times");
}

/// External levels (decision 0022): placing a map's levels and their labels each frame.
#[test]
fn level_placement_does_not_allocate() {
    use lines_core::ProductId;
    use lines_core::levels::{LevelMap, LevelStyle, LevelValue, place, place_labels};

    let mut b = LevelMap::builder("demo", ProductId(1), UtcMillis(0));
    for k in 0..200i64 {
        let p = 600_000 + (k * 37 % 400 - 200) * 25;
        b = if k % 3 == 0 {
            b.zone(
                "zone",
                LevelValue::Tick(Price(p)),
                LevelValue::Tick(Price(p + 100)),
                Some("Z"),
                LevelStyle::default(),
            )
        } else {
            b.line(
                "line",
                LevelValue::Raw(p as f64 + 0.4),
                Some("L"),
                LevelStyle::default(),
            )
        };
    }
    let map = b.build(25).unwrap();
    let mut vp = Viewport::default();
    vp.set_size(1200.0, 700.0);
    (vp.price_lo, vp.price_hi) = (596_000.0, 604_000.0);
    let mut out = Vec::with_capacity(1024);
    place(&map, &vp, Some(600_000.0), |_| true, &mut out); // warm
    let n = allocs_during(|| {
        for i in 0..50 {
            vp.price_hi = 604_000.0 + i as f64 * 25.0;
            place(
                &map,
                &vp,
                Some(600_000.0),
                |k| k != 1 || i % 2 == 0,
                &mut out,
            );
            place_labels(&mut out, &map, 17.6, 700.0, Some(350.0));
        }
    });
    assert_eq!(n, 0, "level placement allocated {n} times");
    assert!(!out.is_empty());
}

/// The DOM price trail (`lines_core::trail`): taking trades (including recentring the ring
/// and far jumps), decaying, re-targeting the scale, reading cells for every visible row and
/// the animation check all run without allocating once the trail exists.
#[test]
fn dom_trail_does_not_allocate() {
    use lines_core::tape::Aggressor;
    use lines_core::trail::{DEFAULT_HALF_LIFE_MS, Trail};
    let mut trail = Trail::new(25, DEFAULT_HALF_LIFE_MS);
    let n = allocs_during(|| {
        let mut sum = 0.0f32;
        for i in 0..20_000i64 {
            let ticks = (i * 7 % 41) - 20 + if i % 5_000 == 0 { 2_000 } else { 0 };
            let side = match i % 3 {
                0 => Some(Aggressor::Buy),
                1 => Some(Aggressor::Sell),
                _ => None,
            };
            trail.trade(
                UtcMillis(i * 3),
                Price(600_000 + ticks * 25),
                Qty(1 + i % 4),
                side,
            );
            if i % 10 == 0 {
                trail.advance(UtcMillis(i * 3 + 1));
                let row = trail.row_of(Price(600_000));
                for r in row - 64..row + 64 {
                    let c = trail.cell(r);
                    sum += c.alpha + c.net;
                }
                let _ = trail.newest();
                let _ = trail.needs_animation(UtcMillis(i * 3 + 2));
            }
        }
        trail.clear(50);
        std::hint::black_box(sum);
    });
    assert_eq!(n, 0, "the trail allocated {n} times");
}
