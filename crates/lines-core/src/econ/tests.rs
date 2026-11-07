use super::*;

const MIN: i64 = 60_000;

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn d(y: u16, m: u8, day: u8) -> CivilDate {
    CivilDate::new(y, m, day).unwrap()
}

const SAMPLE: &str = "\
format 1
version 2026-11-01
zone America/New_York

release cpi
name CPI
agency BLS
importance high
affects ES NQ
coverage 2026-03-01 2026-12-31 verified
source https://www.bls.gov/schedule/news_release/cpi.htm
# EST (UTC-5) the Friday before DST starts, EDT (UTC-4) after it.
2026-03-06 08:30 verified February 2026
2026-03-10 08:30 verified February 2026 (again)
2026-10-30 08:30 verified September 2026
2026-11-03 08:30 unverified October 2026

release ppi
name PPI
agency BLS
importance medium
affects ES
coverage 2026-03-01 2026-11-30 unverified
source https://www.bls.gov/schedule/news_release/ppi.htm
2026-11-03 08:30 unverified October 2026
";

fn sample() -> EconSchedule {
    EconSchedule::parse(SAMPLE).unwrap()
}

#[test]
fn eastern_times_follow_dst_both_ways() {
    let s = sample();
    let at: Vec<_> = s.events.iter().map(|e| e.at).collect();
    // EST before the March change, EDT after; EDT before the November change, EST after.
    assert_eq!(at[0], t("2026-03-06T13:30:00Z"));
    assert_eq!(at[1], t("2026-03-10T12:30:00Z"));
    assert_eq!(at[2], t("2026-10-30T12:30:00Z"));
    assert_eq!(at[3], t("2026-11-03T13:30:00Z"));
    // Ties keep release order (CPI before PPI).
    assert_eq!(at[4], t("2026-11-03T13:30:00Z"));
    assert_eq!(s.releases[s.events[3].release].key, "cpi");
    assert_eq!(s.releases[s.events[4].release].key, "ppi");
    assert_eq!(s.local_time(&s.events[1]), LocalTime::hm(8, 30));
    assert_eq!(s.local_time(&s.events[3]), LocalTime::hm(8, 30));
}

#[test]
fn times_in_a_dst_gap_or_fold_are_refused() {
    let base = "format 1\nversion 2026-11-01\nzone America/New_York\nrelease x\nname X\nagency A\nimportance high\naffects ES\ncoverage 2026-01-01 2026-12-31 verified\nsource s\n";
    // 2026-03-08 02:30 never happens; 2026-11-01 01:30 happens twice.
    for ev in [
        "2026-03-08 02:30 verified gap",
        "2026-11-01 01:30 verified fold",
    ] {
        let e = EconSchedule::parse(&format!("{base}{ev}\n")).unwrap_err();
        assert!(e.message.contains("DST"), "{e}");
        assert_eq!(e.line, 11);
    }
    assert!(EconSchedule::parse(&format!("{base}2026-11-01 03:30 verified fine\n")).is_ok());
}

#[test]
fn validation_names_the_line() {
    let cases: &[(&str, &str)] = &[
        ("", "empty"),
        ("version 2026-11-01\n", "first directive"),
        ("format 2\n", "unsupported"),
        ("format 1\nzone Mars/Base\n", "unknown time zone"),
        (
            "format 1\nversion 2026-11-01\nrelease a\n",
            "before the first release",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease A\n",
            "bad release key",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nname x\n",
            "outside a release",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\nname A\n",
            "has no `agency`",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\nimportance huge\n",
            "high, medium or low",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\ncoverage 2026-02-01 2026-01-01 verified\n",
            "ends before",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\nname A\nagency B\nimportance low\naffects ES\ncoverage 2026-01-01 2026-01-31 verified\nsource s\n2026-02-01 08:30 verified late\n",
            "outside the release's coverage",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\nname A\nagency B\nimportance low\naffects ES\ncoverage 2026-01-01 2026-01-31 verified\nsource s\n2026-01-05 08:30 verified b\n2026-01-05 08:30 verified b\n",
            "strictly increasing",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\nname A\nagency B\nimportance low\naffects ES\ncoverage 2026-01-01 2026-01-31 verified\nsource s\n2026-01-05 08:30 maybe b\n",
            "`verified` or `unverified`",
        ),
        (
            "format 1\nversion 2026-11-01\nzone UTC\nrelease a\nname A\nagency B\nimportance low\naffects ES\ncoverage 2026-01-01 2026-01-31 verified\n",
            "has no `source`",
        ),
        ("format 1\nversion 2026-11-01\nzone UTC\n", "no releases"),
    ];
    for (text, want) in cases {
        let e = EconSchedule::parse(text).unwrap_err();
        assert!(e.to_string().contains(want), "{text:?}: {e}");
    }
}

