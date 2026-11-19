//! The economic calendar (roadmap 8b): scheduled releases from official sources (BLS, BEA, the
//! Federal Reserve) as versioned data, and the questions the news blackout asks of it.
//!
//! Lines ships one schedule (`lines-feed/data/econ-calendar.txt`); this module parses and
//! validates it. Parsing is pure (`&str` in, no I/O); queries never allocate.
//!
//! # Format (version 1)
//!
//! Line based. Blank lines and lines starting with `#` are ignored.
//!
//! ```text
//! format 1                        # required, first directive
//! version 2026-11-07              # required: when the data was checked against the sources
//! zone America/New_York           # required: IANA zone of every event time
//!
//! release cpi                     # starts a release; key [a-z0-9-]+, unique
//! name CPI                        # required: short name
//! agency BLS                      # required
//! importance high                 # required: high | medium | low
//! affects ES MES NQ MNQ ZN        # required: catalog product symbols it moves
//! coverage 2026-11-01 2027-12-31 unverified   # required: dates the list is complete for
//! source https://...              # at least one
//! 2026-11-10 08:30 unverified October 2026    # an event: local date, time, status, label
//! ```
//!
//! Event times are local to `zone` and must exist exactly once there: a time in a DST gap or
//! repeated by a DST fold is refused (the instant would be a guess). Events are strictly
//! increasing within a release and inside its coverage.
//!
//! # Missing and stale
//!
//! A release's list is complete only inside its coverage. Outside it the calendar is
//! [`Coverage::Missing`] for products that release affects: Lines never assumes a date without
//! data is clear. The whole schedule is [`Coverage::Stale`] once its `version` is more than
//! [`STALE_AFTER_DAYS`] old at the time asked about (agencies reschedule; a shutdown moved
//! releases in 2025), until a newer schedule replaces it.

use std::fmt;

use jiff::tz::TimeZone;

use crate::calendar::CivilDate;
use crate::sessions::LocalTime;
use crate::sessions::table::parse_date;
use crate::time::UtcMillis;

/// A schedule older than this many days (its `version` date to the date asked about) is stale.
pub const STALE_AFTER_DAYS: i64 = 60;

const MINUTE: i64 = 60_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Importance {
    Low,
    Medium,
    High,
}

impl Importance {
    pub fn parse(s: &str) -> Option<Importance> {
        match s {
            "low" => Some(Importance::Low),
            "medium" => Some(Importance::Medium),
            "high" => Some(Importance::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Importance::Low => "low",
            Importance::Medium => "medium",
            Importance::High => "high",
        }
    }
}

/// One kind of release (CPI, the jobs report, an FOMC statement).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub key: String,
    /// Short name for the UI ("CPI").
    pub name: String,
    pub agency: String,
    pub importance: Importance,
    /// Catalog product symbols the release moves.
    pub affects: Vec<String>,
    /// First and last local date (inclusive) the event list is complete for.
    pub coverage: (CivilDate, CivilDate),
    pub coverage_verified: bool,
    pub sources: Vec<String>,
}

impl Release {
    pub fn affects(&self, symbol: &str) -> bool {
        self.affects.iter().any(|s| s.eq_ignore_ascii_case(symbol))
    }

    pub fn covers(&self, date: CivilDate) -> bool {
        (self.coverage.0..=self.coverage.1).contains(&date)
    }
}

/// One scheduled release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EconEvent {
    pub at: UtcMillis,
    /// Index into [`EconSchedule::releases`].
    pub release: usize,
    /// What it reports on ("October 2026").
    pub label: String,
    /// Checked against the agency's own publication.
    pub verified: bool,
}

/// Whether the schedule can answer for a product at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// Complete and recent.
    Current,
    /// Older than [`STALE_AFTER_DAYS`]: events may have moved since it was checked.
    Stale { checked: CivilDate },
    /// A release that affects the product has no list for this date.
    Missing { release: usize, date: CivilDate },
}

#[derive(Clone, Debug)]
pub struct EconSchedule {
    /// When the data was checked against its sources.
    pub version: CivilDate,
    /// IANA name of the zone event times were written in.
    pub zone: String,
    tz: TimeZone,
    pub releases: Vec<Release>,
    /// Every release's events, sorted by time (ties by release order).
    pub events: Vec<EconEvent>,
}

