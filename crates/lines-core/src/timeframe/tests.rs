use super::*;
use crate::price::Price;
use crate::qty::Qty;
use crate::sessions::{ExchangeHours, HolidayTable, Hours, LocalTime, SessionSpec};

/// Synthetic CME-equity-like table (tests never depend on the shipped data file).
const TABLE: &str = "\
format 1
version 2026-11-06
calendar test-equity
coverage 2026 2027 verified
source synthetic
2026-01-19 halt 12:00  verified   Martin Luther King Jr. Day
2026-11-26 halt 12:00  verified   Thanksgiving
2026-11-27 early 12:15 verified   Day after Thanksgiving
";

fn cme() -> SessionCalendar {
    SessionCalendar::new(
        SessionSpec {
            time_zone: "America/Chicago",
            hours: Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(17, 0),
                eth_close: LocalTime::hm(16, 0),
                rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
            }),
            holidays: Some("test-equity"),
        },
        Some(&HolidayTable::parse(TABLE).unwrap()),
    )
    .unwrap()
}

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn bucket(tf: Timeframe, cal: Option<&SessionCalendar>, at: &str) -> (String, String, bool) {
    let b = Bucketer::new(tf, cal).bucket(t(at));
    (b.start.to_string(), b.end.to_string(), b.partial)
}

fn b(start: &str, end: &str, partial: bool) -> (String, String, bool) {
    (t(start).to_string(), t(end).to_string(), partial)
}

/// One trade per minute from `from` for `n` minutes, prices walking deterministically.
fn minutes(series: &mut BarSeries, from: UtcMillis, n: i64) {
    for i in 0..n {
        let p = 500_000 + (i * 37 % 23) * 25 - (i * 11 % 7) * 25;
        let at = UtcMillis(from.0 + i * MINUTE + 5_000);
        series.apply_trade(at, Price(p), Qty(1 + i % 4));
        series.apply_trade(UtcMillis(at.0 + 1_000), Price(p + 25), Qty(1));
    }
}

#[test]
fn parse_and_display() {
    for (s, tf) in [
        ("1", Timeframe::Minutes(1)),
        ("5", Timeframe::Minutes(5)),
        ("5m", Timeframe::Minutes(5)),
        ("15M", Timeframe::Minutes(15)),
        ("1h", Timeframe::Hours(1)),
        ("h", Timeframe::Hours(1)),
        ("4h", Timeframe::Hours(4)),
        ("d", Timeframe::Day),
        ("1D", Timeframe::Day),
        ("w", Timeframe::Week),
        ("1w", Timeframe::Week),
    ] {
        assert_eq!(Timeframe::parse(s), Some(tf), "{s}");
    }
    for bad in ["", "m", "7", "0", "3h", "2d", "5s", "99999999", "1x"] {
        assert_eq!(Timeframe::parse(bad), None, "{bad}");
    }
    let labels: Vec<String> = Timeframe::ALL.iter().map(|tf| tf.to_string()).collect();
    assert_eq!(
        labels,
        [
            "1m", "2m", "3m", "5m", "10m", "15m", "30m", "1h", "2h", "4h", "1D", "1W"
        ]
    );
    for tf in Timeframe::ALL {
        assert_eq!(
            Timeframe::parse(&tf.to_string()),
            Some(tf),
            "round trip {tf}"
        );
        assert_eq!(tf.source(), BarSource::OneMinute);
    }
}

#[test]
fn aggregation_is_exact() {
    let mut s = BarSeries::new(MINUTE, 10_000);
    let start = t("2026-11-10T00:00:00Z");
    minutes(&mut s, start, 60);
    let mut d = Derived::new(Timeframe::Minutes(5), s.capacity());
    assert!(d.update(&s, None));
    assert_eq!(d.bars().len(), 12);
    for (k, bar) in d.bars().iter().enumerate() {
        let parts = &s.bars()[k * 5..k * 5 + 5];
        assert_eq!(bar.time, parts[0].time);
        assert_eq!(bar.open, parts[0].open);
        assert_eq!(bar.close, parts[4].close);
        assert_eq!(bar.high, parts.iter().map(|b| b.high).max().unwrap());
        assert_eq!(bar.low, parts.iter().map(|b| b.low).min().unwrap());
        assert_eq!(
            bar.volume,
            Qty(parts.iter().map(|b| b.volume.0).sum::<i64>())
        );
        assert_eq!(d.view().end(k).0, bar.time.0 + 5 * MINUTE);
        assert!(!d.is_partial(k));
    }
    assert!(!d.update(&s, None), "nothing new: nothing to do");
}

