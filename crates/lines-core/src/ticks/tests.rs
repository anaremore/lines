use super::*;
use crate::bars::BarSeries;
use crate::sessions::{ExchangeHours, HolidayTable, Hours, LocalTime, SessionSpec};

const BUY: Option<Aggressor> = Some(Aggressor::Buy);
const SELL: Option<Aggressor> = Some(Aggressor::Sell);

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

/// Synthetic CME-equity-like hours (17:00–16:00 CT), no holidays.
fn cme() -> &'static SessionCalendar {
    cme_with("")
}

/// [`cme`] with these holiday rows.
fn cme_with(rows: &str) -> &'static SessionCalendar {
    let table = format!(
        "format 1\nversion 2026-11-06\ncalendar test-equity\ncoverage 2026 2027 verified\nsource synthetic\n{rows}"
    );
    Box::leak(Box::new(
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
            Some(&HolidayTable::parse(&table).unwrap()),
        )
        .unwrap(),
    ))
}

fn bar(time: UtcMillis, o: i64, h: i64, l: i64, c: i64, v: i64) -> Bar {
    Bar {
        time,
        open: Price(o),
        high: Price(h),
        low: Price(l),
        close: Price(c),
        volume: Qty(v),
    }
}

/// OHLC and volume equal a hand-computed fixture: 3-tick bars over 7 trades.
#[test]
fn bars_are_exact() {
    let t0 = t("2026-11-05T15:00:00Z");
    let at = |s: i64| UtcMillis(t0.0 + s * 1000);
    let trades = [
        (0, 600_000, 2, BUY),
        (1, 600_050, 1, SELL),
        (2, 599_975, 5, BUY),
        (3, 600_025, 3, None),
        (5, 600_100, 1, BUY),
        (8, 600_000, 4, SELL),
        (9, 600_025, 7, BUY),
    ];
    let mut tb = TickBars::new(3, TickReset::Session, None, 100);
    for (s, p, q, a) in trades {
        tb.push(at(s), Price(p), Qty(q), a);
    }
    assert_eq!(
        tb.bars(),
        &[
            bar(at(0), 600_000, 600_050, 599_975, 599_975, 8),
            bar(at(3), 600_025, 600_100, 600_000, 600_000, 8),
            bar(at(9), 600_025, 600_025, 600_025, 600_025, 7),
        ]
    );
    assert_eq!((tb.count(0), tb.count(1), tb.count(2)), (3, 3, 1));
    assert_eq!(tb.last_trade(0), Some(at(2)));
    assert_eq!(tb.last_trade(1), Some(at(8)));
    // Spans: to the next bar's start, the newest to just after its last trade.
    let v = tb.view();
    assert_eq!(v.end(0), at(3));
    assert_eq!(v.end(1), at(9));
    assert_eq!(v.end(2), UtcMillis(at(9).0 + 1));
    // Per-price volume by side, exact.
    let l1: Vec<_> = tb.levels(1).collect();
    assert_eq!(
        l1,
        [
            (Price(600_000), SideVolume::of(Qty(4), SELL)),
            (Price(600_025), SideVolume::of(Qty(3), None)),
            (Price(600_100), SideVolume::of(Qty(1), BUY)),
        ]
    );
    for i in 0..tb.len() {
        let sum = tb
            .levels(i)
            .fold(Qty::ZERO, |a, (_, v)| a.saturating_add(v.total()));
        assert_eq!(sum, tb.bars()[i].volume);
        assert!(!tb.is_partial(i));
    }
    assert_eq!(tb.trades(), 7);
}

