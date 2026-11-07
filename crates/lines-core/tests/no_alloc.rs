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

/// The footprint chart (roadmap 10e) builds every visible bar's cells each frame, on 1-minute
/// and higher timeframes (the merge of the bucket's minutes), with imbalances and POCs.
#[test]
fn footprint_cells_do_not_allocate() {
    use lines_core::footprint::{FootprintFrame, ImbalanceRule, fit_grouping};
    use lines_core::profile::Footprint;
    use lines_core::{Aggressor, Derived, Timeframe};

    let mut fp = Footprint::new(60_000, 10_000);
    let mut series = BarSeries::new(60_000, 10_000);
    for k in 0..120_000i64 {
        let price = Price(600_000 + (k * 37 % 120 - 60) * 25);
        let side = match k % 5 {
            0 => None,
            1 | 3 => Some(Aggressor::Buy),
            _ => Some(Aggressor::Sell),
        };
        let t = UtcMillis(k * 250);
        fp.add(t, price, Qty(k % 7 + 1), side);
        series.apply_trade(t, price, Qty(k % 7 + 1));
    }
    let mut derived = Derived::new(Timeframe::Minutes(15), series.capacity());
    derived.update(&series, None);
    let mut frame = FootprintFrame::with_capacity(512, 60_000, 2_048);
    let mut vp = Viewport::default();
    vp.set_size(1600.0, 900.0);
    vp.price_lo = 598_000.0;
    vp.price_hi = 602_000.0;
    vp.bar_px = 40.0;
    let rule = ImbalanceRule::default();
    let view1 = lines_core::BarsView::fixed(series.bars(), 60_000);
    // Warm up once (the buffers are reserved up front; this only runs the code).
    frame.build(&fp, view1, vp.visible_range(view1.len()), 25, rule);
    let mut cells = 0;
    let n = allocs_during(|| {
        for f in 0..200usize {
            vp.right_index = 499.0 - (f % 40) as f64;
            let g = fit_grouping(1, 25, 0.1 + (f % 4) as f64 * 0.1, 9.0);
            let view = if f % 2 == 0 { view1 } else { derived.view() };
            frame.build(&fp, view, vp.visible_range(view.len()), g as i64 * 25, rule);
            cells += frame.cells.len();
        }
    });
    assert!(cells > 0);
    assert_eq!(n, 0, "footprint cells allocated {n} times");
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

/// The DOM price trail (`lines_core::trail`): taking trades (including recentering the ring
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

#[test]
fn timeframe_updates_do_not_allocate() {
    use lines_core::sessions::{ExchangeHours, Hours, SessionSpec};
    use lines_core::{Derived, LocalTime, SessionCalendar, Timeframe};
    let cal = SessionCalendar::new(
        SessionSpec {
            time_zone: "America/Chicago",
            hours: Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(17, 0),
                eth_close: LocalTime::hm(16, 0),
                rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
            }),
            holidays: None,
        },
        None,
    )
    .unwrap();
    let start = UtcMillis::parse_iso("2026-11-09T23:00:00Z").unwrap().0;
    let mut series = BarSeries::new(UtcMillis::MINUTE, 2_000);
    let mut derived: Vec<Derived> = Timeframe::ALL
        .iter()
        .filter(|tf| tf.source() == lines_core::timeframe::BarSource::OneMinute)
        .map(|&tf| Derived::new(tf, series.capacity()))
        .collect();
    let tick = |s: &mut BarSeries, i: i64| {
        let at = UtcMillis(start + i * 20_000);
        s.apply_trade(at, Price(500_000 + (i % 17) * 25), Qty(1));
    };
    for i in 0..3_000 {
        tick(&mut series, i);
    }
    for d in &mut derived {
        d.update(&series, Some(&cal));
    }
    // Ticks (the forming bucket), a late history bar and a trim: none allocate.
    let n = allocs_during(|| {
        for i in 3_000..12_000 {
            tick(&mut series, i);
            if i % 500 == 0 {
                let mut b = series.bars()[series.len() / 2];
                b.volume = Qty(99);
                series.upsert(b);
            }
            for d in &mut derived {
                d.update(&series, Some(&cal));
            }
        }
    });
    assert!(series.trimmed() > 0, "the test covers a trim");
    assert_eq!(n, 0, "timeframe updates allocated {n} times");
}

