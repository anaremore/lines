use super::*;

/// Synthetic CME-equity-like table (tests never depend on the shipped data file).
const TABLE: &str = "\
format 1
version 2026-11-06
calendar test-equity
coverage 2026 2027 verified
source synthetic
2026-01-01 closed      verified   New Year's Day
2026-01-19 halt 12:00  verified   Martin Luther King Jr. Day
2026-04-03 closed      unverified Good Friday
2026-11-26 halt 12:00  verified   Thanksgiving
2026-11-27 early 12:15 verified   Day after Thanksgiving
2026-12-24 early 12:15 verified   Christmas Eve
2026-12-25 closed      verified   Christmas
2027-01-01 closed      verified   New Year's Day
";

const CT: &str = "America/Chicago";

const EQUITY_HOURS: Hours = Hours::Exchange(ExchangeHours {
    eth_open: LocalTime::hm(17, 0),
    eth_close: LocalTime::hm(16, 0),
    rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
});

fn table() -> HolidayTable {
    HolidayTable::parse(TABLE).unwrap()
}

fn equity() -> SessionCalendar {
    SessionCalendar::new(
        SessionSpec {
            time_zone: CT,
            hours: EQUITY_HOURS,
            holidays: Some("test-equity"),
        },
        Some(&table()),
    )
    .unwrap()
}

fn crypto(day_start: LocalTime) -> SessionCalendar {
    SessionCalendar::new(SessionSpec::continuous_utc(day_start), None).unwrap()
}

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn d(y: u16, m: u8, day: u8) -> CivilDate {
    CivilDate::new(y, m, day).unwrap()
}

fn eth(cal: &SessionCalendar, at: &str) -> Session {
    cal.session_at(t(at), SessionKind::Eth)
        .unwrap_or_else(|| panic!("no session at {at}"))
}

const HOUR: i64 = 3_600_000;

#[test]
fn local_time_parse_and_display() {
    assert_eq!(LocalTime::parse("17:00"), Some(LocalTime::hm(17, 0)));
    assert_eq!(LocalTime::parse("08:30").unwrap().to_string(), "08:30");
    for bad in ["24:00", "8:30", "08:60", "0830", "ab:cd", ""] {
        assert_eq!(LocalTime::parse(bad), None, "{bad}");
    }
}

#[test]
fn regular_session_and_daily_halt() {
    let cal = equity();
    // Tuesday 2026-11-10 (CST, UTC-6).
    let s = eth(&cal, "2026-11-10T15:00:00Z");
    assert_eq!(s.trade_date, d(2026, 11, 10));
    assert_eq!(s.open, t("2026-11-09T23:00:00Z"), "Monday 17:00 CT");
    assert_eq!(s.close, t("2026-11-10T22:00:00Z"), "Tuesday 16:00 CT");
    assert_eq!((s.halt, s.early_close), (None, false));
    assert_eq!(s.holidays, HolidayData::Verified);

    // The daily 16:00-17:00 CT halt is between sessions.
    let last = UtcMillis(t("2026-11-10T22:00:00Z").0 - 1);
    assert!(cal.is_open(last));
    for at in [
        "2026-11-10T22:00:00Z",
        "2026-11-10T22:30:00Z",
        "2026-11-10T22:59:59Z",
    ] {
        assert!(!cal.is_open(t(at)), "{at}");
        assert_eq!(cal.session_at(t(at), SessionKind::Eth), None, "{at}");
        assert_eq!(cal.next_open(t(at)), Some(t("2026-11-10T23:00:00Z")));
        assert_eq!(
            cal.next_session(t(at), SessionKind::Eth)
                .unwrap()
                .trade_date,
            d(2026, 11, 11)
        );
    }
    assert_eq!(cal.next_close(last), Some(t("2026-11-10T22:00:00Z")));
    // 17:00 CT opens Wednesday's session.
    let s = eth(&cal, "2026-11-10T23:00:00Z");
    assert_eq!(s.trade_date, d(2026, 11, 11));
    assert!(cal.is_open(t("2026-11-10T23:00:00Z")));

    // RTH 08:30-15:00 CT on the trade date.
    let r = cal
        .session_at(t("2026-11-10T15:00:00Z"), SessionKind::Rth)
        .unwrap();
    assert_eq!(r.trade_date, d(2026, 11, 10));
    assert_eq!(
        (r.open, r.close),
        (t("2026-11-10T14:30:00Z"), t("2026-11-10T21:00:00Z"))
    );
    assert_eq!(
        cal.session_at(t("2026-11-10T21:30:00Z"), SessionKind::Rth),
        None
    );
    assert_eq!(
        cal.next_session(t("2026-11-10T21:30:00Z"), SessionKind::Rth)
            .unwrap()
            .trade_date,
        d(2026, 11, 11)
    );
}