/// The first trade of a new session closes the forming bar short (partial); continuous mode
/// keeps counting across the break.
#[test]
fn bars_reset_at_the_session_open() {
    let cal = cme();
    // Thu 2026-11-05: the session closes 16:00 CST (22:00 UTC), the next opens 17:00 CST.
    let before = t("2026-11-05T21:59:58Z");
    let after = t("2026-11-05T23:00:01Z");
    let between = t("2026-11-05T22:30:00Z");
    let feed = |tb: &mut TickBars| {
        for i in 0..5 {
            tb.push(UtcMillis(before.0 + i), Price(600_000), Qty(1), BUY);
        }
        // Between sessions: belongs to the next one.
        tb.push(between, Price(600_025), Qty(1), SELL);
        for i in 0..3 {
            tb.push(UtcMillis(after.0 + i), Price(600_050), Qty(1), BUY);
        }
    };
    let mut s = TickBars::new(4, TickReset::Session, Some(cal), 100);
    feed(&mut s);
    // 4 | 1 (partial: the session closed) | 4 (the 22:30 trade opens the next session's bar)
    assert_eq!(s.len(), 3);
    assert_eq!((s.count(0), s.count(1), s.count(2)), (4, 1, 4));
    assert!(!s.is_partial(0) && s.is_partial(1) && !s.is_partial(2));
    assert_eq!(s.bars()[2].time, between);
    // The partial bar's span ends after its own last trade, not at the next session.
    assert_eq!(s.view().end(1), UtcMillis(before.0 + 5));

    let mut c = TickBars::new(4, TickReset::Continuous, Some(cal), 100);
    feed(&mut c);
    assert_eq!(c.len(), 3);
    assert_eq!((c.count(0), c.count(1), c.count(2)), (4, 4, 1));
    assert!((0..3).all(|i| !c.is_partial(i)));

    // Without a calendar: the UTC day.
    let mut u = TickBars::new(10, TickReset::Session, None, 100);
    u.push(t("2026-11-05T23:59:59Z"), Price(1), Qty(1), None);
    u.push(t("2026-11-06T00:00:00Z"), Price(1), Qty(1), None);
    assert_eq!(u.len(), 2);
    assert!(u.is_partial(0));
}

/// Feeds `trades` (time, price) of one lot each into 4-tick session-reset bars.
fn four_tick(cal: &'static SessionCalendar, trades: &[(UtcMillis, i64)]) -> TickBars {
    let mut tb = TickBars::new(4, TickReset::Session, Some(cal), 100);
    for (time, p) in trades {
        tb.push(*time, Price(*p), Qty(1), BUY);
    }
    tb
}

/// The week DST ends (US, Sunday 2026-11-01): Friday's session closes 16:00 CDT = 21:00 UTC,
/// Sunday's opens 17:00 CST = 23:00 UTC. A trade at 21:30 UTC Friday is between sessions (it
/// would be inside one if the close were taken in CST): it opens Sunday's session.
#[test]
fn resets_follow_dst() {
    let tb = four_tick(
        cme(),
        &[
            (t("2026-10-30T20:59:58Z"), 1),
            (t("2026-10-30T20:59:59Z"), 2),
            (t("2026-10-30T21:30:00Z"), 3),
            (t("2026-11-01T23:00:01Z"), 4),
            (t("2026-11-02T14:00:00Z"), 5),
        ],
    );
    assert_eq!(tb.len(), 2);
    assert_eq!((tb.count(0), tb.count(1)), (2, 3));
    assert!(tb.is_partial(0) && tb.is_forming(1) && !tb.is_partial(1));
    assert_eq!(tb.bars()[1].time, t("2026-10-30T21:30:00Z"));
    assert!(tb.starts_session(0) && tb.starts_session(1));
}

/// A holiday halt inside a session (MLK Day: Tuesday 2026-01-20's session halts Monday
/// 18:00Z–23:00Z) is a break like a close: the bar before it is partial, the reopen starts a
/// bar. An early close (2026-11-27, 12:15 CT = 18:15 UTC) cuts the session there: a trade at
/// 18:30 UTC opens the next session's bar.
#[test]
fn resets_at_holiday_halts_and_early_closes() {
    let cal = cme_with(
        "2026-01-19 halt 12:00  verified   MLK\n2026-11-27 early 12:15 verified   Day after Thanksgiving\n",
    );
    let halt = four_tick(
        cal,
        &[
            (t("2026-01-19T17:59:58Z"), 1),
            (t("2026-01-19T17:59:59Z"), 2),
            (t("2026-01-19T23:00:00Z"), 3),
            (t("2026-01-19T23:00:01Z"), 4),
        ],
    );
    assert_eq!(halt.len(), 2);
    assert!(halt.is_partial(0) && halt.starts_session(1));
    assert_eq!(halt.bars()[1].time, t("2026-01-19T23:00:00Z"));

    let early = four_tick(
        cal,
        &[
            (t("2026-11-27T18:14:59Z"), 1),
            (t("2026-11-27T18:30:00Z"), 2),
        ],
    );
    assert_eq!(early.len(), 2);
    assert!(early.is_partial(0));
}