#[test]
fn utc_alignment_without_a_calendar() {
    assert_eq!(
        bucket(Timeframe::Hours(4), None, "2026-11-10T13:07:00Z"),
        b("2026-11-10T12:00:00Z", "2026-11-10T16:00:00Z", false)
    );
    assert_eq!(
        bucket(Timeframe::Day, None, "2026-11-10T13:07:00Z"),
        b("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z", false)
    );
    // Thursday 2026-11-12 → the week from Monday 2026-11-09 00:00 UTC.
    assert_eq!(
        bucket(Timeframe::Week, None, "2026-11-12T13:07:00Z"),
        b("2026-11-09T00:00:00Z", "2026-11-16T00:00:00Z", false)
    );
}

#[test]
fn buckets_align_to_the_sunday_open() {
    let cal = cme();
    // Sunday 2026-11-08 17:00 CST = 23:00Z opens Monday's session (closes 16:00 CST = 22:00Z).
    assert_eq!(
        bucket(Timeframe::Hours(1), Some(&cal), "2026-11-08T23:00:00Z"),
        b("2026-11-08T23:00:00Z", "2026-11-09T00:00:00Z", false)
    );
    assert_eq!(
        bucket(Timeframe::Hours(4), Some(&cal), "2026-11-09T02:59:00Z"),
        b("2026-11-08T23:00:00Z", "2026-11-09T03:00:00Z", false)
    );
    // 23 hours = five 4 h buckets and a partial 3 h one before the 16:00 CT close.
    assert_eq!(
        bucket(Timeframe::Hours(4), Some(&cal), "2026-11-09T21:59:00Z"),
        b("2026-11-09T19:00:00Z", "2026-11-09T22:00:00Z", true)
    );
    // 2 h from 17:00 CT: 21:00Z is a bucket start, not 20:00Z (UTC alignment).
    assert_eq!(
        bucket(Timeframe::Hours(2), Some(&cal), "2026-11-09T20:30:00Z"),
        b("2026-11-09T19:00:00Z", "2026-11-09T21:00:00Z", false)
    );
    assert_eq!(
        bucket(Timeframe::Day, Some(&cal), "2026-11-09T05:00:00Z"),
        b("2026-11-08T23:00:00Z", "2026-11-09T22:00:00Z", false)
    );
    // Week: Monday's open (Sunday evening) to Friday's close.
    assert_eq!(
        bucket(Timeframe::Week, Some(&cal), "2026-11-11T05:00:00Z"),
        b("2026-11-08T23:00:00Z", "2026-11-13T22:00:00Z", false)
    );
    // Labels: the trade date (Sunday evening's open is Monday's), a week's Monday.
    assert_eq!(
        bar_date(Timeframe::Day, Some(&cal), t("2026-11-08T23:00:00Z")),
        CivilDate::new(2026, 11, 9).unwrap()
    );
    assert_eq!(
        bar_date(Timeframe::Week, Some(&cal), t("2026-11-08T23:00:00Z")),
        CivilDate::new(2026, 11, 9).unwrap()
    );
    assert_eq!(
        bar_date(Timeframe::Day, None, t("2026-11-08T23:00:00Z")),
        CivilDate::new(2026, 11, 8).unwrap()
    );
}