#[test]
fn sunday_open_belongs_to_monday() {
    let cal = equity();
    // Saturday and Sunday before 17:00 CT: closed; next session is Monday's.
    for at in ["2026-11-07T12:00:00Z", "2026-11-08T22:59:59Z"] {
        assert!(!cal.is_open(t(at)), "{at}");
        assert_eq!(cal.session_at(t(at), SessionKind::Eth), None);
        assert_eq!(cal.next_open(t(at)), Some(t("2026-11-08T23:00:00Z")));
    }
    // Friday close → Sunday 17:00 CST open.
    assert_eq!(
        eth(&cal, "2026-11-06T21:59:00Z").close,
        t("2026-11-06T22:00:00Z")
    );
    let s = eth(&cal, "2026-11-08T23:30:00Z");
    assert_eq!(
        s.trade_date,
        d(2026, 11, 9),
        "Sunday evening is Monday's session"
    );
    assert_eq!(s.open, t("2026-11-08T23:00:00Z"));
    assert_eq!(s.close, t("2026-11-09T22:00:00Z"));
    assert_eq!(
        cal.session_at(t("2026-11-08T23:30:00Z"), SessionKind::Rth),
        None
    );
    assert!(
        cal.session(d(2026, 11, 8), SessionKind::Eth).is_none(),
        "Sunday is no trade date"
    );
    assert!(cal.session(d(2026, 11, 7), SessionKind::Eth).is_none());
}

#[test]
fn dst_spring_forward() {
    // US DST starts Sunday 2026-03-08 02:00 CST. Friday closes 16:00 CST (22:00Z); Sunday opens
    // 17:00 CDT (22:00Z); Monday closes 16:00 CDT (21:00Z): a 23-hour session.
    let cal = equity();
    assert_eq!(
        eth(&cal, "2026-03-06T21:00:00Z").close,
        t("2026-03-06T22:00:00Z")
    );
    let s = cal.session(d(2026, 3, 9), SessionKind::Eth).unwrap();
    assert_eq!(s.open, t("2026-03-08T22:00:00Z"));
    assert_eq!(s.close, t("2026-03-09T21:00:00Z"));
    assert_eq!(s.close.0 - s.open.0, 23 * HOUR);
    assert_eq!(cal.next_open(t("2026-03-07T00:00:00Z")), Some(s.open));
    let r = cal.session(d(2026, 3, 9), SessionKind::Rth).unwrap();
    assert_eq!(
        (r.open, r.close),
        (t("2026-03-09T13:30:00Z"), t("2026-03-09T20:00:00Z"))
    );
    // A 24/7 session whose local day contains the change is 23 hours long.
    let day = SessionCalendar::new(
        SessionSpec {
            time_zone: CT,
            hours: Hours::Continuous {
                day_start: LocalTime::MIDNIGHT,
            },
            holidays: None,
        },
        None,
    )
    .unwrap();
    let s = day
        .session_at(t("2026-03-08T12:00:00Z"), SessionKind::Eth)
        .unwrap();
    assert_eq!(s.trade_date, d(2026, 3, 8));
    assert_eq!(
        (s.open, s.close),
        (t("2026-03-08T06:00:00Z"), t("2026-03-09T05:00:00Z"))
    );
}