/// A session's last bar stops forming when the market clock passes the close (it can never
/// take another trade), not only at the next session's first trade; continuous bars keep
/// forming. A holiday halt's start closes the session part before it the same way.
#[test]
fn the_closed_sessions_last_bar_is_partial_at_the_close() {
    let mut s = four_tick(
        cme(),
        &[
            (t("2026-11-05T21:59:58Z"), 1),
            (t("2026-11-05T21:59:59Z"), 2),
        ],
    );
    let open = Some(t("2026-11-05T21:59:59.500Z"));
    assert!(
        s.is_forming_at(0, open) && !s.is_partial_at(0, open),
        "still open"
    );
    assert!(s.is_forming_at(0, None), "no clock: as built");
    let closed = Some(t("2026-11-05T22:00:00Z"));
    assert!(s.is_partial_at(0, closed) && !s.is_forming_at(0, closed));
    // The next session's first trade opens a bar; the old one stays partial, the new one forms.
    s.push(t("2026-11-05T23:00:01Z"), Price(3), Qty(1), BUY);
    let later = Some(t("2026-11-05T23:00:02Z"));
    assert!(s.is_partial_at(0, later) && s.is_forming_at(1, later));
    assert!(!s.is_partial_at(1, later));
    // A full bar is never partial.
    let full = four_tick(cme(), &[(t("2026-11-05T21:59:58Z"), 1); 4]);
    assert!(!full.is_partial_at(0, Some(t("2026-11-05T22:30:00Z"))));
    // A late trade from the closed session keeps the bar's close.
    let mut late = four_tick(cme(), &[(t("2026-11-05T21:59:58Z"), 1)]);
    late.push(t("2026-11-05T21:59:57Z"), Price(2), Qty(1), BUY);
    assert!(late.is_partial_at(0, closed));

    let mut c = TickBars::new(4, TickReset::Continuous, Some(cme()), 100);
    c.push(t("2026-11-05T21:59:58Z"), Price(1), Qty(1), BUY);
    let night = Some(t("2026-11-05T22:30:00Z"));
    assert!(
        c.is_forming_at(0, night) && !c.is_partial_at(0, night),
        "continuous runs on"
    );

    let cal = cme_with(
        "2026-01-19 halt 12:00  verified   MLK
",
    );
    let halt = four_tick(cal, &[(t("2026-01-19T17:59:58Z"), 1)]);
    assert!(
        halt.is_partial_at(0, Some(t("2026-01-19T18:00:00Z"))),
        "a halt's start"
    );
    assert!(halt.is_forming_at(0, Some(t("2026-01-19T17:59:59Z"))));
}

/// A late trade from the previous session, delivered after the new session's first trade,
/// joins the forming bar: sessions never go backwards, so it never opens a second partial bar.
#[test]
fn a_late_trade_from_the_previous_session_joins_the_forming_bar() {
    let tb = four_tick(
        cme(),
        &[
            (t("2026-11-05T21:59:58Z"), 1),
            (t("2026-11-05T23:00:01Z"), 2),
            (t("2026-11-05T21:59:59Z"), 3), // late
            (t("2026-11-05T23:00:02Z"), 4),
        ],
    );
    assert_eq!(tb.len(), 2);
    assert_eq!((tb.count(0), tb.count(1)), (1, 3));
    assert!(tb.is_partial(0) && !tb.is_partial(1));
    // The bar keeps its start; its low/high take the late trade's price.
    assert_eq!(tb.bars()[1].time, t("2026-11-05T23:00:01Z"));
    assert_eq!(tb.bars()[1].low, Price(2));
}

/// Continuous bars run across the break; the bar holding the new session's first trade is
/// marked for the divider.
#[test]
fn continuous_bars_mark_the_session_start() {
    let mut tb = TickBars::new(3, TickReset::Continuous, Some(cme()), 100);
    for (i, s) in [
        "21:59:57",
        "21:59:58",
        "21:59:59",
        "21:59:59.5",
        "23:00:01",
        "23:00:02",
    ]
    .iter()
    .enumerate()
    {
        tb.push(t(&format!("2026-11-05T{s}Z")), Price(i as i64), Qty(1), BUY);
    }
    assert_eq!(tb.len(), 2);
    assert!(tb.starts_session(0) && tb.starts_session(1));
    assert!(!tb.is_partial(0) && !tb.is_partial(1));
    // Contiguous across the break.
    assert_eq!(tb.view().end(0), tb.bars()[1].time);
}

