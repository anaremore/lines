//! The holiday table: exchange holidays and early closes as versioned data, not code.
//!
//! Lines ships one table (`lines-feed/data/exchange-holidays.txt`); this module parses and
//! validates it. Parsing is pure (`&str` in, no I/O) and happens once at load, never on a
//! query path.
//!
//! # Format (version 1)
//!
//! Line based. Blank lines and lines starting with `#` are ignored. Directives:
//!
//! ```text
//! format 1                       # required, first directive
//! version 2026-11-06             # required: date of the data revision
//!
//! calendar cme-equity            # starts a calendar; name is [a-z0-9-]+, unique
//! coverage 2026 2027 unverified  # required: years the list is meant to be complete for, and
//!                                # whether that completeness was checked
//! source https://...             # at least one per calendar
//! 2026-01-01 closed       verified   New Year's Day
//! 2026-01-19 halt 12:00   verified   Martin Luther King Jr. Day
//! 2026-11-27 early 12:15  unverified Day after Thanksgiving
//! ```
//!
//! Day rules (times are exchange-local, in the calendar's time zone):
//! - `closed`: no trading on that calendar day and no trade date. The evening open for the
//!   next trade date happens as usual.
//! - `halt HH:MM`: trading runs from the usual evening open until `HH:MM`, then halts; the day
//!   is not a trade date, so its trades belong to the *next* trade date's session.
//! - `early HH:MM`: a normal trade date that closes at `HH:MM`.
//!
//! Every entry says whether its date was checked against the exchange's own publication
//! (`verified`) or not (`unverified`); so does the coverage (an unchecked list may be missing a
//! day). Unverified data is applied, and the sessions it shapes are flagged
//! ([`HolidayData::Unverified`](super::HolidayData)): with unverified coverage, every session
//! in it is.
//!
//! Validation: dates are weekdays, strictly increasing within a calendar and inside its
//! coverage; times are valid; every calendar has a coverage and a source.

use std::fmt;

use super::LocalTime;
use crate::calendar::CivilDate;

/// What happens on a holiday or special day.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DayRule {
    /// No trading at all on this calendar day, and no trade date.
    Closed,
    /// Trading until this local time, then halted; not a trade date (its trades belong to the
    /// next trade date).
    Halt(LocalTime),
    /// A trade date that closes early at this local time.
    EarlyClose(LocalTime),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holiday {
    pub date: CivilDate,
    pub rule: DayRule,
    /// Checked against the exchange's own published calendar.
    pub verified: bool,
    pub name: String,
}

/// One exchange calendar (e.g. CME equity index products).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolidayCalendar {
    pub name: String,
    /// First and last year (inclusive) whose holidays are complete.
    pub coverage: (u16, u16),
    /// The list was checked to be complete for the coverage years.
    pub coverage_verified: bool,
    pub sources: Vec<String>,
    /// Sorted by date, unique.
    pub days: Vec<Holiday>,
}

impl HolidayCalendar {
    pub fn covers(&self, date: CivilDate) -> bool {
        (self.coverage.0..=self.coverage.1).contains(&date.year)
    }