/// Tick bars (roadmap 9b): once warm (bars dropped at capacity give their price lists to new
/// bars), counting a trade into every preset, both reset modes, and reading the view never
/// allocate.
#[test]
fn tick_bar_updates_do_not_allocate() {
    use lines_core::sessions::{ExchangeHours, Hours, SessionSpec};
    use lines_core::ticks::TICK_PRESETS;
    use lines_core::{Aggressor, LocalTime, SessionCalendar, TickBars, TickReset};
    let cal: &'static SessionCalendar = Box::leak(Box::new(
        SessionCalendar::new(
            SessionSpec {
                time_zone: "America/Chicago",
                hours: Hours::Exchange(ExchangeHours {
                    eth_open: LocalTime::hm(17, 0),
                    eth_close: LocalTime::hm(16, 0),
                    rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
                }),
                holidays: None,
            },
            None,
        )
        .unwrap(),
    ));
    let start = UtcMillis::parse_iso("2026-11-09T23:00:00Z").unwrap().0;
    let mut bars: Vec<TickBars> = TICK_PRESETS
        .iter()
        .flat_map(|&n| {
            [TickReset::Session, TickReset::Continuous].map(|r| TickBars::new(n, r, Some(cal), 64))
        })
        .collect();
    // Roadmap 9b's other kinds through the same builder: volume, range (a wide one: more prices
    // per bar than a new bar reserves) and seconds bars.
    let scale = lines_core::BarScale {
        tick: 25,
        qty_unit: 1,
    };
    for kind in [
        lines_core::BarKind::Volume(500),
        lines_core::BarKind::Range(4),
        lines_core::BarKind::Range(40),
        lines_core::BarKind::Seconds(30),
        lines_core::BarKind::Seconds(3_600),
    ] {
        for r in [TickReset::Session, TickReset::Continuous] {
            bars.push(TickBars::with_kind(kind, r, Some(cal), scale, 64));
        }
    }
    // Trades every 40 s over 50 prices (a range bar of 40 ticks holds up to 41: more than a new
    // bar reserves), crossing session closes.
    let trade = |bars: &mut Vec<TickBars>, i: i64| {
        let at = UtcMillis(start + i * 40_000);
        let side = if i % 3 == 0 {
            Aggressor::Sell
        } else {
            Aggressor::Buy
        };
        for b in bars.iter_mut() {
            b.push(
                at,
                Price(500_000 + (i % 50) * 25),
                Qty(1 + i % 3),
                Some(side),
            );
        }
    };
    for i in 0..200_000 {
        trade(&mut bars, i);
    }
    let mut sum = 0usize;
    let n = allocs_during(|| {
        for i in 200_000..400_000 {
            trade(&mut bars, i);
            let v = bars[i as usize % bars.len()].view();
            sum += v.len() + v.slot(UtcMillis(start + i * 40_000)).unwrap_or(0) as usize;
        }
    });
    std::hint::black_box(sum);
    assert!(
        bars.iter().all(|b| b.trimmed() > 0),
        "the test covers trims"
    );
    // Session resets close bars short, per kind (a tick and a volume builder; range bars
    // this narrow complete before a reset can catch one: the unit tests cover theirs).
    for kind in [
        lines_core::BarKind::Ticks(TICK_PRESETS[0]),
        lines_core::BarKind::Volume(500),
    ] {
        assert!(
            bars.iter()
                .filter(|b| b.kind() == kind && b.reset() == TickReset::Session)
                .any(|b| (0..b.len()).any(|i| b.is_partial(i))),
            "{kind:?}: the test covers resets"
        );
    }
    assert_eq!(n, 0, "tick bar updates allocated {n} times");
}

