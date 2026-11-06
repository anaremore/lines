//! Session queries run per tick (VWAP resets, daily bars, profiles): they must not allocate.
//! Construction (loading the zone, cloning the holiday list) may.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lines_core::sessions::{ExchangeHours, Hours};
use lines_core::{
    CivilDate, HolidayTable, LocalTime, SessionCalendar, SessionKind, SessionSpec, UtcMillis,
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
fn session_queries_do_not_allocate() {
    let table = HolidayTable::parse(
        "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2027 verified\nsource synthetic\n\
         2026-01-19 halt 12:00 verified MLK\n2026-11-27 early 12:15 verified Black Friday\n\
         2026-12-25 closed verified Christmas\n",
    )
    .unwrap();
    let equity = SessionCalendar::new(
        SessionSpec {
            time_zone: "America/Chicago",
            hours: Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(17, 0),
                eth_close: LocalTime::hm(16, 0),
                rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
            }),
            holidays: Some("c"),
        },
        Some(&table),
    )
    .unwrap();
    let crypto =
        SessionCalendar::new(SessionSpec::continuous_utc(LocalTime::MIDNIGHT), None).unwrap();
    let start = UtcMillis::parse_iso("2026-01-01T00:00:00Z").unwrap();
    // Warm up any lazily initialised state in the time-zone library.
    for cal in [&equity, &crypto] {
        let _ = cal.session_at(start, SessionKind::Eth);
    }
    let mut sink = 0i64;
    let n = allocs_during(|| {
        for cal in [&equity, &crypto] {
            // Every 7 minutes through 2026, DST changes and holidays included.
            let mut t = start;
            while t.0 < start.0 + 366 * 86_400_000 {
                for kind in [SessionKind::Eth, SessionKind::Rth] {
                    if let Some(s) = cal.session_at(t, kind) {
                        sink += s.open.0;
                    }
                    if let Some(s) = cal.next_session(t, kind) {
                        sink += s.close.0;
                    }
                }
                sink += cal.is_open(t) as i64;
                sink += cal.next_open(t).map_or(0, |x| x.0);
                sink += cal.next_close(t).map_or(0, |x| x.0);
                t.0 += 7 * 60_000;
            }
            let to = UtcMillis(start.0 + 366 * 86_400_000);
            sink += cal.sessions(start, to, SessionKind::Eth).count() as i64;
            sink += cal
                .session(CivilDate::new(2026, 11, 27).unwrap(), SessionKind::Eth)
                .map_or(0, |s| s.close.0);
        }
    });
    assert_eq!(n, 0, "session queries allocated {n} times");
    assert_ne!(sink, 0);
}
