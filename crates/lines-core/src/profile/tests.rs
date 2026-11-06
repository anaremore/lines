use super::*;
use crate::book::OrderBook;
use crate::calendar::CivilDate;
use crate::ladder::build_ladder;
use crate::sessions::{ExchangeHours, HolidayTable, Hours, LocalTime, SessionSpec};
use crate::vap::VolumeAtPrice;

/// Synthetic CME-equity-like calendar (never the shipped data file).
const TABLE: &str = "\
format 1
version 2026-11-06
calendar test-equity
coverage 2026 2026 verified
source synthetic
2026-11-26 halt 12:00  verified   Thanksgiving
2026-11-27 early 12:15 verified   Day after Thanksgiving
";

fn equity() -> &'static SessionCalendar {
    let table = HolidayTable::parse(TABLE).unwrap();
    let cal = SessionCalendar::new(
        SessionSpec {
            time_zone: "America/Chicago",
            hours: Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(17, 0),
                eth_close: LocalTime::hm(16, 0),
                rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
            }),
            holidays: Some("test-equity"),
        },
        Some(&table),
    )
    .unwrap();
    Box::leak(Box::new(cal))
}

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn date(y: u16, m: u8, d: u8) -> CivilDate {
    CivilDate::new(y, m, d).unwrap()
}

const TICK: i64 = 25;
const BUY: Option<Aggressor> = Some(Aggressor::Buy);
const SELL: Option<Aggressor> = Some(Aggressor::Sell);

fn profiles() -> SessionProfiles {
    SessionProfiles::new(Some(equity()), 60_000, 10_000)
}