impl PartialEq for EconSchedule {
    fn eq(&self, o: &Self) -> bool {
        self.version == o.version
            && self.zone == o.zone
            && self.releases == o.releases
            && self.events == o.events
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleError {
    /// 1-based line number (0 for whole-file problems).
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "economic calendar: {}", self.message)
        } else {
            write!(f, "economic calendar line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ScheduleError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ScheduleError> {
    Err(ScheduleError {
        line,
        message: message.into(),
    })
}

fn status(s: Option<&str>) -> Option<bool> {
    match s? {
        "verified" => Some(true),
        "unverified" => Some(false),
        _ => None,
    }
}

/// A release being parsed: fields stay optional until it is finished.
struct Draft {
    line: usize,
    key: String,
    name: Option<String>,
    agency: Option<String>,
    importance: Option<Importance>,
    affects: Vec<String>,
    coverage: Option<(CivilDate, CivilDate, bool)>,
    sources: Vec<String>,
    /// (line, local date, event)
    events: Vec<(usize, CivilDate, EconEvent)>,
}

impl Draft {
    fn finish(self, index: usize) -> Result<(Release, Vec<EconEvent>), ScheduleError> {
        let n = self.line;
        let missing = |what: &str| err(n, format!("release {} has no `{what}`", self.key));
        let Some(name) = self.name.clone() else {
            return missing("name");
        };
        let Some(agency) = self.agency.clone() else {
            return missing("agency");
        };
        let Some(importance) = self.importance else {
            return missing("importance");
        };
        let Some((from, through, cv)) = self.coverage else {
            return missing("coverage");
        };
        if self.affects.is_empty() {
            return missing("affects");
        }
        if self.sources.is_empty() {
            return missing("source");
        }
        let mut prev: Option<UtcMillis> = None;
        let mut events = Vec::with_capacity(self.events.len());
        for (line, date, mut ev) in self.events {
            if !(from..=through).contains(&date) {
                return err(line, format!("{date} is outside the release's coverage"));
            }
            if prev.is_some_and(|p| ev.at <= p) {
                return err(line, "events must be in strictly increasing time order");
            }
            prev = Some(ev.at);
            ev.release = index;
            events.push(ev);
        }
        Ok((
            Release {
                key: self.key,
                name,
                agency,
                importance,
                affects: self.affects,
                coverage: (from, through),
                coverage_verified: cv,
                sources: self.sources,
            },
            events,
        ))
    }
}

/// The one instant of a local date and time in `tz`, or `None` in a DST gap or fold.
fn exact_instant(tz: &TimeZone, date: CivilDate, time: LocalTime) -> Option<UtcMillis> {
    let year = i16::try_from(date.year).ok()?;
    let dt = jiff::civil::date(year, date.month as i8, date.day as i8).at(
        time.hour() as i8,
        time.minute() as i8,
        0,
        0,
    );
    let ts = tz.to_ambiguous_timestamp(dt).unambiguous().ok()?;
    Some(UtcMillis(ts.as_millisecond()))
}

impl EconSchedule {
    /// Parse and validate a schedule.
    pub fn parse(text: &str) -> Result<EconSchedule, ScheduleError> {
        let mut format_seen = false;
        let mut version = None;
        let mut zone: Option<(String, TimeZone)> = None;
        let mut drafts: Vec<Draft> = Vec::new();

        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (head, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            if !format_seen {
                if head != "format" {
                    return err(n, "the first directive must be `format 1`");
                }
                if rest != "1" {
                    return err(n, "unsupported format (expected `format 1`)");
                }
                format_seen = true;
                continue;
            }
            let needs_release = |d: &mut Vec<Draft>| -> Result<(), ScheduleError> {
                if d.is_empty() {
                    return err(n, format!("`{head}` outside a release"));
                }
                Ok(())
            };
            match head {
                "format" => return err(n, "duplicate `format`"),
                "version" => {
                    if version.is_some() {
                        return err(n, "duplicate `version`");
                    }
                    let Some(d) = parse_date(rest) else {
                        return err(n, "`version` needs a YYYY-MM-DD date");
                    };
                    version = Some(d);
                }
                "zone" => {
                    if zone.is_some() {
                        return err(n, "duplicate `zone`");
                    }
                    let Ok(tz) = jiff::tz::db().get(rest) else {
                        return err(n, format!("unknown time zone {rest:?}"));
                    };
                    zone = Some((rest.to_owned(), tz));
                }
                "release" => {
                    if zone.is_none() || version.is_none() {
                        return err(n, "`version` and `zone` come before the first release");
                    }
                    if rest.is_empty()
                        || !rest
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    {
                        return err(n, format!("bad release key {rest:?} (use [a-z0-9-])"));
                    }
                    if drafts.iter().any(|d| d.key == rest) {
                        return err(n, format!("duplicate release {rest}"));
                    }
                    drafts.push(Draft {
                        line: n,
                        key: rest.to_owned(),
                        name: None,
                        agency: None,
                        importance: None,
                        affects: Vec::new(),
                        coverage: None,
                        sources: Vec::new(),
                        events: Vec::new(),
                    });
                }
                "name" | "agency" | "importance" | "affects" | "coverage" | "source" => {
                    needs_release(&mut drafts)?;
                    let d = drafts.last_mut().expect("checked");
                    if rest.is_empty() {
                        return err(n, format!("`{head}` needs a value"));
                    }
                    match head {
                        "name" if d.name.is_none() => d.name = Some(rest.to_owned()),
                        "agency" if d.agency.is_none() => d.agency = Some(rest.to_owned()),
                        "importance" if d.importance.is_none() => {
                            let Some(imp) = Importance::parse(rest) else {
                                return err(n, "importance is high, medium or low");
                            };
                            d.importance = Some(imp);
                        }
                        "affects" if d.affects.is_empty() => {
                            d.affects = rest.split_whitespace().map(str::to_owned).collect();
                        }
                        "coverage" if d.coverage.is_none() => {
                            let mut w = rest.split_whitespace();
                            let from = w.next().and_then(parse_date);
                            let through = w.next().and_then(parse_date);
                            let st = status(w.next());
                            let (Some(from), Some(through), Some(st), None) =
                                (from, through, st, w.next())
                            else {
                                return err(
                                    n,
                                    "`coverage` is <from> <through> verified|unverified",
                                );
                            };
                            if through < from {
                                return err(n, "coverage ends before it starts");
                            }
                            d.coverage = Some((from, through, st));
                        }
                        "source" => d.sources.push(rest.to_owned()),
                        _ => return err(n, format!("duplicate `{head}`")),
                    }
                }
                _ => {
                    let Some(date) = parse_date(head) else {
                        return err(n, format!("unknown directive {head:?}"));
                    };
                    needs_release(&mut drafts)?;
                    let mut w = rest.splitn(3, char::is_whitespace);
                    let Some(time) = w.next().and_then(LocalTime::parse) else {
                        return err(n, "an event needs a HH:MM time after its date");
                    };
                    let Some(verified) = status(w.next()) else {
                        return err(n, "an event needs `verified` or `unverified`");
                    };
                    let label = w.next().unwrap_or("").trim();
                    if label.is_empty() {
                        return err(n, "an event needs a label");
                    }
                    let (_, tz) = zone.as_ref().expect("a release needs the zone first");
                    let Some(at) = exact_instant(tz, date, time) else {
                        return err(
                            n,
                            format!("{date} {time} doesn't exist exactly once in the zone (DST)"),
                        );
                    };
                    drafts.last_mut().expect("checked").events.push((
                        n,
                        date,
                        EconEvent {
                            at,
                            release: 0,
                            label: label.to_owned(),
                            verified,
                        },
                    ));
                }
            }
        }
        if !format_seen {
            return err(0, "empty (no `format 1`)");
        }
        let Some(version) = version else {
            return err(0, "no `version`");
        };
        let Some((zone, tz)) = zone else {
            return err(0, "no `zone`");
        };
        if drafts.is_empty() {
            return err(0, "no releases");
        }
        let mut releases = Vec::with_capacity(drafts.len());
        let mut events = Vec::new();
        for (i, d) in drafts.into_iter().enumerate() {
            let (r, ev) = d.finish(i)?;
            releases.push(r);
            events.extend(ev);
        }
        // Stable: equal times keep release order.
        events.sort_by_key(|e| e.at);
        Ok(EconSchedule {
            version,
            zone,
            tz,
            releases,
            events,
        })
    }

    /// The local (schedule zone) date at `t`.
    pub fn local_date(&self, t: UtcMillis) -> CivilDate {
        let (date, _) = self.local(t);
        date
    }

    /// The local (schedule zone) date and time at `t`.
    pub fn local(&self, t: UtcMillis) -> (CivilDate, LocalTime) {
        let ts = jiff::Timestamp::from_millisecond(t.0).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        let dt = self.tz.to_datetime(ts);
        (
            CivilDate {
                year: dt.year() as u16,
                month: dt.month() as u8,
                day: dt.day() as u8,
            },
            LocalTime::hm(dt.hour() as u8, dt.minute() as u8),
        )
    }

    pub fn release(&self, e: &EconEvent) -> &Release {
        &self.releases[e.release]
    }

    /// Whether the schedule can answer for `symbol` about releases at least `min` important
    /// whose windows could hold `now`: the local dates from `now − back_ms` to `now + ahead_ms`
    /// must be covered by every such release that affects it (a release below `min` never
    /// matters). Missing wins over stale.
    pub fn coverage(
        &self,
        symbol: &str,
        now: UtcMillis,
        back_ms: i64,
        ahead_ms: i64,
        min: Importance,
    ) -> Coverage {
        let dates = [
            self.local_date(UtcMillis(now.0 - back_ms.max(0))),
            self.local_date(now),
            self.local_date(UtcMillis(now.0 + ahead_ms.max(0))),
        ];
        let counts = |r: &Release| r.importance >= min && r.affects(symbol);
        for (i, r) in self.releases.iter().enumerate() {
            if !counts(r) {
                continue;
            }
            if let Some(&date) = dates.iter().find(|d| !r.covers(**d)) {
                return Coverage::Missing { release: i, date };
            }
        }
        let affected = self.releases.iter().any(counts);
        if affected && dates[1].days() - self.version.days() > STALE_AFTER_DAYS {
            return Coverage::Stale {
                checked: self.version,
            };
        }
        Coverage::Current
    }

    /// The event whose window `[at − before, at + after)` holds `now`, if any (the earliest
    /// when windows overlap).
    pub fn window_at(
        &self,
        symbol: Option<&str>,
        now: UtcMillis,
        before_ms: i64,
        after_ms: i64,
        min: Importance,
    ) -> Option<&EconEvent> {
        // Events are sorted: skip those whose window ended.
        let first = self.events.partition_point(|e| e.at.0 + after_ms <= now.0);
        self.events[first..]
            .iter()
            .take_while(|e| e.at.0 - before_ms <= now.0)
            .find(|e| {
                let r = self.release(e);
                r.importance >= min && symbol.is_none_or(|s| r.affects(s))
            })
    }

    /// The first event at or after `from` (for countdowns and lists).
    pub fn next_event(
        &self,
        symbol: Option<&str>,
        from: UtcMillis,
        min: Importance,
    ) -> Option<&EconEvent> {
        let first = self.events.partition_point(|e| e.at < from);
        self.events[first..].iter().find(|e| {
            let r = self.release(e);
            r.importance >= min && symbol.is_none_or(|s| r.affects(s))
        })
    }

    /// Events from `from` on, any importance (the palette list).
    pub fn upcoming(&self, from: UtcMillis) -> impl Iterator<Item = &EconEvent> {
        let first = self.events.partition_point(|e| e.at < from);
        self.events[first..].iter()
    }

    /// The next time strictly after `now` a window opens (`at − before`), for waking a clock.
    pub fn next_window_start(
        &self,
        symbol: Option<&str>,
        now: UtcMillis,
        before_ms: i64,
        min: Importance,
    ) -> Option<UtcMillis> {
        // Window starts are sorted like the events: skip those not after `now`.
        let first = self.events.partition_point(|e| e.at.0 - before_ms <= now.0);
        self.events[first..]
            .iter()
            .find(|e| {
                let r = self.release(e);
                r.importance >= min && symbol.is_none_or(|s| r.affects(s))
            })
            .map(|e| UtcMillis(e.at.0 - before_ms))
    }

    /// `HH:MM` of an event in the schedule's zone.
    pub fn local_time(&self, e: &EconEvent) -> LocalTime {
        self.local(e.at).1
    }

    /// Whole minutes left until `until`, rounded up (0 when past).
    pub fn minutes_left(now: UtcMillis, until: UtcMillis) -> i64 {
        ((until.0 - now.0).max(0) + MINUTE - 1) / MINUTE
    }

    /// The schedule in its file format (parses back to an equal schedule): how a refreshed
    /// schedule is cached.
    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "format 1
version {}
zone {}
",
            self.version, self.zone
        );
        for (i, r) in self.releases.iter().enumerate() {
            let _ = write!(
                out,
                "
release {}
name {}
agency {}
importance {}
affects {}
coverage {} {} {}
",
                r.key,
                r.name,
                r.agency,
                r.importance.as_str(),
                r.affects.join(" "),
                r.coverage.0,
                r.coverage.1,
                if r.coverage_verified {
                    "verified"
                } else {
                    "unverified"
                },
            );
            for src in &r.sources {
                let _ = writeln!(out, "source {src}");
            }
            for e in self.events.iter().filter(|e| e.release == i) {
                let (date, time) = self.local(e.at);
                let st = if e.verified { "verified" } else { "unverified" };
                let _ = writeln!(out, "{date} {time} {st} {}", e.label);
            }
        }
        out
    }

