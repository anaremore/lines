//! The economic calendar pane's rows and header (roadmap 8b), GPUI-free: which releases a
//! range holds, how each reads (time in the display zone, impact, the account's blackout
//! window, where the line comes from), which one is next, which are past, and which is inside
//! its blackout window now. The pane (`crate::econ_view`) draws them.

use lines_core::UtcMillis;
use lines_core::display_tz::DisplayTz;
use lines_core::econ::{EconEvent, EconSchedule, Importance};
use lines_trading::news::{NewsAction, NewsBlackout};
use serde::{Deserialize, Serialize};

use crate::pane_columns::CalCol;

const DAY: i64 = 86_400_000;

/// What the pane lists: the display zone's day, its week (Monday to Sunday) or its month.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Range {
    Today,
    #[default]
    Week,
    Month,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Today, Range::Week, Range::Month];

    pub fn label(self) -> &'static str {
        match self {
            Range::Today => "Today",
            Range::Week => "Week",
            Range::Month => "Month",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Range::default()
    }

    /// `[from, to)` around `now` in the display zone, each edge at that day's local midnight
    /// (a DST change inside the range moves neither edge).
    pub fn period(self, now: UtcMillis, z: &DisplayTz) -> (UtcMillis, UtcMillis) {
        let local = z.local(now).0;
        let day = local.div_euclid(DAY);
        // Local midnight of day `d` in UTC: the offset at the instant itself (found from a
        // first guess at today's offset; a midnight inside a DST gap lands an hour off).
        let start = |d: i64| {
            let wall = d * DAY;
            let guess = UtcMillis(wall - z.offset_ms(now));
            UtcMillis(wall - z.offset_ms(guess))
        };
        match self {
            Range::Today => (start(day), start(day + 1)),
            Range::Week => {
                // 1970-01-01 was a Thursday: Monday-based weekday.
                let monday = day - (day + 3).rem_euclid(7);
                (start(monday), start(monday + 7))
            }
            Range::Month => {
                let date = lines_core::CivilDate::from_days(day);
                let first = day - (date.day as i64 - 1);
                let n = lines_core::calendar::days_in_month(date.year, date.month) as i64;
                (start(first), start(first + n))
            }
        }
    }
}

/// Where a row stands now.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    /// Released (and its window, if any, over): dimmed.
    Past,
    /// Inside the account's blackout window now.
    Inside,
    /// The first release still to come.
    Next,
    Later,
}

/// One release as the table shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub at: UtcMillis,
    /// Indexed by [`CalCol`].
    pub cells: [String; 5],
    pub state: RowState,
    pub high: bool,
}

/// "07:30" in the display zone.
fn hhmm(z: &DisplayTz, t: UtcMillis) -> String {
    let (_, _, _, h, m, _) = z.civil(t);
    format!("{h:02}:{m:02}")
}

fn impact(i: Importance) -> &'static str {
    match i {
        Importance::High => "High",
        Importance::Medium => "Medium",
        Importance::Low => "Low",
    }
}

/// Where a line comes from: the calendar it was built on has it (bundled, or a screenshot's
/// stand-in), or a refresh added it from FRED at the release's usual time.
pub fn source(base: &EconSchedule, s: &EconSchedule, e: &EconEvent) -> &'static str {
    let key = &s.release(e).key;
    let known = base
        .events
        .iter()
        .find(|b| b.at == e.at && base.release(b).key == *key);
    match known {
        Some(b) if b.verified => "bundled · verified",
        Some(_) => "bundled · unverified",
        None => "FRED · usual time",
    }
}