    /// The special rule for `date`, if any. Binary search, no allocation.
    pub fn get(&self, date: CivilDate) -> Option<&Holiday> {
        self.days
            .binary_search_by_key(&date, |h| h.date)
            .ok()
            .map(|i| &self.days[i])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolidayTable {
    /// Data revision.
    pub version: CivilDate,
    pub calendars: Vec<HolidayCalendar>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableError {
    /// 1-based line number (0 for whole-file problems).
    pub line: usize,
    pub message: String,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "holiday table: {}", self.message)
        } else {
            write!(f, "holiday table line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for TableError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, TableError> {
    Err(TableError {
        line,
        message: message.into(),
    })
}

pub(crate) fn parse_date(s: &str) -> Option<CivilDate> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    CivilDate::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

impl HolidayTable {
    pub fn calendar(&self, name: &str) -> Option<&HolidayCalendar> {
        self.calendars.iter().find(|c| c.name == name)
    }

    /// Parse and validate a table.
    pub fn parse(text: &str) -> Result<HolidayTable, TableError> {
        let mut format_seen = false;
        let mut version = None;
        let mut calendars: Vec<HolidayCalendar> = Vec::new();
        // Line on which the current calendar started, for whole-calendar errors.
        let mut cal_line = 0;

        let finish = |c: &HolidayCalendar, line: usize| -> Result<(), TableError> {
            if c.coverage == (0, 0) {
                return err(line, format!("calendar {} has no coverage", c.name));
            }
            if c.sources.is_empty() {
                return err(line, format!("calendar {} has no source", c.name));
            }
            Ok(())
        };

        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let head = words.next().unwrap_or_default();
            if !format_seen {
                if head != "format" {
                    return err(n, "the first directive must be `format 1`");
                }
                if words.next() != Some("1") || words.next().is_some() {
                    return err(n, "unsupported format (expected `format 1`)");
                }
                format_seen = true;
                continue;
            }
            match head {
                "format" => return err(n, "duplicate `format`"),
                "version" => {
                    if version.is_some() {
                        return err(n, "duplicate `version`");
                    }
                    let Some(d) = words.next().and_then(parse_date) else {
                        return err(n, "`version` needs a YYYY-MM-DD date");
                    };
                    version = Some(d);
                }
                "calendar" => {
                    let Some(name) = words.next() else {
                        return err(n, "`calendar` needs a name");
                    };
                    if words.next().is_some()
                        || !name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    {
                        return err(n, format!("bad calendar name {name:?} (use [a-z0-9-])"));
                    }
                    if calendars.iter().any(|c| c.name == name) {
                        return err(n, format!("duplicate calendar {name}"));
                    }
                    if let Some(prev) = calendars.last() {
                        finish(prev, cal_line)?;
                    }
                    cal_line = n;
                    calendars.push(HolidayCalendar {
                        name: name.to_owned(),
                        coverage: (0, 0),
                        coverage_verified: false,
                        sources: Vec::new(),
                        days: Vec::new(),
                    });
                }
                "coverage" | "source" => {
                    let Some(cal) = calendars.last_mut() else {
                        return err(n, format!("`{head}` outside a calendar"));
                    };
                    if head == "source" {
                        let url = line["source".len()..].trim();
                        if url.is_empty() {
                            return err(n, "`source` needs a reference");
                        }
                        cal.sources.push(url.to_owned());
                        continue;
                    }
                    if cal.coverage != (0, 0) || !cal.days.is_empty() {
                        return err(n, "`coverage` must come once, before the dates");
                    }
                    let parts: Vec<&str> = words.collect();
                    let [a, b, status] = parts[..] else {
                        return err(n, "`coverage` needs: first last verified|unverified");
                    };
                    match (a.parse::<u16>(), b.parse::<u16>()) {
                        (Ok(a), Ok(b)) if (1900..=2999).contains(&a) && a <= b && b <= 2999 => {
                            cal.coverage = (a, b)
                        }
                        _ => return err(n, "`coverage` needs two years, first <= last"),
                    }
                    cal.coverage_verified = match status {
                        "verified" => true,
                        "unverified" => false,
                        _ => return err(n, "coverage status must be `verified` or `unverified`"),
                    };
                }
                date if date.starts_with(|c: char| c.is_ascii_digit()) => {
                    let Some(cal) = calendars.last_mut() else {
                        return err(n, "date outside a calendar");
                    };
                    let Some(date) = parse_date(date) else {
                        return err(n, format!("bad date {date:?}"));
                    };
                    if cal.coverage == (0, 0) {
                        return err(n, "`coverage` must come before the dates");
                    }
                    if !cal.covers(date) {
                        return err(n, format!("{date} is outside the coverage"));
                    }
                    if !date.is_business_day() {
                        return err(n, format!("{date} is a weekend day"));
                    }
                    if cal.days.last().is_some_and(|h| h.date >= date) {
                        return err(n, format!("{date} is not after the previous date"));
                    }
                    let rule = match words.next() {
                        Some("closed") => DayRule::Closed,
                        Some(kind @ ("halt" | "early")) => {
                            let Some(t) = words.next().and_then(LocalTime::parse) else {
                                return err(n, format!("`{kind}` needs a time HH:MM"));
                            };
                            if kind == "halt" {
                                DayRule::Halt(t)
                            } else {
                                DayRule::EarlyClose(t)
                            }
                        }
                        other => {
                            return err(n, format!("unknown rule {other:?} (closed, halt, early)"));
                        }
                    };
                    let verified = match words.next() {
                        Some("verified") => true,
                        Some("unverified") => false,
                        _ => return err(n, "status must be `verified` or `unverified`"),
                    };
                    let name = words.collect::<Vec<_>>().join(" ");
                    if name.is_empty() {
                        return err(n, "a holiday needs a name");
                    }
                    cal.days.push(Holiday {
                        date,
                        rule,
                        verified,
                        name,
                    });
                }
                other => return err(n, format!("unknown directive {other:?}")),
            }
        }
        if !format_seen {
            return err(0, "empty table (no `format 1`)");
        }
        let Some(version) = version else {
            return err(0, "missing `version`");
        };
        if let Some(last) = calendars.last() {
            finish(last, cal_line)?;
        }
        Ok(HolidayTable { version, calendars })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = "\
# comment
format 1
version 2026-11-06

calendar test-a
coverage 2026 2027 verified
source https://example.invalid/holidays#x
2026-01-01 closed verified New Year's Day
2026-01-19 halt 12:00 verified   Martin Luther King Jr. Day
2026-11-27 early 12:15 unverified Day after Thanksgiving

calendar test-b
coverage 2026 2026 unverified
source somewhere
";

    fn d(y: u16, m: u8, day: u8) -> CivilDate {
        CivilDate::new(y, m, day).unwrap()
    }

    #[test]
    fn parses_a_valid_table() {
        let t = HolidayTable::parse(OK).unwrap();
        assert_eq!(t.version, d(2026, 11, 6));
        let a = t.calendar("test-a").unwrap();
        assert_eq!(a.coverage, (2026, 2027));
        assert!(a.coverage_verified);
        assert!(!t.calendar("test-b").unwrap().coverage_verified);
        assert_eq!(a.sources, ["https://example.invalid/holidays#x"]);
        assert_eq!(a.days.len(), 3);
        assert_eq!(a.days[1].rule, DayRule::Halt(LocalTime::hm(12, 0)));
        assert_eq!(a.days[1].name, "Martin Luther King Jr. Day");
        assert!(!a.days[2].verified);
        assert_eq!(
            a.get(d(2026, 11, 27)).unwrap().rule,
            DayRule::EarlyClose(LocalTime::hm(12, 15))
        );
        assert!(a.get(d(2026, 11, 26)).is_none());
        assert!(a.covers(d(2027, 12, 31)) && !a.covers(d(2028, 1, 1)));
        assert!(t.calendar("test-b").unwrap().days.is_empty());
        assert!(t.calendar("nope").is_none());
    }

    #[track_caller]
    fn rejects(text: &str, want_line: usize, needle: &str) {
        let e = HolidayTable::parse(text).unwrap_err();
        assert_eq!(e.line, want_line, "{e}");
        assert!(e.message.contains(needle), "{e}");
    }

    #[test]
    fn rejects_invalid_tables() {
        let head =
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 verified\nsource s\n";
        rejects("", 0, "empty");
        rejects("format 2\n", 1, "unsupported");
        rejects("version 2026-01-01\n", 1, "first directive");
        rejects("format 1\n", 0, "missing `version`");
        rejects("format 1\nversion 2026-13-01\n", 2, "YYYY-MM-DD");
        rejects(
            "format 1\nversion 2026-11-06\n2026-01-01 closed verified X\n",
            3,
            "outside a calendar",
        );
        rejects(
            "format 1\nversion 2026-11-06\ncalendar C\n",
            3,
            "bad calendar name",
        );
        rejects(
            "format 1\nversion 2026-11-06\ncalendar c\nsource s\n",
            3,
            "no coverage",
        );
        rejects(
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 verified\n",
            3,
            "no source",
        );
        rejects(
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 verified\ncalendar d\n",
            3,
            "no source",
        );
        rejects(&format!("{head}calendar c\n"), 6, "duplicate calendar");
        rejects(&format!("{head}coverage 2026 2027 verified\n"), 6, "once");
        rejects(
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2027 2026 verified\n",
            4,
            "first <= last",
        );
        rejects(
            &format!("{head}2026-01-1 closed verified X\n"),
            6,
            "bad date",
        );
        rejects(
            &format!("{head}2027-01-01 closed verified X\n"),
            6,
            "outside the coverage",
        );
        rejects(
            &format!("{head}2026-01-03 closed verified X\n"),
            6,
            "weekend",
        );
        rejects(
            &format!("{head}2026-01-02 closed verified X\n2026-01-01 closed verified Y\n"),
            7,
            "not after",
        );
        rejects(
            &format!("{head}2026-01-02 closed verified X\n2026-01-02 closed verified Y\n"),
            7,
            "not after",
        );
        rejects(
            &format!("{head}2026-01-02 shut verified X\n"),
            6,
            "unknown rule",
        );
        rejects(
            &format!("{head}2026-01-02 halt verified X\n"),
            6,
            "needs a time",
        );
        rejects(
            &format!("{head}2026-01-02 early 24:00 verified X\n"),
            6,
            "needs a time",
        );
        rejects(&format!("{head}2026-01-02 closed maybe X\n"), 6, "status");
        rejects(
            &format!("{head}2026-01-02 closed verified\n"),
            6,
            "needs a name",
        );
        rejects(&format!("{head}frobnicate\n"), 6, "unknown directive");
        rejects(
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026\n",
            4,
            "first last verified|unverified",
        );
        rejects(
            "format 1\nversion 2026-11-06\ncalendar c\ncoverage 2026 2026 maybe\n",
            4,
            "coverage status",
        );
    }
}
