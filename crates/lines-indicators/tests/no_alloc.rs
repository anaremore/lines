//! The VWAP chart geometry runs in paint: it must not allocate (counting allocator).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lines_core::{Bar, Price, Qty, UtcMillis, Viewport};
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
    let before = ALLOCS.with(|c| c.get());
    let mut drawn = 0;
    for frame in 0..500 {
        vp.right_index = 4_999.0 - (frame % 300) as f64;
        vp.bar_px = [2.0, 5.0, 9.5][frame % 3];
        build_columns(&points, &bars, &vp, Bands(0b111), &mut out);
        drawn += out.len();
    }
    let n = ALLOCS.with(|c| c.get()) - before;
    assert!(drawn > 0);
    assert_eq!(n, 0, "VWAP geometry allocated {n} times");
    // Columns break at reset periods and at bars without a value.
    build_columns(&points, &bars, &vp, Bands::NONE, &mut out);
    assert!(out.iter().any(|c| !c.joins));
    assert!(out.iter().any(|c| c.joins));
}
