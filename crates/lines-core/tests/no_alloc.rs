//! Enforces "no heap allocation in the paint path" for everything Lines controls:
//! viewport updates (pan, wheel zoom, drag scaling), candle/grid geometry, crosshair hit-testing, and label formatting
//! (axis labels, crosshair price/time tags, OHLCV readout).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lines_core::geometry::{crosshair, write_ohlcv};
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
                line.time.write_axis_label(&mut label).unwrap();
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
                let Some(ch) = crosshair(bars, &vp, fmt.tick, series.period_ms(), scale, x, y)
                else {
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