/// The rows of `range` at `now`: every release in it, whatever the product (the calendar is
/// for every market); the window column under `rule` (none: a dash).
pub fn rows(
    s: &EconSchedule,
    base: &EconSchedule,
    rule: Option<&NewsBlackout>,
    now: UtcMillis,
    range: Range,
    z: &DisplayTz,
) -> Vec<Row> {
    let (from, to) = range.period(now, z);
    let mut next_seen = false;
    s.upcoming(from)
        .take_while(|e| e.at < to)
        .map(|e| {
            let r = s.release(e);
            let counted = rule.filter(|n| r.importance >= n.min_importance());
            let window = counted.map(|n| {
                (
                    UtcMillis(e.at.0 - n.before_ms()),
                    UtcMillis(e.at.0 + n.after_ms()),
                )
            });
            let inside = window.is_some_and(|(a, b)| now >= a && now < b);
            let state = if inside {
                RowState::Inside
            } else if e.at < now {
                RowState::Past
            } else if !next_seen {
                next_seen = true;
                RowState::Next
            } else {
                RowState::Later
            };
            if inside && e.at >= now {
                // Inside its window before the release: it is the next release too.
                next_seen = true;
            }
            let window_text = match window {
                Some((a, b)) if inside => format!("{}–{} · now", hhmm(z, a), hhmm(z, b)),
                Some((a, b)) => format!("{}–{}", hhmm(z, a), hhmm(z, b)),
                None => "—".to_owned(),
            };
            let mut cells: [String; 5] = Default::default();
            cells[CalCol::Time.index()] = crate::palette_lists::when(z, e.at);
            cells[CalCol::Release.index()] = format!("{} · {}", r.name, e.label);
            cells[CalCol::Impact.index()] = impact(r.importance).to_owned();
            cells[CalCol::Window.index()] = window_text;
            cells[CalCol::Source.index()] = source(base, s, e).to_owned();
            Row {
                at: e.at,
                cells,
                state,
                high: r.importance == Importance::High,
            }
        })
        .collect()
}

/// The account's rule in words: "Blackout: block entries ±5 min (Paper 50K eval)", or "No
/// blackout (Paper)"; no account: "No account".
pub fn rule_text(rule: Option<&NewsBlackout>, account: Option<&str>) -> String {
    let Some(name) = account else {
        return "No account".to_owned();
    };
    let Some(r) = rule else {
        return format!("No blackout ({name})");
    };
    let action = match r.action {
        NewsAction::Warn => "warn only",
        NewsAction::Block => "block entries",
        NewsAction::Flatten => "block entries and flatten",
    };
    let window = if r.before_min == r.after_min {
        format!("±{} min", r.before_min)
    } else {
        format!("−{}/+{} min", r.before_min, r.after_min)
    };
    let medium = if r.include_medium {
        ", medium releases too"
    } else {
        ""
    };
    format!("Blackout: {action} {window}{medium} ({name})")
}