/// Drawings (roadmap 11): projecting a product's drawings for a frame, hit-testing the
/// pointer, snapping an anchor and writing a measure's text never allocate.
/// Drawings (roadmap 11): projecting a product's drawings for a frame (styled: extended
/// lines and zones, decision 0026), hit-testing the pointer, snapping an anchor, walking a
/// line's dash pattern and writing a measure's text never allocate.
#[test]
fn drawings_do_not_allocate() {
    use lines_core::drawings::{
        Anchor, Drawing, MeasureParts, Style, TimeAxis, Tool, measure, pick, project, snap_anchor,
        write_measure,
    };
    use lines_core::instrument::Ratio;
    use lines_core::{BarsView, Derived, Timeframe};
    let fmt = PriceFormat::new(
        PriceScale::decimal(2),
        DisplayStyle::Decimal { digits: 2 },
        25,
    );
    let mut series = BarSeries::new(UtcMillis::MINUTE, 5_000);
    for i in 0..3_000i64 {
        series.apply_trade(
            UtcMillis(i * 20_000),
            Price(600_000 + (i % 37) * 25),
            Qty(1),
        );
    }
    let mut derived = Derived::new(Timeframe::Minutes(15), series.capacity());
    derived.update(&series, None);
    let drawings: Vec<Drawing> = (0..64i64)
        .map(|i| {
            let tool = Tool::ALL[(i % 8) as usize];
            let a = Anchor {
                time: UtcMillis(i * 600_000),
                price: Price(600_000 + i * 25),
            };
            let b = Anchor {
                time: UtcMillis(i * 600_000 + 3_600_000),
                price: Price(600_500 - i * 25),
            };
            let mut style = Style::DEFAULT;
            style.extend_left = i % 3 == 0;
            style.extend_right = i % 2 == 0;
            style.axis_tag = i % 5 == 0;
            Drawing {
                id: i as u64 + 1,
                ..Drawing::new(tool, a, b, style)
            }
        })
        .collect();
    let dash = lines_core::style::DashPattern {
        seg: [12.0, 4.0, 2.0, 4.0],
        n: 4,
    };
    let mut vp = Viewport::default();
    vp.set_size(1200.0, 700.0);
    vp.price_lo = 599_000.0;
    vp.price_hi = 602_000.0;
    let mut drawn = Vec::with_capacity(drawings.len());
    let n = allocs_during(|| {
        for frame in 0..300 {
            let one = BarsView::fixed(series.bars(), UtcMillis::MINUTE);
            let view = if frame % 2 == 0 { one } else { derived.view() };
            let axis = TimeAxis::new(view);
            vp.right_index = view.len() as f64 - 1.0 - (frame % 50) as f64;
            vp.bar_px = [3.0, 7.0][frame % 2];
            drawn.clear();
            for d in &drawings {
                if let Some(p) = project(d, &axis, &vp) {
                    drawn.push(p);
                }
            }
            let mut pieces = 0;
            for p in &drawn {
                let len = (p.p1.0 - p.p0.0).hypot(p.p1.1 - p.p0.1);
                dash.segments(len, |_, _| pieces += 1);
            }
            std::hint::black_box(pieces);
            let (x, y) = ((frame * 4) as f32, (frame * 2) as f32);
            let _ = pick(&drawn, Some(3), x, y, 4.0, 5.0);
            let a = snap_anchor(&axis, &vp, 25, frame % 3 == 0, x as f64, y as f64);
            if let Some(a) = a {
                let m = measure(drawings[0].a, a, &axis, &fmt, Ratio::new(50, 1));
                let mut s = FixedStr::<96>::new();
                let _ = write_measure(&m, &fmt, MeasureParts::ALL, &mut s);
            }
        }
    });
    assert!(!drawn.is_empty());
    assert_eq!(n, 0, "drawings allocated {n} times");
}

/// The latency HUD (roadmap 6b): recording a sample (feed thread, chart paint) and reading
/// p50/p99 and the sparkline never allocate once a window exists.
#[test]
fn latency_windows_do_not_allocate() {
    use lines_core::latency::{ClockOffset, LatencyWindow, WINDOW_S};
    let mut w = LatencyWindow::new();
    let mut c = ClockOffset::default();
    let mut spark = [0u64; WINDOW_S];
    let mut seen = 0u64;
    let n = allocs_during(|| {
        for i in 0..20_000i64 {
            w.record(i * 7, (i as u64 * 37) % 50_000);
            c.record(i * 7, i * 7 - 20);
        }
        let s = w.summary(140_000);
        w.sparkline(140_000, 0.99, &mut spark);
        seen = s.n + c.bound_ms(140_000).unwrap_or(0) as u64;
    });
    assert_eq!(n, 0, "latency windows allocated {n} times");
    assert!(seen > 0);
}

#[test]
fn chart_order_tags_do_not_allocate() {
    use lines_core::geometry::order_tags::{
        LineIn, MAX_TAGS, TagBox, edge_markers, handles, hit, layout_tags, tag_band,
    };
    use lines_core::geometry::snap_price;
    let vp = Viewport {
        width: 800.0,
        height: 400.0,
        price_lo: 590_000.0,
        price_hi: 610_000.0,
        ..Viewport::default()
    };
    // Orders on the plot and beyond both edges.
    let lines: Vec<LineIn> = (0..40)
        .map(|i| LineIn {
            id: i,
            price: Price(585_000 + (i as i64) * 700),
            text_w: 90.0,
        })
        .collect();
    let mut tags = [TagBox::default(); MAX_TAGS];
    let n = allocs_during(|| {
        for frame in 0..200 {
            let m = edge_markers(&vp, &lines, 800.0, 400.0, [80.0, 80.0]);
            let band = tag_band(&m, 400.0);
            let k = layout_tags(&vp, &lines, 800.0, 400.0, band, 1.0, &mut tags);
            let h = handles(&tags[0], [true, true]);
            let y = frame as f32 % 400.0;
            let _ = hit(&tags[..k], &m, &h, 700.0, y);
            let _ = snap_price(&vp, y as f64, 25);
        }
    });
    assert_eq!(n, 0, "chart order tags allocated {n} times");
}