#[test]
fn a_holiday_halt_splits_buckets() {
    let cal = cme();
    // MLK Day: Tuesday 2026-01-20's session opens Sunday 23:00Z, halts Monday 18:00Z-23:00Z.
    assert_eq!(
        bucket(Timeframe::Hours(4), Some(&cal), "2026-01-19T17:59:00Z"),
        b("2026-01-19T15:00:00Z", "2026-01-19T18:00:00Z", true)
    );
    // After the halt, buckets start again at the reopen.
    assert_eq!(
        bucket(Timeframe::Hours(4), Some(&cal), "2026-01-19T23:30:00Z"),
        b("2026-01-19T23:00:00Z", "2026-01-20T03:00:00Z", false)
    );
    // The day is the whole trade date, halt included.
    assert_eq!(
        bucket(Timeframe::Day, Some(&cal), "2026-01-19T16:00:00Z"),
        b("2026-01-18T23:00:00Z", "2026-01-20T22:00:00Z", false)
    );
    // That week has no Monday trade date: it starts with Tuesday's session.
    assert_eq!(
        bucket(Timeframe::Week, Some(&cal), "2026-01-21T16:00:00Z"),
        b("2026-01-18T23:00:00Z", "2026-01-23T22:00:00Z", false)
    );
    // Aggregating across the halt never mixes the two sides.
    let mut s = BarSeries::new(MINUTE, 10_000);
    minutes(&mut s, t("2026-01-19T17:00:00Z"), 60); // to 18:00Z, the halt
    minutes(&mut s, t("2026-01-19T23:00:00Z"), 30);
    let mut d = Derived::new(Timeframe::Hours(4), s.capacity());
    d.update(&s, Some(&cal));
    let times: Vec<String> = d.bars().iter().map(|b| b.time.to_string()).collect();
    assert_eq!(
        times,
        [
            t("2026-01-19T15:00:00Z").to_string(),
            t("2026-01-19T23:00:00Z").to_string()
        ]
    );
    assert!(d.is_partial(0));
    assert!(!d.is_partial(1));
}

#[test]
fn buckets_follow_dst() {
    let cal = cme();
    // US DST starts 2026-03-08: Friday's session opened 17:00 CST (23:00Z), Sunday's opens
    // 17:00 CDT (22:00Z), so hourly buckets move with it.
    assert_eq!(
        bucket(Timeframe::Hours(1), Some(&cal), "2026-03-05T23:30:00Z"),
        b("2026-03-05T23:00:00Z", "2026-03-06T00:00:00Z", false)
    );
    assert_eq!(
        bucket(Timeframe::Hours(1), Some(&cal), "2026-03-08T22:30:00Z"),
        b("2026-03-08T22:00:00Z", "2026-03-08T23:00:00Z", false)
    );
    assert_eq!(
        bucket(Timeframe::Hours(4), Some(&cal), "2026-03-09T20:30:00Z"),
        b("2026-03-09T18:00:00Z", "2026-03-09T21:00:00Z", true)
    );
}

#[test]
fn incremental_updates_equal_a_full_rebuild() {
    let cal = cme();
    for tf in Timeframe::ALL {
        let mut s = BarSeries::new(MINUTE, 3_000);
        let mut d = Derived::new(tf, s.capacity());
        let check = |s: &BarSeries, d: &mut Derived, step: &str| {
            d.update(s, Some(&cal));
            let mut full = Derived::new(tf, s.capacity());
            full.update(s, Some(&cal));
            assert_eq!(d.bars(), full.bars(), "{tf} after {step}");
            assert_eq!(d.ends, full.ends, "{tf} after {step}");
            assert_eq!(d.partial, full.partial, "{tf} after {step}");
        };
        // Live ticks one minute at a time across a session close and the next open.
        let mut at = t("2026-11-09T20:50:00Z");
        for i in 0..40 {
            minutes(&mut s, at, 1);
            if i % 3 == 0 {
                check(&s, &mut d, "a tick");
            }
            at = UtcMillis(at.0 + MINUTE);
            if at == t("2026-11-09T22:00:00Z") {
                at = t("2026-11-09T23:00:00Z");
            }
        }
        check(&s, &mut d, "ticks");
        // A late history fill in the middle.
        s.upsert(Bar {
            time: t("2026-11-09T21:10:00Z"),
            ..*s.bars().first().unwrap()
        });
        check(&s, &mut d, "a late upsert");
        // History before everything.
        s.upsert(Bar {
            time: t("2026-11-09T15:00:00Z"),
            ..*s.bars().first().unwrap()
        });
        check(&s, &mut d, "an early upsert");
        // Many changes at once (the change log overflows).
        minutes(&mut s, t("2026-11-10T01:00:00Z"), 200);
        check(&s, &mut d, "a burst");
        // Trimming from the front.
        minutes(&mut s, t("2026-11-10T05:00:00Z"), 4_000);
        assert!(s.trimmed() > 0);
        check(&s, &mut d, "a trim");
        s.clear();
        check(&s, &mut d, "a clear");
        assert!(d.bars().is_empty());
    }
}