#[test]
fn dst_fall_back() {
    // US DST ends Sunday 2026-11-01 02:00 CDT. Friday closes 16:00 CDT (21:00Z); Sunday opens
    // 17:00 CST (23:00Z); Monday closes 16:00 CST (22:00Z).
    let cal = equity();
    assert_eq!(
        eth(&cal, "2026-10-30T20:00:00Z").close,
        t("2026-10-30T21:00:00Z")
    );
    let s = cal.session(d(2026, 11, 2), SessionKind::Eth).unwrap();
    assert_eq!(s.open, t("2026-11-01T23:00:00Z"));
    assert_eq!(s.close, t("2026-11-02T22:00:00Z"));
    assert_eq!(s.close.0 - s.open.0, 23 * HOUR);
    // The repeated hour 01:00-02:00 local on Sunday: closed either way.
    assert!(!cal.is_open(t("2026-11-01T06:30:00Z")));
    assert!(!cal.is_open(t("2026-11-01T07:30:00Z")));
    let day = SessionCalendar::new(
        SessionSpec {
            time_zone: CT,
            hours: Hours::Continuous {
                day_start: LocalTime::MIDNIGHT,
            },
            holidays: None,
        },
        None,
    )
    .unwrap();
    let s = day.session(d(2026, 11, 1), SessionKind::Eth).unwrap();
    assert_eq!(
        (s.open, s.close),
        (t("2026-11-01T05:00:00Z"), t("2026-11-02T06:00:00Z"))
    );
    assert_eq!(s.close.0 - s.open.0, 25 * HOUR);
}

#[test]
fn holiday_halt_rolls_into_next_trade_date() {
    // MLK Day, Monday 2026-01-19: open Sunday 17:00 CT, halt 12:00 CT Monday, reopen 17:00 CT;
    // all of it is Tuesday's session.
    let cal = equity();
    assert!(!cal.is_trade_date(d(2026, 1, 19)));
    assert!(cal.session(d(2026, 1, 19), SessionKind::Eth).is_none());
    let s = cal.session(d(2026, 1, 20), SessionKind::Eth).unwrap();
    assert_eq!(s.open, t("2026-01-18T23:00:00Z"));
    assert_eq!(
        s.halt,
        Some((t("2026-01-19T18:00:00Z"), t("2026-01-19T23:00:00Z")))
    );
    assert_eq!(s.close, t("2026-01-20T22:00:00Z"));
    assert_eq!(
        eth(&cal, "2026-01-19T16:00:00Z"),
        s,
        "Monday morning is Tuesday's session"
    );
    assert!(cal.is_open(t("2026-01-19T16:00:00Z")));
    assert_eq!(
        cal.next_close(t("2026-01-19T16:00:00Z")),
        Some(t("2026-01-19T18:00:00Z"))
    );
    // In the holiday halt: inside the session's span, but closed.
    let mid = t("2026-01-19T20:00:00Z");
    assert_eq!(cal.session_at(mid, SessionKind::Eth), Some(s));
    assert!(!cal.is_open(mid));
    assert_eq!(cal.next_open(mid), Some(t("2026-01-19T23:00:00Z")));
    assert_eq!(cal.next_close(mid), Some(s.close));
    // No RTH on the holiday.
    assert_eq!(
        cal.session_at(t("2026-01-19T16:00:00Z"), SessionKind::Rth),
        None
    );
    assert_eq!(
        cal.next_session(t("2026-01-19T16:00:00Z"), SessionKind::Rth)
            .unwrap()
            .trade_date,
        d(2026, 1, 20)
    );
    // The previous session (Friday) is untouched.
    assert_eq!(
        cal.session(d(2026, 1, 16), SessionKind::Eth).unwrap().halt,
        None
    );
}