    /// This schedule with official release dates fetched from `source` (a refresh, roadmap
    /// 8b), as of the local date `today`:
    ///
    /// - For a release with dates from `today` on (up to the first gap longer than
    ///   [`MAX_GAP_DAYS`]), each date the schedule lacks is added with the release's usual
    ///   local time (the most common among its events), `unverified`, labelled by its month.
    ///   Every line the schedule had stays, listed or not (never fewer windows). Coverage
    ///   runs through the later of the old end and the last date kept, `unverified` when a
    ///   new date came in.
    /// - A release the source didn't answer for (absent from `fetched`, or only past dates)
    ///   stays as it was.
    /// - The version becomes `today` only when every release was answered (`complete`):
    ///   otherwise the unanswered ones would look freshly checked.
    pub fn refreshed(
        &self,
        fetched: &[(String, Vec<CivilDate>)],
        source: &dyn Fn(&str) -> Option<String>,
        today: CivilDate,
        complete: bool,
    ) -> Result<EconSchedule, ScheduleError> {
        let mut out = self.clone();
        let mut events: Vec<EconEvent> = Vec::with_capacity(self.events.len());
        for (i, r) in self.releases.iter().enumerate() {
            let old: Vec<(CivilDate, &EconEvent)> = self
                .events
                .iter()
                .filter(|e| e.release == i)
                .map(|e| (self.local_date(e.at), e))
                .collect();
            let mut dates: Vec<CivilDate> = fetched
                .iter()
                .find(|(k, _)| *k == r.key)
                .map(|(_, d)| d.iter().copied().filter(|d| *d >= today).collect())
                .unwrap_or_default();
            dates.sort();
            dates.dedup();
            // A list is complete only up to its first long gap: past one, later dates
            // (an announced annual update months out) say nothing about the months between.
            let mut prev = today;
            dates.retain(|d| {
                let keep = d.days() - prev.days() <= MAX_GAP_DAYS;
                if keep {
                    prev = *d;
                }
                keep
            });
            let (Some(&first), Some(&last)) = (dates.first(), dates.last()) else {
                events.extend(old.iter().map(|(_, e)| (*e).clone()));
                continue;
            };
            let time = usual_time(old.iter().map(|(_, e)| self.local_time(e)));
            let mut added = false;
            // Every line the schedule had stays: a date the source doesn't list costs at most
            // an extra window, while dropping one could leave a real release unguarded.
            events.extend(old.iter().map(|(_, e)| (*e).clone()));
            for d in dates {
                match old.iter().find(|(od, _)| *od == d) {
                    Some(_) => {}
                    None => {
                        let Some(at) = exact_instant(&self.tz, d, time) else {
                            return err(0, format!("{} on {d} {time}: not exactly once", r.key));
                        };
                        added = true;
                        events.push(EconEvent {
                            at,
                            release: i,
                            label: format!("{} {} release", MONTHS[d.month as usize - 1], d.year),
                            verified: false,
                        });
                    }
                }
            }
            let rel = &mut out.releases[i];
            rel.coverage.0 = rel.coverage.0.min(first);
            rel.coverage.1 = rel.coverage.1.max(last);
            if added {
                rel.coverage_verified = false;
            }
            if let Some(src) = source(&r.key)
                && !rel.sources.contains(&src)
            {
                rel.sources.push(src);
            }
        }
        events.sort_by_key(|e| e.at);
        out.events = events;
        if complete {
            out.version = out.version.max(today);
        }
        // Through the parser again: every rule a file must meet, a refreshed one meets.
        EconSchedule::parse(&out.to_text())
    }
}

/// The longest gap between consecutive fetched dates of a release that still counts as one
/// complete list (quarterly releases are about 90 days apart).
pub const MAX_GAP_DAYS: i64 = 100;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The most common time among `times` (ties: the earliest), 08:30 when there are none.
fn usual_time(times: impl Iterator<Item = LocalTime>) -> LocalTime {
    let mut seen: Vec<(LocalTime, usize)> = Vec::new();
    for t in times {
        match seen.iter_mut().find(|(s, _)| *s == t) {
            Some((_, n)) => *n += 1,
            None => seen.push((t, 1)),
        }
    }
    seen.sort_by_key(|&(t, n)| (std::cmp::Reverse(n), t));
    seen.first().map_or(LocalTime::hm(8, 30), |&(t, _)| t)
}

#[cfg(test)]
mod tests;