/// The calendar's source and freshness in words, and whether it is a problem (stale or
/// failing): "FRED · updated 14:33", "FRED · refresh failed: …", "Bundled calendar of
/// 2026-11-07 · no FRED key", with " · stale" when it is.
pub fn freshness(
    r: &crate::econ_ui::Refresh,
    s: &EconSchedule,
    refreshed: bool,
    now: UtcMillis,
    z: &DisplayTz,
) -> (String, bool) {
    let stale = s.local_date(now).days() - s.version.days() > lines_core::econ::STALE_AFTER_DAYS;
    let st = if stale { " · stale" } else { "" };
    let at = |t: UtcMillis| {
        if z.local(t).0.div_euclid(DAY) == z.local(now).0.div_euclid(DAY) {
            hhmm(z, t)
        } else {
            crate::palette_lists::when(z, t)
        }
    };
    if r.running {
        return (format!("FRED · refreshing…{st}"), stale);
    }
    if let Some(p) = &r.problem {
        return (format!("FRED · refresh failed: {p}{st}"), true);
    }
    if !r.not_this_run
        && !r.no_key
        && let Some(t) = r.updated
    {
        return (format!("FRED · updated {}{st}", at(t)), stale);
    }
    let why = if r.no_key && !r.not_this_run {
        " · no FRED key"
    } else {
        ""
    };
    // A refresh from an earlier run (the cache), not refreshed in this one.
    let what = if refreshed {
        "FRED calendar of"
    } else {
        "Bundled calendar of"
    };
    (format!("{what} {}{why}{st}", s.version), stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::econ_ui::Refresh;

    fn at(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    /// A small calendar: CPI (high) Thu 2026-11-12 08:30 ET, PPI (medium) Fri 11-13, GDP
    /// (medium) Wed 11-25.
    fn sched() -> EconSchedule {
        EconSchedule::parse(
            "format 1
version 2026-11-07
zone America/New_York
release cpi
name CPI
agency BLS
importance high
affects ES
coverage 2026-11-01 2026-12-31 verified
source https://www.bls.gov/
2026-11-12 08:30 verified October 2026
release ppi
name PPI
agency BLS
importance medium
affects ES
coverage 2026-11-01 2026-12-31 unverified
source https://www.bls.gov/
2026-11-13 08:30 unverified October 2026
release gdp
name GDP
agency BEA
importance medium
affects ES
coverage 2026-11-01 2026-12-31 verified
source https://www.bea.gov/
2026-11-25 08:30 verified Q3 2026
",
        )
        .unwrap()
    }

    fn chicago() -> DisplayTz {
        DisplayTz::resolve(&lines_core::display_tz::ZoneChoice::from_saved(Some(
            "America/Chicago",
        )))
        .unwrap()
    }

    #[test]
    fn ranges_are_the_zones_day_week_and_month() {
        let z = chicago();
        // Thu 2026-11-12 10:00 Chicago (16:00 UTC).
        let now = at("2026-11-12T16:00:00Z");
        assert_eq!(
            Range::Today.period(now, &z),
            (at("2026-11-12T06:00:00Z"), at("2026-11-13T06:00:00Z"))
        );
        // Monday 11-09 to Monday 11-16.
        assert_eq!(
            Range::Week.period(now, &z),
            (at("2026-11-09T06:00:00Z"), at("2026-11-16T06:00:00Z"))
        );
        // November starts in daylight time (CDT, UTC−5): its first edge is 05:00 UTC.
        assert_eq!(
            Range::Month.period(now, &z),
            (at("2026-11-01T05:00:00Z"), at("2026-12-01T06:00:00Z"))
        );
        // March: starts in standard time, ends in daylight time.
        let march = at("2027-03-20T16:00:00Z");
        assert_eq!(
            Range::Month.period(march, &z),
            (at("2027-03-01T06:00:00Z"), at("2027-04-01T05:00:00Z"))
        );
    }

    /// Past rows, the release inside its window (marked in words), the next one; a release
    /// the rule doesn't count has no window; times in Chicago.
    #[test]
    fn rows_mark_past_inside_and_next() {
        let s = sched();
        let z = chicago();
        let rule = NewsBlackout {
            before_min: 5,
            ..NewsBlackout::new(NewsAction::Block)
        };
        // 07:29 Chicago on CPI day: inside CPI's window (07:25–07:32).
        let now = at("2026-11-12T13:29:00Z");
        let r = rows(&s, &s, Some(&rule), now, Range::Month, &z);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].state, RowState::Inside);
        assert_eq!(r[0].cells[CalCol::Window.index()], "07:25–07:32 · now");
        assert_eq!(r[0].cells[CalCol::Time.index()], "Thu Nov 12 07:30");
        assert_eq!(r[0].cells[CalCol::Release.index()], "CPI · October 2026");
        assert_eq!(r[0].cells[CalCol::Impact.index()], "High");
        assert_eq!(r[0].cells[CalCol::Source.index()], "bundled · verified");
        // PPI is medium: a high-only rule gives it no window; it is the next release after
        // the one in progress.
        assert_eq!(r[1].cells[CalCol::Window.index()], "—");
        assert_eq!(r[1].state, RowState::Later);
        assert_eq!(r[1].cells[CalCol::Source.index()], "bundled · unverified");
        // After CPI: it is past, PPI is next.
        let later = at("2026-11-12T15:00:00Z");
        let r = rows(&s, &s, Some(&rule), later, Range::Month, &z);
        assert_eq!(
            r.iter().map(|r| r.state).collect::<Vec<_>>(),
            [RowState::Past, RowState::Next, RowState::Later]
        );
        // Just after CPI, still inside its after-window: CPI is inside (warn outline) and PPI
        // is the next release (accent outline): both are marked.
        let just_after = at("2026-11-12T13:31:00Z");
        let r = rows(&s, &s, Some(&rule), just_after, Range::Month, &z);
        assert_eq!(
            r.iter().map(|r| r.state).collect::<Vec<_>>(),
            [RowState::Inside, RowState::Next, RowState::Later]
        );
        // Today only: CPI alone.
        assert_eq!(rows(&s, &s, None, later, Range::Today, &z).len(), 1);
    }

    /// A line the calendar it was built on doesn't have came from a refresh: FRED, at the
    /// release's usual time.
    #[test]
    fn a_refreshed_line_says_its_time_is_assumed() {
        let base = sched();
        let src = |_: &str| None;
        let s = base
            .refreshed(
                &[(
                    "gdp".to_owned(),
                    vec![lines_core::CivilDate::new(2026, 12, 23).unwrap()],
                )],
                &src,
                lines_core::CivilDate::new(2026, 11, 12).unwrap(),
                false,
            )
            .unwrap();
        let r = rows(
            &s,
            &base,
            None,
            at("2026-12-22T15:00:00Z"),
            Range::Week,
            &chicago(),
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].cells[CalCol::Source.index()], "FRED · usual time");
    }

    #[test]
    fn the_rule_reads_in_words() {
        assert_eq!(rule_text(None, None), "No account");
        assert_eq!(rule_text(None, Some("Paper")), "No blackout (Paper)");
        let r = NewsBlackout {
            before_min: 5,
            after_min: 5,
            ..NewsBlackout::new(NewsAction::Block)
        };
        assert_eq!(
            rule_text(Some(&r), Some("Paper 50K eval")),
            "Blackout: block entries ±5 min (Paper 50K eval)"
        );
        let f = NewsBlackout {
            include_medium: true,
            ..NewsBlackout::new(NewsAction::Flatten)
        };
        assert_eq!(
            rule_text(Some(&f), Some("Sim")),
            "Blackout: block entries and flatten ±2 min, medium releases too (Sim)"
        );
    }

    #[test]
    fn freshness_reads_in_words() {
        let s = sched();
        let z = chicago();
        let now = at("2026-11-12T20:40:00Z");
        let f = |r: Refresh| freshness(&r, &s, false, now, &z);
        assert_eq!(
            f(Refresh {
                updated: Some(at("2026-11-12T20:33:00Z")),
                ..Default::default()
            }),
            ("FRED · updated 14:33".to_owned(), false)
        );
        assert_eq!(
            f(Refresh {
                updated: Some(at("2026-11-11T20:33:00Z")),
                ..Default::default()
            })
            .0,
            "FRED · updated Wed Nov 11 14:33"
        );
        assert_eq!(
            f(Refresh {
                problem: Some("FRED answered 429".into()),
                ..Default::default()
            }),
            ("FRED · refresh failed: FRED answered 429".to_owned(), true)
        );
        assert_eq!(
            f(Refresh {
                no_key: true,
                ..Default::default()
            })
            .0,
            "Bundled calendar of 2026-11-07 · no FRED key"
        );
        assert_eq!(
            f(Refresh {
                not_this_run: true,
                ..Default::default()
            })
            .0,
            "Bundled calendar of 2026-11-07"
        );
        // A cached refresh from an earlier run, no key now.
        assert_eq!(
            freshness(
                &Refresh {
                    no_key: true,
                    ..Default::default()
                },
                &s,
                true,
                now,
                &z
            )
            .0,
            "FRED calendar of 2026-11-07 · no FRED key"
        );
        let stale = freshness(
            &Refresh {
                not_this_run: true,
                ..Default::default()
            },
            &s,
            false,
            at("2027-03-01T15:00:00Z"),
            &z,
        );
        assert_eq!(
            stale,
            ("Bundled calendar of 2026-11-07 · stale".to_owned(), true)
        );
    }
}