/// (trade date, total, [(price, buy, sell, unknown)]) of each session of `kind`.
#[allow(clippy::type_complexity)]
fn summary(
    p: &SessionProfiles,
    kind: SessionKind,
) -> Vec<(CivilDate, i64, Vec<(i64, i64, i64, i64)>)> {
    p.sessions(kind)
        .iter()
        .map(|s| {
            (
                s.session.trade_date,
                s.volume.total().0,
                s.volume
                    .iter()
                    .map(|(p, v)| (p.0, v.buy.0, v.sell.0, v.unknown.0))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn eth_session_boundary_and_daily_break() {
    // November 2026 is CST (UTC-6): ETH 17:00-16:00 CT = 23:00-22:00 UTC, RTH 14:30-21:00 UTC.
    let mut p = profiles();
    p.add(t("2026-11-05T14:29:59"), Price(600_000), Qty(1), BUY); // 08:29:59 CT: ETH only
    p.add(t("2026-11-05T14:30:00"), Price(600_025), Qty(2), SELL); // RTH open
    p.add(t("2026-11-05T21:59:59"), Price(600_000), Qty(3), None); // 15:59:59 CT: ETH only
    p.add(t("2026-11-05T22:30:00"), Price(600_050), Qty(4), BUY); // break: no session
    p.add(t("2026-11-05T23:00:00"), Price(600_050), Qty(5), BUY); // 17:00 CT: trade date 11-06
    assert_eq!(
        summary(&p, SessionKind::Eth),
        vec![
            (
                date(2026, 11, 5),
                6,
                vec![(600_000, 1, 0, 3), (600_025, 0, 2, 0)]
            ),
            (date(2026, 11, 6), 5, vec![(600_050, 5, 0, 0)]),
        ]
    );
    assert_eq!(
        summary(&p, SessionKind::Rth),
        vec![(date(2026, 11, 5), 2, vec![(600_025, 0, 2, 0)])]
    );
    assert_eq!(p.outside_eth(), 1);
    // The break trade is still in the footprint (visible range counts every trade).
    let fp: i64 = p
        .footprint()
        .range(t("2026-11-05T00:00:00"), t("2026-11-06T00:00:00"))
        .map(|(_, v)| v.total().0)
        .sum();
    assert_eq!(fp, 15);
    // Current session: in the 11-06 session that has trades; during the break, the newest.
    let cur = |at| {
        p.current(SessionKind::Eth, t(at))
            .map(|s| s.session.trade_date)
    };
    assert_eq!(cur("2026-11-06T01:00:00"), Some(date(2026, 11, 6)));
    assert_eq!(cur("2026-11-05T22:30:00"), Some(date(2026, 11, 6)));
    // The next session (trade date 11-09, opening Sunday) has no trades yet: nothing to show.
    assert_eq!(cur("2026-11-08T23:30:00"), None);
}

#[test]
fn dst_change_moves_session_bounds_in_utc() {
    // US DST ended Sunday 2026-11-01. Friday 10-30 is CDT (UTC-5): RTH 13:30-20:00 UTC, close
    // 21:00 UTC. Monday 11-02's session opens Sunday 17:00 CST = 23:00 UTC; RTH 14:30 UTC.
    let mut p = profiles();
    p.add(t("2026-10-30T13:30:00"), Price(600_000), Qty(1), BUY); // RTH 10-30 (CDT)
    p.add(t("2026-10-30T20:30:00"), Price(600_000), Qty(2), SELL); // after RTH, ETH 10-30
    p.add(t("2026-10-30T21:30:00"), Price(600_000), Qty(4), SELL); // after the 16:00 CDT close
    p.add(t("2026-11-01T22:30:00"), Price(600_025), Qty(8), BUY); // 16:30 CST: before the open
    p.add(t("2026-11-01T23:00:00"), Price(600_025), Qty(16), BUY); // open, trade date 11-02
    p.add(t("2026-11-02T14:00:00"), Price(600_050), Qty(32), BUY); // 08:00 CST: not RTH
    p.add(t("2026-11-02T14:30:00"), Price(600_050), Qty(64), SELL); // RTH 11-02 (CST)
    assert_eq!(
        summary(&p, SessionKind::Eth),
        vec![
            (date(2026, 10, 30), 3, vec![(600_000, 1, 2, 0)]),
            (
                date(2026, 11, 2),
                112,
                vec![(600_025, 16, 0, 0), (600_050, 32, 64, 0)]
            ),
        ]
    );
    assert_eq!(
        summary(&p, SessionKind::Rth),
        vec![
            (date(2026, 10, 30), 1, vec![(600_000, 1, 0, 0)]),
            (date(2026, 11, 2), 64, vec![(600_050, 0, 64, 0)]),
        ]
    );
    assert_eq!(p.outside_eth(), 2);
}

#[test]
fn holiday_halt_and_early_close() {
    // Thanksgiving (Thu 11-26) halts at 12:00 CT and is not a trade date: its morning belongs
    // to Friday 11-27's session, which closes early at 12:15 CT (18:15 UTC), RTH included.
    let mut p = profiles();
    p.add(t("2026-11-25T23:30:00"), Price(600_000), Qty(1), BUY); // Wed evening: TD 11-27
    p.add(t("2026-11-26T15:00:00"), Price(600_025), Qty(2), BUY); // Thu 09:00 CT: TD 11-27, no RTH
    p.add(t("2026-11-26T19:00:00"), Price(600_025), Qty(4), BUY); // Thu 13:00 CT: inside the halt
    p.add(t("2026-11-27T15:00:00"), Price(600_050), Qty(8), SELL); // Fri RTH
    p.add(t("2026-11-27T18:14:59"), Price(600_050), Qty(16), SELL); // last second
    p.add(t("2026-11-27T18:15:00"), Price(600_075), Qty(32), SELL); // after the early close
    let eth = p.sessions(SessionKind::Eth);
    assert_eq!(eth.len(), 1);
    let s = &eth[0];
    assert_eq!(s.session.trade_date, date(2026, 11, 27));
    assert!(s.session.early_close);
    assert_eq!(s.session.close, t("2026-11-27T18:15:00"));
    // A trade inside the holiday halt still lies in the session's span: it is counted (the
    // source printed it; the calendar can't make it untrue).
    assert_eq!(
        summary(&p, SessionKind::Eth),
        vec![(
            date(2026, 11, 27),
            31,
            vec![(600_000, 1, 0, 0), (600_025, 6, 0, 0), (600_050, 0, 24, 0)]
        )]
    );
    assert_eq!(
        summary(&p, SessionKind::Rth),
        vec![(date(2026, 11, 27), 24, vec![(600_050, 0, 24, 0)])]
    );
    assert_eq!(p.outside_eth(), 1);
}

#[test]
fn late_trades_go_to_their_own_session_and_old_sessions_are_dropped() {
    let mut p = profiles();
    p.add(t("2026-11-05T15:00:00"), Price(600_000), Qty(1), BUY);
    p.add(t("2026-11-06T15:00:00"), Price(600_000), Qty(2), BUY);
    p.add(t("2026-11-05T16:00:00"), Price(600_000), Qty(4), BUY); // late, 11-05
    p.add(t("2026-11-04T15:00:00"), Price(600_000), Qty(8), BUY); // older session, inserted
    let totals: Vec<_> = summary(&p, SessionKind::Eth)
        .into_iter()
        .map(|(d, v, _)| (d.day, v))
        .collect();
    assert_eq!(totals, vec![(4, 8), (5, 5), (6, 2)]);
    assert_eq!(p.data_start(), Some(t("2026-11-04T15:00:00")));

    let mut p = profiles();
    let mut day = date(2026, 6, 1);
    let mut n = 0;
    while n < SessionProfiles::MAX_SESSIONS + 5 {
        if day.is_business_day() {
            p.add(
                equity().instant(day, LocalTime::hm(10, 0)),
                Price(600_000),
                Qty(1),
                None,
            );
            n += 1;
        }
        day = day.add_days(1);
    }
    assert_eq!(
        p.sessions(SessionKind::Eth).len(),
        SessionProfiles::MAX_SESSIONS
    );
    assert_eq!(
        p.sessions(SessionKind::Rth).len(),
        SessionProfiles::MAX_SESSIONS
    );
    // A trade older than every kept session is not resurrected as a session.
    let oldest = p.sessions(SessionKind::Eth)[0].session.trade_date;
    p.add(t("2026-06-01T15:00:00"), Price(600_000), Qty(1), None);
    assert_eq!(p.sessions(SessionKind::Eth)[0].session.trade_date, oldest);
    assert_eq!(
        p.sessions(SessionKind::Eth).len(),
        SessionProfiles::MAX_SESSIONS
    );
}

/// Rows from per-row totals (`grouping` 1), low to high from `base` units.
fn rows_of(vols: &[i64]) -> ProfileRows {
    let mut r = ProfileRows::with_capacity(64);
    let levels: Vec<(Price, SideVolume)> = vols
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                Price(1_000 + i as i64 * TICK),
                SideVolume::of(Qty(*v), None),
            )
        })
        .collect();
    r.build(TICK, 70, || levels.iter().copied());
    r
}

#[test]
fn point_of_control_and_value_area_hand_computed() {
    // total 65; 70% needs 100*va >= 4550 => va >= 46. Start at the POC (30, row 3):
    // below 9 vs above 12 -> above (42); below 9 vs above 3 -> below (51). VA rows 2..=4.
    let r = rows_of(&[4, 6, 9, 30, 12, 3, 1]);
    assert_eq!((r.total, r.max_row, r.poc), (Qty(65), Qty(30), Some(3)));
    assert_eq!(r.value_area, Some((2, 4)));
    assert_eq!(r.row_price(3), Price(1_075));
    // Equal neighbours are added together: 20 + 15 + 15 = 50 >= 49 (70% of 70).
    let r = rows_of(&[2, 8, 15, 20, 15, 8, 2]);
    assert_eq!((r.poc, r.value_area), (Some(3), Some((2, 4))));
    // Empty rows inside the range count as rows of volume 0; 100% takes every row.
    let mut r = ProfileRows::with_capacity(8);
    let levels = [
        (Price(1_000), SideVolume::of(Qty(5), BUY)),
        (Price(1_075), SideVolume::of(Qty(5), SELL)),
    ];
    r.build(TICK, 100, || levels.iter().copied());
    assert_eq!(r.rows.len(), 4);
    assert_eq!(r.value_area, Some((0, 3)));
    assert_eq!(r.rows[1], ProfileRow::default());
}

#[test]
fn poc_ties_nearest_the_middle_then_lower() {
    // Two maxima: rows 0 and 2 of 0..=4; row 2 is the middle.
    assert_eq!(rows_of(&[10, 3, 10, 3, 3]).poc, Some(2));
    // Equally near the middle (rows 0 and 3 of 0..=3): the lower.
    assert_eq!(rows_of(&[10, 3, 3, 10]).poc, Some(0));
    assert_eq!(rows_of(&[1, 10, 10, 1]).poc, Some(1));
    // Single row; empty input.
    let r = rows_of(&[7]);
    assert_eq!((r.poc, r.value_area), (Some(0), Some((0, 0))));
    let r = rows_of(&[]);
    assert_eq!(
        (r.rows.len(), r.poc, r.value_area, r.total),
        (0, None, None, Qty::ZERO)
    );
}

#[test]
fn rows_group_like_the_dom_ladder() {
    // Same trades into a profile and the DOM's volume column, grouping 5: identical rows,
    // including negative prices (rows use floor division).
    let mut vap = VolumeAtPrice::default();
    let mut pv = PriceVolume::default();
    for k in -40i64..60 {
        let price = Price(k * 7 % 31 * TICK);
        let q = Qty(k.rem_euclid(5) + 1);
        vap.add(price, q);
        pv.add(price, q, if k % 2 == 0 { BUY } else { SELL });
    }
    let units = 5 * TICK;
    let mut r = ProfileRows::with_capacity(64);
    r.build(units, 70, || pv.iter());
    let top = r.base_row + r.rows.len() as i64 - 1;
    let mut ladder = Vec::with_capacity(64);
    build_ladder(
        &OrderBook::default(),
        &vap,
        None,
        units,
        top,
        r.rows.len(),
        &mut ladder,
    );
    for (i, row) in r.rows.iter().enumerate() {
        let l = &ladder[r.rows.len() - 1 - i]; // ladder runs high to low
        assert_eq!(l.price, r.row_price(i));
        assert_eq!(l.volume, row.total, "row {i}");
        assert_eq!(row.total, row.volume.buy.saturating_add(row.volume.sell));
    }
    assert_eq!(r.total, vap.total());
}

#[test]
fn deterministic_and_order_independent() {
    let mut trades = Vec::new();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 0..2_000i64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let time = UtcMillis(t("2026-11-04T00:00:00").0 + (x % (3 * 86_400_000)) as i64);
        let price = Price(600_000 + (x >> 20) as i64 % 80 * TICK);
        let side = match i % 3 {
            0 => BUY,
            1 => SELL,
            _ => None,
        };
        trades.push((time, price, Qty((x >> 40) as i64 % 9 + 1), side));
    }
    let run = |order: &mut dyn Iterator<Item = &(UtcMillis, Price, Qty, Option<Aggressor>)>| {
        let mut p = profiles();
        for &(a, b, c, d) in order {
            p.add(a, b, c, d);
        }
        p
    };
    let a = run(&mut trades.iter());
    let b = run(&mut trades.iter().rev());
    for kind in [SessionKind::Eth, SessionKind::Rth] {
        assert_eq!(a.sessions(kind), b.sessions(kind));
        assert!(!a.sessions(kind).is_empty());
    }
    assert_eq!(a.footprint(), b.footprint());
    let rows = |p: &SessionProfiles| {
        let mut r = ProfileRows::with_capacity(256);
        let s = p.sessions(SessionKind::Eth);
        r.build(2 * TICK, 70, || s.iter().flat_map(|s| s.volume.iter()));
        r
    };
    assert_eq!(rows(&a), rows(&b));
}