#[test]
fn thanksgiving_halt_then_early_close() {
    let cal = equity();
    let s = cal.session(d(2026, 11, 27), SessionKind::Eth).unwrap();
    assert_eq!(s.open, t("2026-11-25T23:00:00Z"), "Wednesday 17:00 CT");
    assert_eq!(
        s.halt,
        Some((t("2026-11-26T18:00:00Z"), t("2026-11-26T23:00:00Z")))
    );
    assert_eq!(s.close, t("2026-11-27T18:15:00Z"), "Friday 12:15 CT");
    assert!(s.early_close);
    let r = cal.session(d(2026, 11, 27), SessionKind::Rth).unwrap();
    assert_eq!(
        (r.open, r.close),
        (t("2026-11-27T14:30:00Z"), t("2026-11-27T18:15:00Z"))
    );
    assert!(r.early_close);
}

#[test]
fn closed_holiday_and_early_close() {
    // Christmas Eve 2026 (Thursday) closes 12:15 CT; Christmas (Friday) closed; reopen Sunday.
    let cal = equity();
    let eve = cal.session(d(2026, 12, 24), SessionKind::Eth).unwrap();
    assert_eq!(eve.close, t("2026-12-24T18:15:00Z"));
    assert!(eve.early_close);
    assert!(!cal.is_open(t("2026-12-24T18:15:00Z")));
    assert!(
        !cal.is_open(t("2026-12-24T23:30:00Z")),
        "no Thursday evening open"
    );
    assert!(!cal.is_open(t("2026-12-25T16:00:00Z")));
    assert!(cal.session(d(2026, 12, 25), SessionKind::Eth).is_none());
    assert_eq!(
        cal.next_open(t("2026-12-24T18:15:00Z")),
        Some(t("2026-12-27T23:00:00Z"))
    );
    let mon = eth(&cal, "2026-12-27T23:00:00Z");
    assert_eq!(mon.trade_date, d(2026, 12, 28));
    assert_eq!(mon.halt, None);
    assert_eq!(cal.holiday(d(2026, 12, 25)).unwrap().name, "Christmas");
    // A regular day is not an early close.
    assert!(
        !cal.session(d(2026, 12, 23), SessionKind::Eth)
            .unwrap()
            .early_close
    );
}

#[test]
fn year_boundary() {
    // 2027-01-01 (Friday) closed: Thursday 2026-12-31 is a full day, next open Sunday 2027-01-03.
    let cal = equity();
    let from = t("2026-12-29T00:00:00Z");
    let to = t("2027-01-06T00:00:00Z");
    let dates: Vec<_> = cal
        .sessions(from, to, SessionKind::Eth)
        .map(|s| s.trade_date)
        .collect();
    assert_eq!(
        dates,
        [
            d(2026, 12, 29),
            d(2026, 12, 30),
            d(2026, 12, 31),
            d(2027, 1, 4),
            d(2027, 1, 5),
            d(2027, 1, 6)
        ]
    );
    let nye = cal.session(d(2026, 12, 31), SessionKind::Eth).unwrap();
    assert_eq!(nye.close, t("2026-12-31T22:00:00Z"));
    assert_eq!(cal.next_open(nye.close), Some(t("2027-01-03T23:00:00Z")));
    assert_eq!(eth(&cal, "2027-01-03T23:00:00Z").trade_date, d(2027, 1, 4));
    let rth: Vec<_> = cal
        .sessions(from, to, SessionKind::Rth)
        .map(|s| s.trade_date)
        .collect();
    assert_eq!(
        rth,
        [
            d(2026, 12, 29),
            d(2026, 12, 30),
            d(2026, 12, 31),
            d(2027, 1, 4),
            d(2027, 1, 5)
        ]
    );
}