#[test]
fn bars_view_maps_times_to_bars() {
    let cal = cme();
    let mut s = BarSeries::new(MINUTE, 1_000);
    minutes(&mut s, t("2026-11-09T21:00:00Z"), 60);
    minutes(&mut s, t("2026-11-09T23:00:00Z"), 60);
    let mut d = Derived::new(Timeframe::Minutes(15), s.capacity());
    d.update(&s, Some(&cal));
    let v = d.view();
    assert_eq!(v.len(), 8);
    assert_eq!(v.bar_at(t("2026-11-09T21:14:59Z")), Some(0));
    assert_eq!(v.bar_at(t("2026-11-09T21:15:00Z")), Some(1));
    assert_eq!(v.bar_at(t("2026-11-09T22:30:00Z")), None, "the daily break");
    assert_eq!(v.bar_at(t("2026-11-09T23:00:00Z")), Some(4));
    assert_eq!(v.bar_at(t("2026-11-09T20:00:00Z")), None);
    let fixed = BarsView::fixed(s.bars(), MINUTE);
    assert_eq!(fixed.end(0), t("2026-11-09T21:01:00Z"));
    assert_eq!(fixed.bar_at(t("2026-11-09T21:00:30Z")), Some(0));
}

/// Crosshair sync between a 1-minute and a 15-minute chart: each maps the other's time into
/// its own bars.
#[test]
fn crosshair_sync_between_1m_and_15m() {
    let cal = cme();
    let mut s = BarSeries::new(MINUTE, 1_000);
    minutes(&mut s, t("2026-11-09T20:00:00Z"), 120); // to the 22:00Z close
    minutes(&mut s, t("2026-11-09T23:00:00Z"), 60);
    let one = BarsView::fixed(s.bars(), MINUTE);
    let mut d = Derived::new(Timeframe::Minutes(15), s.capacity());
    d.update(&s, Some(&cal));
    let fifteen = d.view();
    // The 1-minute chart's crosshair on 20:37 marks the 15-minute bar from 20:30.
    let i = fifteen.slot(t("2026-11-09T20:37:00Z")).unwrap() as usize;
    assert_eq!(fifteen.bars[i].time, t("2026-11-09T20:30:00Z"));
    // The 15-minute chart's crosshair reports its bar's start: the 1-minute bar at 20:30.
    let j = one.slot(fifteen.bars[i].time).unwrap() as usize;
    assert_eq!(one.bars[j].time, t("2026-11-09T20:30:00Z"));
    // In the daily break neither chart has a bar.
    assert_eq!(fifteen.slot(t("2026-11-09T22:30:00Z")), None);
    assert_eq!(one.slot(t("2026-11-09T22:30:00Z")), None);
    // Past the newest bar: extrapolated slots, as on the 1-minute chart.
    let last = fifteen.len() as i64 - 1;
    assert_eq!(fifteen.slot(t("2026-11-10T00:00:00Z")), Some(last + 1));
    assert_eq!(fifteen.slot(t("2026-11-10T00:31:00Z")), Some(last + 3));
    assert_eq!(fifteen.slot(t("2026-11-09T19:40:00Z")), Some(-2));
}

#[test]
fn trims_are_counted() {
    let mut s = BarSeries::new(MINUTE, 600);
    minutes(&mut s, t("2026-11-10T00:00:00Z"), 600);
    let mut d = Derived::new(Timeframe::Minutes(5), s.capacity());
    d.update(&s, None);
    let first = d.bars()[0].time;
    minutes(&mut s, t("2026-11-10T10:00:00Z"), 200);
    assert!(s.trimmed() > 0);
    d.update(&s, None);
    let dropped = (d.bars()[0].time.0 - first.0) / (5 * MINUTE);
    assert_eq!(d.trimmed(), dropped as u64);
}