#[test]
fn footprint_range_and_bound() {
    let mut f = Footprint::new(60_000, 3);
    for m in 0..5i64 {
        f.add(UtcMillis(m * 60_000 + 59_999), Price(100), Qty(m + 1), BUY);
        f.add(UtcMillis(m * 60_000), Price(75), Qty(1), SELL);
    }
    assert_eq!(f.first_bar(), Some(UtcMillis(120_000)));
    let got: Vec<_> = f
        .range(UtcMillis(120_000), UtcMillis(240_000))
        .map(|(p, v)| (p.0, v.buy.0, v.sell.0))
        .collect();
    assert_eq!(got, vec![(75, 0, 1), (100, 3, 0), (75, 0, 1), (100, 4, 0)]);
    assert_eq!(f.range(UtcMillis(240_000), UtcMillis(240_000)).count(), 0);
}

#[test]
fn grouping_fits_the_buffer() {
    let (lo, hi) = (Price(0), Price(999 * TICK)); // 1000 ticks
    assert_eq!(ProfileRows::fit_grouping(1, TICK, lo, hi, 1000), 1);
    assert_eq!(ProfileRows::fit_grouping(1, TICK, lo, hi, 999), 2);
    assert_eq!(ProfileRows::fit_grouping(3, TICK, lo, hi, 300), 5);
    assert_eq!(ProfileRows::fit_grouping(1, TICK, lo, hi, 30), 250);
    assert_eq!(ProfileRows::fit_grouping(1, TICK, lo, hi, 3), 2500);
}