/// Crosshair sync between a 500t chart and a 1m chart built from the same trades: a 1-minute
/// time lands on the tick bar holding it, a tick bar's start on the minute holding it.
#[test]
fn crosshair_sync_between_500t_and_1m() {
    let t0 = t("2026-11-05T15:00:00Z");
    let mut minutes = BarSeries::new(60_000, 10_000);
    let mut ticks = TickBars::new(500, TickReset::Session, Some(cme()), 10_000);
    // 20 000 trades, every 150–450 ms: a 500t bar spans one to four minutes.
    let mut now = t0.0;
    for i in 0..20_000i64 {
        now += 150 + (i * 7919 % 301);
        let p = Price(600_000 + (i * 31 % 17) * 25);
        minutes.apply_trade(UtcMillis(now), p, Qty(1));
        ticks.push(UtcMillis(now), p, Qty(1), BUY);
    }
    let one = BarsView::fixed(minutes.bars(), 60_000);
    let tv = ticks.view();
    assert_eq!(tv.len(), 40);
    for (k, m) in minutes.bars().iter().enumerate() {
        // The minute's start (what a 1m chart sends) → the tick bar whose span holds it.
        let slot = tv
            .slot(m.time)
            .expect("inside the tick bars or before them");
        if m.time < tv.bars[0].time {
            assert!(slot < 0);
            continue;
        }
        let i = slot as usize;
        assert!(
            tv.bars[i].time <= m.time && m.time < tv.end(i),
            "minute {k}"
        );
    }
    for (i, b) in tv.bars.iter().enumerate() {
        // A tick bar's start (what the 500t chart sends) → the minute holding it.
        let slot = one.slot(b.time).expect("a minute") as usize;
        assert_eq!(
            minutes.bars()[slot].time,
            b.time.floor_to(60_000),
            "bar {i}"
        );
    }
    // Past the newest bar: extrapolated by the view's period, never a wrong bar.
    let after = UtcMillis(tv.end(tv.len() - 1).0 + 10 * tv.period_ms);
    assert_eq!(tv.slot(after), Some(tv.len() as i64 - 1 + 1 + 10));
}

/// At capacity the oldest bars go in chunks, counted as trimmed; the newest stay exact.
#[test]
fn memory_is_bounded() {
    let mut tb = TickBars::new(2, TickReset::Continuous, None, 32);
    for i in 0..1_000i64 {
        tb.push(UtcMillis(i * 1000), Price(i), Qty(1), BUY);
    }
    assert!(tb.len() <= 32);
    assert_eq!(tb.trimmed() + tb.len() as u64, 500);
    let last = tb.bars()[tb.len() - 1];
    assert_eq!((last.open, last.close), (Price(998), Price(999)));
    tb.clear();
    assert!(tb.is_empty());
    assert_eq!(tb.trimmed(), 500);
}

#[test]
fn view_period_is_axis_friendly() {
    assert_eq!(nice_period(0), 1_000);
    assert_eq!(nice_period(7_345), 5_000);
    assert_eq!(nice_period(61_000), 60_000);
    assert_eq!(nice_period(i64::MAX), 14_400_000);
}

/// Volume bars: a bar closes once it holds `n` whole contracts (here a product with two
/// quantity decimals); the trade that reaches it stays whole in its bar.
#[test]
fn volume_bars_hold_whole_contracts() {
    let t0 = t("2026-11-05T15:00:00Z");
    let at = |s: i64| UtcMillis(t0.0 + s * 1000);
    let scale = BarScale {
        tick: 25,
        qty_unit: 100,
    };
    let mut tb = TickBars::with_kind(BarKind::Volume(5), TickReset::Session, None, scale, 100);
    // 2.00 + 2.50 = 4.50 (short of 5), then 1.25 reaches 5.75: the bar closes holding it.
    for (s, q) in [(0, 200), (1, 250), (2, 125), (3, 100), (4, 600)] {
        tb.push(at(s), Price(600_000), Qty(q), BUY);
    }
    let v: Vec<i64> = tb.bars().iter().map(|b| b.volume.0).collect();
    assert_eq!(
        v,
        [575, 700],
        "never split; a bar holds at least 5 contracts"
    );
    assert!(!tb.is_forming(0) && !tb.is_forming(1), "both done");
    let mut f = TickBars::with_kind(BarKind::Volume(5), TickReset::Session, None, scale, 100);
    f.push(at(0), Price(600_000), Qty(130), BUY);
    // 3.70 to go, rounded up: 4 contracts.
    assert_eq!(f.to_go(None), Some(4));
}