#[test]
fn windows_hold_from_before_to_just_short_of_after() {
    let s = sample();
    let at = t("2026-03-10T12:30:00Z");
    let w = |now: UtcMillis| s.window_at(Some("ES"), now, 2 * MIN, 2 * MIN, Importance::High);
    assert!(w(UtcMillis(at.0 - 2 * MIN - 1)).is_none());
    assert_eq!(w(UtcMillis(at.0 - 2 * MIN)).unwrap().at, at);
    assert_eq!(w(UtcMillis(at.0 + 2 * MIN - 1)).unwrap().at, at);
    assert!(w(UtcMillis(at.0 + 2 * MIN)).is_none());
    // A product the release doesn't move, and a lower-importance release.
    assert!(
        s.window_at(Some("BTC/USD"), at, 2 * MIN, 2 * MIN, Importance::High)
            .is_none()
    );
    let ppi = t("2026-11-03T13:30:00Z");
    let only_ppi_for = |sym| {
        s.window_at(Some(sym), ppi, MIN, MIN, Importance::Medium)
            .map(|e| s.release(e).key.as_str())
    };
    // CPI and PPI share the minute: the earliest by release order is reported.
    assert_eq!(only_ppi_for("ES"), Some("cpi"));
    assert_eq!(only_ppi_for("NQ"), Some("cpi"));
}

#[test]
fn next_window_start_and_next_event() {
    let s = sample();
    let now = t("2026-03-06T13:28:00Z");
    // Exactly at a window start: the next one is the following event's.
    assert_eq!(
        s.next_window_start(Some("ES"), now, 2 * MIN, Importance::High),
        Some(t("2026-03-10T12:28:00Z"))
    );
    assert_eq!(
        s.next_event(Some("ES"), now, Importance::High).unwrap().at,
        t("2026-03-06T13:30:00Z")
    );
    assert_eq!(s.upcoming(t("2026-11-01T00:00:00Z")).count(), 2);
    assert_eq!(
        EconSchedule::minutes_left(now, t("2026-03-06T13:30:00.001Z")),
        3
    );
    assert_eq!(EconSchedule::minutes_left(now, now), 0);
}

#[test]
fn coverage_is_missing_outside_and_stale_when_old() {
    let s = sample();
    assert_eq!(
        s.coverage("ES", t("2026-06-01T12:00:00Z"), 0),
        Coverage::Current
    );
    // PPI (ES only) ends in November; CPI covers December.
    assert_eq!(
        s.coverage("ES", t("2026-12-02T12:00:00Z"), 0),
        Coverage::Missing {
            release: 1,
            date: d(2026, 12, 2)
        }
    );
    assert_eq!(
        s.coverage("NQ", t("2026-12-02T12:00:00Z"), 0),
        Coverage::Current
    );
    // The local date decides: 2026-03-01T04:30Z is 23:30 on February 28 in New York, before
    // coverage.
    assert_eq!(
        s.coverage("NQ", t("2026-03-01T04:30:00Z"), 0),
        Coverage::Missing {
            release: 0,
            date: d(2026, 2, 28)
        }
    );
    // Looking ahead past the end of coverage.
    assert_eq!(
        s.coverage("NQ", t("2026-12-31T23:00:00Z"), 0),
        Coverage::Current
    );
    assert_eq!(
        s.coverage("NQ", t("2027-01-01T04:58:00Z"), 0),
        Coverage::Current,
        "still 23:58 on Dec 31 in New York"
    );
    assert!(matches!(
        s.coverage("NQ", t("2027-01-01T04:58:00Z"), 2 * MIN),
        Coverage::Missing { .. }
    ));
    // Products no release moves have nothing to miss.
    assert_eq!(
        s.coverage("BTC/USD", t("2030-01-01T00:00:00Z"), 0),
        Coverage::Current
    );
    // Stale after STALE_AFTER_DAYS from the version (2026-11-01), if not already missing.
    let old =
        EconSchedule::parse(&SAMPLE.replace("version 2026-11-01", "version 2026-04-01")).unwrap();
    assert_eq!(
        old.coverage("NQ", t("2026-05-31T12:00:00Z"), 0),
        Coverage::Current
    );
    assert_eq!(
        old.coverage("NQ", t("2026-06-01T12:00:00Z"), 0),
        Coverage::Stale {
            checked: d(2026, 4, 1)
        }
    );
}