/// Trade times come from untrusted files (replays, fuzzing): any `i64` must be accepted
/// without a panic. Far-out times are clamped by the calendar to its supported years.
#[test]
fn any_trade_time_is_accepted() {
    let mut p = profiles();
    let year = 365 * 86_400_000i64;
    let edge = |y: i64| (y - 1970) * year;
    for ms in [
        i64::MIN,
        i64::MIN + 1,
        -1,
        0,
        edge(1896),
        edge(1899),
        edge(1900) - 1,
        edge(1900),
        edge(2999),
        edge(3000),
        edge(3001),
        i64::MAX - 1,
        i64::MAX,
    ] {
        for d in [-2 * 86_400_000, -1, 0, 1, 2 * 86_400_000] {
            let t = UtcMillis(ms.saturating_add(d));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                p.add(t, Price(600_000), Qty(1), BUY)
            }));
            assert!(r.is_ok(), "panicked at {} ms", t.0);
        }
    }
    // Times near or past the calendar's ends are in no session (footprint only); 1970 is.
    for s in p.sessions(SessionKind::Eth) {
        assert!(
            (1900..3000).contains(&s.session.trade_date.year),
            "{:?}",
            s.session
        );
    }
    assert!(
        p.sessions(SessionKind::Eth)
            .iter()
            .any(|s| s.session.trade_date.year == 1970)
    );
}