/// Range bars are exact in ticks: a bar is never wider than `r` ticks; it is done at exactly
/// `r`; a trade that would widen it opens the next bar at its own price (a gap stays a gap).
#[test]
fn range_bars_are_exact_in_ticks() {
    let t0 = t("2026-11-05T15:00:00Z");
    let at = |s: i64| UtcMillis(t0.0 + s * 1000);
    let scale = BarScale {
        tick: 25,
        qty_unit: 1,
    };
    let mut tb = TickBars::with_kind(BarKind::Range(4), TickReset::Session, None, scale, 100);
    // Ticks of 25: up 3 ticks, then a 5th tick would make 5 → a new bar; that bar reaches 4.
    let prices = [
        600_000, 600_050, 600_075, // range 3 ticks
        600_125, // would be 5 ticks: next bar at 600_125
        600_100, 600_050, 600_025, // 600_125..600_025 = 4 ticks: done
        600_025, // done bar takes nothing: a new bar
        600_400, // a gap of 15 ticks: closes that bar short, opens at 600_400
    ];
    for (i, p) in prices.into_iter().enumerate() {
        tb.push(at(i as i64), Price(p), Qty(1), BUY);
    }
    let ranges: Vec<(i64, i64)> = tb.bars().iter().map(|b| (b.low.0, b.high.0)).collect();
    assert_eq!(
        ranges,
        [
            (600_000, 600_075),
            (600_025, 600_125),
            (600_025, 600_025),
            (600_400, 600_400)
        ]
    );
    for b in tb.bars() {
        assert!((b.high.0 - b.low.0) % 25 == 0 && b.high.0 - b.low.0 <= 4 * 25);
    }
    assert_eq!(
        tb.bars()[1].open,
        Price(600_125),
        "opens at the trade's own price"
    );
    // The newest: 0 of 4 ticks wide, 4 to go.
    assert_eq!(tb.to_go(None), Some(4));
}

/// Seconds bars: the trades of each aligned slot, starting at the slot's start and spanning
/// it; empty slots have no bar; a late trade joins the newest; the slot's end closes it.
#[test]
fn seconds_bars_fill_aligned_slots() {
    let t0 = t("2026-11-05T15:00:00Z");
    let at = |ms: i64| UtcMillis(t0.0 + ms);
    let mut tb = TickBars::with_kind(
        BarKind::Seconds(30),
        TickReset::Session,
        None,
        BarScale::UNIT,
        100,
    );
    for (ms, p) in [
        (1_000, 100),
        (29_999, 101),
        (30_000, 102),
        (75_000, 103), // 60-90 s: the 30-60 s slot had one trade, 60-90 s gets this one
        (59_000, 99),  // late (an earlier slot): joins the newest bar
    ] {
        tb.push(at(ms), Price(p), Qty(1), BUY);
    }
    let starts: Vec<i64> = tb.bars().iter().map(|b| b.time.0 - t0.0).collect();
    assert_eq!(starts, [0, 30_000, 60_000]);
    let v = tb.view();
    assert_eq!(
        (v.end(0), v.end(1), v.end(2)),
        (at(30_000), at(60_000), at(90_000))
    );
    assert_eq!(v.period_ms, 30_000);
    assert_eq!(tb.count(2), 2, "the late trade joined the newest bar");
    assert_eq!(tb.bars()[2].low, Price(99));
    // Forming until its slot ends; never partial for ending; no "to go" (the clock closes it).
    assert!(tb.is_forming_at(2, Some(at(89_999))));
    assert!(!tb.is_forming_at(2, Some(at(90_000))));
    assert!(!tb.is_partial_at(2, Some(at(90_000))));
    assert_eq!(tb.to_go(Some(at(80_000))), None);
    assert!(!BarKind::Seconds(7).is_valid() && BarKind::Seconds(30).is_valid());
}

/// Every kind restarts at the session open (unless continuous), like tick bars.
#[test]
fn every_kind_resets_at_the_session_open() {
    let cal = cme();
    // CST: the session opens 23:00 UTC; trades just before and after it.
    let before = t("2026-11-05T21:59:00Z");
    let after = t("2026-11-05T23:00:30Z");
    for kind in [
        BarKind::Ticks(100),
        BarKind::Volume(100),
        BarKind::Range(100),
        BarKind::Seconds(3_600),
    ] {
        let mut tb = TickBars::with_kind(kind, TickReset::Session, Some(cal), BarScale::UNIT, 100);
        tb.push(before, Price(100), Qty(1), BUY);
        tb.push(after, Price(101), Qty(1), BUY);
        assert_eq!(tb.len(), 2, "{kind:?}");
        assert!(tb.starts_session(1), "{kind:?}");
        let mut c =
            TickBars::with_kind(kind, TickReset::Continuous, Some(cal), BarScale::UNIT, 100);
        c.push(before, Price(100), Qty(1), BUY);
        c.push(after, Price(101), Qty(1), BUY);
        let expect = if matches!(kind, BarKind::Seconds(_)) {
            2
        } else {
            1
        };
        assert_eq!(c.len(), expect, "{kind:?} continuous");
    }
}
