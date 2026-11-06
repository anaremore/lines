//! Enforces "no heap allocation in the paint path" for everything Lines controls:
//! viewport updates, candle/grid geometry, and label formatting.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lines_core::{Bar, BarSeries, FixedStr, Price, PriceFormat, Scene, UtcMillis, Viewport};

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
    let fmt = PriceFormat::from_cqg(0.01, 2, 0.25).unwrap();
    let mut series = BarSeries::new(60_000, 20_000);
    for i in 0..10_000i64 {
        let p = 500_000 + (i * 37 % 400) * 25;
        series.upsert(Bar {
            time: UtcMillis(i * 60_000),
            open: Price(p),
            high: Price(p + 100),
            low: Price(p - 100),
            close: Price(p + 25),
            volume: 3,
        });
    }

    let mut vp = Viewport::default();
    vp.set_size(1920.0, 1000.0);
    let mut scene = Scene::with_capacity(0);
    scene.ensure_capacity(vp.width); // done on resize, outside the paint path

    let n = allocs_during(|| {
        for frame in 0..500 {
            // Feed side: a live trade into the forming bar.
            series.apply_trade(
                UtcMillis(9_999 * 60_000 + frame),
                Price(500_000 + frame % 8 * 25),
                1,
            );
            // Interaction.
            vp.zoom_at(
                900.0,
                if frame % 2 == 0 { 1.1 } else { 1.0 / 1.1 },
                series.len(),
            );
            vp.pan(if frame % 3 == 0 { -5.0 } else { 5.0 }, 0.0, series.len());
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
        }
    });
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