#[test]
fn holiday_data_quality_is_reported() {
    let cal = equity();
    let s = |y, m, day| {
        cal.session(d(y, m, day), SessionKind::Eth)
            .unwrap()
            .holidays
    };
    assert_eq!(s(2026, 11, 10), HolidayData::Verified);
    assert_eq!(
        s(2026, 4, 6),
        HolidayData::Unverified,
        "Monday after an unverified Good Friday"
    );
    assert_eq!(s(2025, 11, 10), HolidayData::NotCovered);
    assert_eq!(s(2028, 1, 3), HolidayData::NotCovered);
    // Outside coverage, weekdays are regular (2025-12-25 is not in the table).
    assert!(cal.is_trade_date(d(2025, 12, 25)));
    // Unverified coverage: every covered session is Unverified, holiday or not.
    let loose =
        HolidayTable::parse(&TABLE.replace("2026 2027 verified", "2026 2027 unverified")).unwrap();
    let loose = SessionCalendar::new(
        SessionSpec {
            time_zone: CT,
            hours: EQUITY_HOURS,
            holidays: Some("test-equity"),
        },
        Some(&loose),
    )
    .unwrap();
    assert_eq!(
        loose
            .session(d(2026, 11, 10), SessionKind::Eth)
            .unwrap()
            .holidays,
        HolidayData::Unverified
    );
    // Without any table, everything is NotCovered.
    let bare = SessionCalendar::new(
        SessionSpec {
            time_zone: CT,
            hours: EQUITY_HOURS,
            holidays: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(
        bare.session(d(2026, 12, 25), SessionKind::Eth)
            .unwrap()
            .holidays,
        HolidayData::NotCovered
    );
}

#[test]
fn crypto_is_24_7() {
    let cal = crypto(LocalTime::MIDNIGHT);
    for at in [
        "2026-11-07T12:00:00Z",
        "2026-12-25T00:00:00Z",
        "2026-03-08T08:00:00Z",
    ] {
        assert!(cal.is_open(t(at)), "{at}");
    }
    assert_eq!(cal.next_open(t("2026-11-07T12:00:00Z")), None);
    assert_eq!(cal.next_close(t("2026-11-07T12:00:00Z")), None);
    assert!(!cal.has_rth());
    assert_eq!(
        cal.session_at(t("2026-11-07T12:00:00Z"), SessionKind::Rth),
        None
    );
    let s = eth(&cal, "2026-11-07T23:59:59Z");
    assert_eq!(s.trade_date, d(2026, 11, 7), "Saturday is a session too");
    assert_eq!(
        (s.open, s.close),
        (t("2026-11-07T00:00:00Z"), t("2026-11-08T00:00:00Z"))
    );
    assert_eq!(s.holidays, HolidayData::NotApplicable);
    assert_eq!(eth(&cal, "2026-11-08T00:00:00Z").trade_date, d(2026, 11, 8));
    // Configurable day start: 22:00 UTC. The day ending at 22:00 on the 8th is the 8th's.
    let cal = crypto(LocalTime::hm(22, 0));
    let s = eth(&cal, "2026-11-07T22:00:00Z");
    assert_eq!(s.trade_date, d(2026, 11, 8));
    assert_eq!(
        (s.open, s.close),
        (t("2026-11-07T22:00:00Z"), t("2026-11-08T22:00:00Z"))
    );
    assert_eq!(eth(&cal, "2026-11-07T21:59:59Z").trade_date, d(2026, 11, 7));
    let n = cal
        .sessions(
            t("2026-11-01T00:00:00Z"),
            t("2026-12-01T00:00:00Z"),
            SessionKind::Eth,
        )
        .count();
    assert_eq!(
        n, 31,
        "30 days in November touch 31 sessions starting 22:00"
    );
}

#[test]
fn validation() {
    let tbl = table();
    let spec = |hours, holidays| SessionSpec {
        time_zone: CT,
        hours,
        holidays,
    };
    let ex = |eth_open, eth_close, rth| {
        Hours::Exchange(ExchangeHours {
            eth_open,
            eth_close,
            rth,
        })
    };
    let h = LocalTime::hm;
    let err = |s: SessionSpec, t: &HolidayTable| SessionCalendar::new(s, Some(t)).unwrap_err();
    assert!(matches!(
        SessionCalendar::new(
            SessionSpec {
                time_zone: "Mars/Olympus",
                ..spec(EQUITY_HOURS, None)
            },
            None
        ),
        Err(SessionError::UnknownTimeZone(_))
    ));
    assert!(matches!(
        err(spec(EQUITY_HOURS, Some("nope")), &tbl),
        SessionError::UnknownHolidayCalendar(_)
    ));
    assert!(matches!(
        SessionCalendar::new(spec(EQUITY_HOURS, Some("test-equity")), None),
        Err(SessionError::UnknownHolidayCalendar(_))
    ));
    assert!(matches!(
        err(spec(ex(h(16, 0), h(17, 0), None), None), &tbl),
        SessionError::BadHours(_)
    ));
    assert!(matches!(
        err(
            spec(ex(h(17, 0), h(16, 0), Some((h(8, 30), h(16, 30)))), None),
            &tbl
        ),
        SessionError::BadHours(_)
    ));
    assert!(matches!(
        err(
            spec(
                Hours::Continuous { day_start: h(0, 0) },
                Some("test-equity")
            ),
            &tbl
        ),
        SessionError::BadHours(_)
    ));
    let with = |rows: &str| {
        HolidayTable::parse(&format!(
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 verified\nsource s\n{rows}"
        ))
        .unwrap()
    };
    let bad_holiday = |rows: &str| {
        matches!(
            err(spec(EQUITY_HOURS, Some("c")), &with(rows)),
            SessionError::BadHoliday { .. }
        )
    };
    assert!(bad_holiday(
        "2026-11-26 halt 12:00 verified A\n2026-11-27 halt 12:00 verified B\n"
    ));
    assert!(bad_holiday(
        "2026-11-27 halt 12:00 verified A\n2026-11-30 halt 12:00 verified B\n"
    ));
    assert!(bad_holiday("2026-11-27 early 16:00 verified A\n"));
    assert!(bad_holiday("2026-11-27 halt 17:00 verified A\n"));
    // An early close before the RTH open leaves the day without RTH.
    let gf = SessionCalendar::new(
        spec(EQUITY_HOURS, Some("c")),
        Some(&with("2026-04-03 early 08:15 verified Good Friday\n")),
    )
    .unwrap();
    let e = gf.session(d(2026, 4, 3), SessionKind::Eth).unwrap();
    assert_eq!(e.close, t("2026-04-03T13:15:00Z"));
    assert!(gf.session(d(2026, 4, 3), SessionKind::Rth).is_none());
    assert_eq!(
        gf.next_session(t("2026-04-03T12:00:00Z"), SessionKind::Rth)
            .unwrap()
            .trade_date,
        d(2026, 4, 6)
    );
    // A halt next to a closure is fine (one internal break).
    assert!(
        SessionCalendar::new(
            spec(EQUITY_HOURS, Some("c")),
            Some(&with(
                "2026-11-26 halt 12:00 verified A\n2026-11-27 closed verified B\n"
            ))
        )
        .is_ok()
    );
}

#[test]
fn halt_before_a_closed_day_and_weekend() {
    // Thursday halt, Friday closed: Monday's session holds Thursday morning.
    let tbl = HolidayTable::parse(
        "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 verified\nsource s\n\
         2026-11-26 halt 12:00 verified A\n2026-11-27 closed verified B\n",
    )
    .unwrap();
    let cal = SessionCalendar::new(
        SessionSpec {
            time_zone: CT,
            hours: EQUITY_HOURS,
            holidays: Some("c"),
        },
        Some(&tbl),
    )
    .unwrap();
    let s = cal.session(d(2026, 11, 30), SessionKind::Eth).unwrap();
    assert_eq!(s.open, t("2026-11-25T23:00:00Z"));
    assert_eq!(
        s.halt,
        Some((t("2026-11-26T18:00:00Z"), t("2026-11-29T23:00:00Z")))
    );
    assert_eq!(
        eth(&cal, "2026-11-26T15:00:00Z").trade_date,
        d(2026, 11, 30)
    );
}

/// Deterministic xorshift for property tests (no extra dependency).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }
}

fn calendars() -> Vec<SessionCalendar> {
    vec![
        equity(),
        crypto(LocalTime::MIDNIGHT),
        crypto(LocalTime::hm(17, 30)),
    ]
}

#[test]
fn property_sessions_are_monotonic_and_non_overlapping() {
    let lo = t("2025-06-01T00:00:00Z").0;
    let hi = t("2028-06-01T00:00:00Z").0;
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    for cal in calendars() {
        for kind in [SessionKind::Eth, SessionKind::Rth] {
            for _ in 0..60 {
                let from = UtcMillis(rng.range(lo, hi));
                let to = UtcMillis(from.0 + rng.range(HOUR, 40 * 24 * HOUR));
                let mut prev: Option<Session> = None;
                for s in cal.sessions(from, to, kind) {
                    assert!(s.open < s.close, "{s:?}");
                    assert!(s.close > from && s.open < to, "overlaps the range: {s:?}");
                    assert_eq!(s.kind, kind);
                    if let Some((a, b)) = s.halt {
                        assert!(s.open < a && a < b && b < s.close, "{s:?}");
                    }
                    // The trade date is the local date of the session's last instant.
                    let (date, _) = cal.local(UtcMillis(s.close.0 - 1));
                    assert_eq!(date, s.trade_date, "{s:?}");
                    assert_eq!(cal.session(s.trade_date, kind), Some(s));
                    if let Some(p) = prev {
                        assert!(p.close <= s.open, "overlap: {p:?} / {s:?}");
                        assert!(p.trade_date < s.trade_date);
                        // No trade date is skipped.
                        let mut x = p.trade_date.add_days(1);
                        while x < s.trade_date {
                            assert!(cal.session(x, kind).is_none(), "skipped {x}");
                            x = x.add_days(1);
                        }
                    }
                    if kind == SessionKind::Rth {
                        let e = cal.session(s.trade_date, SessionKind::Eth).unwrap();
                        assert!(e.open <= s.open && s.close <= e.close, "{s:?} in {e:?}");
                    }
                    prev = Some(s);
                }
            }
        }
    }
}

#[test]
fn property_point_queries_agree() {
    let lo = t("2025-06-01T00:00:00Z").0;
    let hi = t("2028-06-01T00:00:00Z").0;
    let mut rng = Rng(0x0dd_ba11_cafe_f00d);
    for cal in calendars() {
        for _ in 0..3000 {
            let at = UtcMillis(rng.range(lo, hi));
            let next = cal.next_session(at, SessionKind::Eth).unwrap();
            assert!(at < next.close);
            match cal.session_at(at, SessionKind::Eth) {
                Some(s) => {
                    assert_eq!(s, next);
                    assert!(s.contains(at));
                    assert_eq!(cal.is_open(at), s.is_open(at));
                }
                None => {
                    assert!(at < next.open);
                    assert!(!cal.is_open(at));
                }
            }
            if let Some(o) = cal.next_open(at) {
                assert!(o > at);
                assert!(cal.is_open(o));
                assert!(!cal.is_open(UtcMillis(o.0 - 1)));
            }
            if let Some(c) = cal.next_close(at) {
                assert!(c > at);
                assert!(!cal.is_open(c));
                assert!(cal.is_open(UtcMillis(c.0 - 1)));
                // Nothing closes in between.
                if cal.is_open(at) {
                    let mid = UtcMillis(at.0 + (c.0 - at.0) / 2);
                    assert!(cal.is_open(mid));
                }
            }
            if let Some(r) = cal.session_at(at, SessionKind::Rth) {
                assert!(cal.is_open(at), "RTH is within open ETH: {r:?}");
            }
        }
    }
}
