//! Trading-session calendars (decision 0017, prerequisite of roadmap 5c).
//!
//! A [`SessionCalendar`] answers, exactly and deterministically, for one product:
//! - which session (ETH or RTH) contains an instant, with its trade date and UTC bounds
//!   ([`SessionCalendar::session_at`], [`SessionCalendar::next_session`]);
//! - the sessions overlapping a UTC range ([`SessionCalendar::sessions`]), for VWAP resets,
//!   volume profiles and daily bars;
//! - whether the market is open, and the next open/close ([`SessionCalendar::is_open`],
//!   [`SessionCalendar::next_open`], [`SessionCalendar::next_close`]);
//! - holidays and early closes, from a versioned data table ([`table`]).
//!
//! # Model
//!
//! Hours are exchange-local, in a real IANA time zone with DST (`jiff`, tz database compiled
//! in so every machine agrees). Instants stay UTC everywhere else (decision 0003).
//!
//! **A session belongs to its trade date.** For an exchange product, the ETH session of trade
//! date `D` opens at `eth_open` on the calendar day before `D` and closes at `eth_close` on `D`:
//! CME's Sunday 17:00 CT open is the start of *Monday's* session. Trade dates are Monday to
//! Friday minus holidays. Holiday rules ([`DayRule`]):
//! - `Closed` day: no trade date; the next session still opens that evening as usual.
//! - `Halt` day: trades from the usual evening open until the halt time, then stops. It is
//!   not a trade date: that morning belongs to the **next** trade date's session, which
//!   therefore has an internal break ([`Session::halt`]). At most one halt day may fall
//!   between two trade dates (validated).
//! - `EarlyClose`: the session (and RTH, if later) ends at the given time.
//!
//! RTH is the day session of trade date `D`, on `D` itself, clipped by an early close (none if
//! the early close is at or before the RTH open).
//!
//! **Same-day hours** (equities, decision 0028: NYSE 04:00–20:00 ET, RTH 09:30–16:00): when
//! `eth_open` is before `eth_close`, the session of `D` opens and closes on `D` itself. Halt
//! days don't exist for these (refused); an early close ends the whole session.
//!
//! Continuous products (crypto spot) trade 24/7; the "session" is a day starting at a
//! configurable local time (UTC for Coinbase). Its trade date is the date of its last instant,
//! the same convention as exchange sessions (a 00:00 UTC day is that UTC date).
//!
//! Outside a holiday table's coverage, weekdays are assumed regular and sessions say so
//! ([`HolidayData::NotCovered`]): never a silent guess.
//!
//! # Cost
//!
//! Queries are pure and allocation-free: a time-zone offset lookup, a few date steps and
//! binary searches in the holiday list (`tests/no_alloc.rs`). Construction loads the zone
//! once.

pub mod table;

use std::fmt;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::calendar::CivilDate;
use crate::time::{UtcMillis, days_from_civil};
pub use table::{DayRule, Holiday, HolidayCalendar, HolidayTable, TableError};

/// A local wall-clock time, minutes after midnight (00:00 ..= 23:59).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime(u16);

impl LocalTime {
    pub const MIDNIGHT: LocalTime = LocalTime(0);

    /// `h` < 24 and `m` < 60 (checked at compile time in `const` contexts).
    pub const fn hm(h: u8, m: u8) -> LocalTime {
        assert!(h < 24 && m < 60, "invalid local time");
        LocalTime(h as u16 * 60 + m as u16)
    }

    /// Parse `HH:MM` (24-hour, two digits each).
    pub fn parse(s: &str) -> Option<LocalTime> {
        let (h, m) = s.split_once(':')?;
        if h.len() != 2 || m.len() != 2 {
            return None;
        }
        let (h, m): (u8, u8) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then(|| LocalTime::hm(h, m))
    }

    pub fn minutes(self) -> u16 {
        self.0
    }
    pub fn hour(self) -> u8 {
        (self.0 / 60) as u8
    }
    pub fn minute(self) -> u8 {
        (self.0 % 60) as u8
    }
}

impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour(), self.minute())
    }
}

/// Full (electronic) session or the regular day session.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SessionKind {
    Eth,
    Rth,
}

/// Exchange hours for products with trade dates Monday–Friday.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExchangeHours {
    /// ETH open: on the calendar day *before* the trade date when it is after `eth_close`
    /// (overnight futures hours), else on the trade date itself (same-day hours).
    pub eth_open: LocalTime,
    /// ETH close, on the trade date.
    pub eth_close: LocalTime,
    /// RTH `(open, close)` on the trade date, within the ETH session.
    pub rth: Option<(LocalTime, LocalTime)>,
}

impl ExchangeHours {
    /// The session opens and closes on its trade date (no overnight part).
    pub fn same_day(&self) -> bool {
        self.eth_open < self.eth_close
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Hours {
    Exchange(ExchangeHours),
    /// Around the clock, every day. Sessions are days starting at `day_start` local time.
    Continuous {
        day_start: LocalTime,
    },
}

/// Static description of a product's hours: catalog data.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionSpec {
    /// IANA time zone the hours are in (e.g. `America/Chicago`).
    pub time_zone: &'static str,
    pub hours: Hours,
    /// Name of the holiday calendar in the [`HolidayTable`] (exchange products only).
    pub holidays: Option<&'static str>,
}

impl SessionSpec {
    /// 24/7 with a session day starting at `day_start` UTC.
    pub const fn continuous_utc(day_start: LocalTime) -> SessionSpec {
        SessionSpec {
            time_zone: "UTC",
            hours: Hours::Continuous { day_start },
            holidays: None,
        }
    }
}

/// How trustworthy the holiday data behind a session is.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum HolidayData {
    /// Inside the table's coverage, every rule involved is verified (or none applies).
    Verified,
    /// Inside coverage, but shaped by at least one unverified rule.
    Unverified,
    /// Outside the table's coverage (or no table): holidays unknown, weekdays assumed regular.
    NotCovered,
    /// The product has no holidays (24/7).
    NotApplicable,
}

/// One session. Bounds are UTC, `[open, close)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Session {
    pub trade_date: CivilDate,
    pub kind: SessionKind,
    pub open: UtcMillis,
    pub close: UtcMillis,
    /// A holiday halt inside the session, `[start, end)`: closed, but the trade date (and
    /// e.g. the VWAP) carries on after it.
    pub halt: Option<(UtcMillis, UtcMillis)>,
    /// Closes earlier than the regular time.
    pub early_close: bool,
    pub holidays: HolidayData,
}

impl Session {
    /// `t` is within the session's span (including any holiday halt).
    pub fn contains(&self, t: UtcMillis) -> bool {
        self.open <= t && t < self.close
    }

    /// The market is open at `t` within this session.
    pub fn is_open(&self, t: UtcMillis) -> bool {
        self.contains(t) && !self.halt.is_some_and(|(a, b)| a <= t && t < b)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    UnknownTimeZone(String),
    UnknownHolidayCalendar(String),
    /// Hours that don't describe a valid session layout.
    BadHours(String),
    /// A holiday rule that contradicts the hours.
    BadHoliday {
        date: CivilDate,
        message: String,
    },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::UnknownTimeZone(z) => write!(f, "unknown time zone {z:?}"),
            SessionError::UnknownHolidayCalendar(c) => write!(f, "unknown holiday calendar {c:?}"),
            SessionError::BadHours(m) => write!(f, "bad session hours: {m}"),
            SessionError::BadHoliday { date, message } => write!(f, "holiday {date}: {message}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Instants are clamped to this range of years (far beyond any market data).
const MIN_YEAR: i16 = 1900;
const MAX_YEAR: i16 = 2999;

/// A product's session calendar: hours, zone and holidays, ready for allocation-free queries.
#[derive(Clone, Debug)]
pub struct SessionCalendar {
    spec: SessionSpec,
    tz: TimeZone,
    holidays: Option<HolidayCalendar>,
    min_ms: i64,
    max_ms: i64,
}

impl SessionCalendar {
    /// Build and validate. `table` is required when the spec names a holiday calendar.
    pub fn new(
        spec: SessionSpec,
        table: Option<&HolidayTable>,
    ) -> Result<SessionCalendar, SessionError> {
        let tz = jiff::tz::db()
            .get(spec.time_zone)
            .map_err(|_| SessionError::UnknownTimeZone(spec.time_zone.to_owned()))?;
        let holidays = match spec.holidays {
            None => None,
            Some(name) => Some(
                table
                    .and_then(|t| t.calendar(name))
                    .cloned()
                    .ok_or_else(|| SessionError::UnknownHolidayCalendar(name.to_owned()))?,
            ),
        };
        let day_ms = |y: i16, m: u32, d: u32| days_from_civil(y as i64, m, d) * 86_400_000;
        let cal = SessionCalendar {
            spec,
            tz,
            holidays,
            min_ms: day_ms(MIN_YEAR, 1, 1),
            max_ms: day_ms(MAX_YEAR, 12, 31),
        };
        cal.validate()?;
        Ok(cal)
    }

    fn validate(&self) -> Result<(), SessionError> {
        let bad = |m: String| Err(SessionError::BadHours(m));
        let h = match self.spec.hours {
            Hours::Continuous { .. } => {
                if self.holidays.is_some() {
                    return bad("24/7 hours take no holiday calendar".into());
                }
                return Ok(());
            }
            Hours::Exchange(h) => h,
        };
        if h.eth_close == h.eth_open {
            return bad(format!(
                "ETH close {} must differ from the open {}",
                h.eth_close, h.eth_open
            ));
        }
        let floor = if h.same_day() {
            h.eth_open
        } else {
            LocalTime::MIDNIGHT
        };
        if let Some((o, c)) = h.rth
            && !(floor <= o && o < c && c <= h.eth_close)
        {
            return bad(format!("RTH {o}-{c} must be within the trade date's ETH"));
        }
        let Some(cal) = &self.holidays else {
            return Ok(());
        };
        for day in &cal.days {
            let fail = |m: String| {
                Err(SessionError::BadHoliday {
                    date: day.date,
                    message: m,
                })
            };
            match day.rule {
                DayRule::Closed => {}
                DayRule::Halt(_) if h.same_day() => {
                    return fail("same-day hours have no halt days (use closed or early)".into());
                }
                DayRule::Halt(t) => {
                    if t == LocalTime::MIDNIGHT || t >= h.eth_open {
                        return fail(format!(
                            "halt {t} must be after midnight and before {}",
                            h.eth_open
                        ));
                    }
                    // At most one halt between two trade dates (one internal break per session).
                    let mut d = day.date.add_days(1);
                    while !self.is_trade_date(d) {
                        if matches!(self.rule(d), Some(DayRule::Halt(_))) {
                            return fail(format!("second halt day {d} before the next trade date"));
                        }
                        d = d.add_days(1);
                    }
                }
                DayRule::EarlyClose(t) => {
                    // An early close at or before the RTH open leaves the day without RTH.
                    if t <= floor || t >= h.eth_close {
                        return fail(format!(
                            "early close {t} must be after {floor} and before {}",
                            h.eth_close
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn spec(&self) -> &SessionSpec {
        &self.spec
    }

    pub fn time_zone(&self) -> &TimeZone {
        &self.tz
    }

    pub fn holiday_calendar(&self) -> Option<&HolidayCalendar> {
        self.holidays.as_ref()
    }

    pub fn has_rth(&self) -> bool {
        matches!(
            self.spec.hours,
            Hours::Exchange(ExchangeHours { rth: Some(_), .. })
        )
    }

    /// The special rule for a date, if any.
    pub fn holiday(&self, date: CivilDate) -> Option<&Holiday> {
        self.holidays.as_ref()?.get(date)
    }

    fn rule(&self, date: CivilDate) -> Option<DayRule> {
        self.holiday(date).map(|h| h.rule)
    }

    /// `date` has a session.
    pub fn is_trade_date(&self, date: CivilDate) -> bool {
        match self.spec.hours {
            Hours::Continuous { .. } => true,
            Hours::Exchange(_) => {
                date.is_business_day()
                    && !matches!(self.rule(date), Some(DayRule::Closed | DayRule::Halt(_)))
            }
        }
    }

    fn data_for(&self, date: CivilDate) -> HolidayData {
        match &self.holidays {
            _ if matches!(self.spec.hours, Hours::Continuous { .. }) => HolidayData::NotApplicable,
            Some(c) if c.covers(date) && !c.coverage_verified => HolidayData::Unverified,
            Some(c) if c.covers(date) => match c.get(date) {
                Some(h) if !h.verified => HolidayData::Unverified,
                _ => HolidayData::Verified,
            },
            _ => HolidayData::NotCovered,
        }
    }

    fn clamp(&self, t: UtcMillis) -> Timestamp {
        Timestamp::from_millisecond(t.0.clamp(self.min_ms, self.max_ms)).expect("clamped")
    }

    /// Local date and time of an instant in the exchange's zone (allocation-free).
    pub fn local(&self, t: UtcMillis) -> (CivilDate, LocalTime) {
        let dt = self.tz.to_datetime(self.clamp(t));
        (
            CivilDate {
                year: dt.year() as u16,
                month: dt.month() as u8,
                day: dt.day() as u8,
            },
            LocalTime(dt.hour() as u16 * 60 + dt.minute() as u16),
        )
    }

    /// The instant of a local date and time. A wall time skipped by DST resolves forward and a
    /// repeated one to its first occurrence (RFC 5545 "compatible"); exchange hours never sit
    /// in a DST gap.
    pub fn instant(&self, date: CivilDate, time: LocalTime) -> UtcMillis {
        let year = (date.year as i16).clamp(MIN_YEAR, MAX_YEAR);
        let dt = jiff::civil::date(year, date.month as i8, date.day as i8).at(
            time.hour() as i8,
            time.minute() as i8,
            0,
            0,
        );
        let ts = self
            .tz
            .to_ambiguous_timestamp(dt)
            .compatible()
            .expect("dates are clamped to the supported range");
        UtcMillis(ts.as_millisecond())
    }

    /// The `kind` session of `trade_date`, or `None` if that date has none.
    pub fn session(&self, trade_date: CivilDate, kind: SessionKind) -> Option<Session> {
        match self.spec.hours {
            Hours::Continuous { day_start } => {
                if kind == SessionKind::Rth {
                    return None;
                }
                let start_day = if day_start == LocalTime::MIDNIGHT {
                    trade_date
                } else {
                    trade_date.add_days(-1)
                };
                Some(Session {
                    trade_date,
                    kind,
                    open: self.instant(start_day, day_start),
                    close: self.instant(start_day.add_days(1), day_start),
                    halt: None,
                    early_close: false,
                    holidays: HolidayData::NotApplicable,
                })
            }
            Hours::Exchange(h) => {
                if !self.is_trade_date(trade_date) {
                    return None;
                }
                let early = match self.rule(trade_date) {
                    Some(DayRule::EarlyClose(t)) => Some(t),
                    _ => None,
                };
                let mut holidays = self.data_for(trade_date);
                if kind == SessionKind::Rth {
                    let (o, c) = h.rth?;
                    let c = early.map_or(c, |e| e.min(c));
                    if c <= o {
                        return None; // closed before RTH would open
                    }
                    return Some(Session {
                        trade_date,
                        kind,
                        open: self.instant(trade_date, o),
                        close: self.instant(trade_date, c),
                        halt: None,
                        early_close: c < h.rth?.1,
                        holidays,
                    });
                }
                let close = self.instant(trade_date, early.unwrap_or(h.eth_close));
                if h.same_day() {
                    return Some(Session {
                        trade_date,
                        kind,
                        open: self.instant(trade_date, h.eth_open),
                        close,
                        halt: None,
                        early_close: early.is_some(),
                        holidays,
                    });
                }
                let evening = trade_date.add_days(-1);
                let mut open = self.instant(evening, h.eth_open);
                let mut halt = None;
                // Non-trade days since the previous trade date: a halt day's morning belongs
                // to this session; any unverified closure shapes it too.
                let mut d = evening;
                while !self.is_trade_date(d) {
                    let data = self.data_for(d);
                    if data == HolidayData::Unverified
                        || (data == HolidayData::NotCovered && holidays == HolidayData::Verified)
                    {
                        holidays = data;
                    }
                    if let Some(DayRule::Halt(t)) = self.rule(d) {
                        open = self.instant(d.add_days(-1), h.eth_open);
                        halt = Some((self.instant(d, t), self.instant(evening, h.eth_open)));
                    }
                    d = d.add_days(-1);
                }
                Some(Session {
                    trade_date,
                    kind,
                    open,
                    close,
                    halt,
                    early_close: early.is_some(),
                    holidays,
                })
            }
        }
    }

    fn next_trade_date(&self, mut d: CivilDate) -> CivilDate {
        // Terminates: outside the table's coverage every weekday is a trade date.
        while !self.is_trade_date(d) {
            d = d.add_days(1);
        }
        d
    }

    /// The first `kind` session that has not closed by `t`: the one containing `t`, or else the
    /// next one. `None` only for RTH on a product without RTH.
    pub fn next_session(&self, t: UtcMillis, kind: SessionKind) -> Option<Session> {
        let (date, time) = self.local(t);
        let mut candidate = match self.spec.hours {
            Hours::Continuous { day_start } => {
                // The session that started at the latest `day_start` at or before `t`.
                let start = if time >= day_start {
                    date
                } else {
                    date.add_days(-1)
                };
                if day_start == LocalTime::MIDNIGHT {
                    start
                } else {
                    start.add_days(1)
                }
            }
            Hours::Exchange(h) => match kind {
                SessionKind::Eth if !h.same_day() && time >= h.eth_open => date.add_days(1),
                _ => date,
            },
        };
        loop {
            let d = self.next_trade_date(candidate);
            candidate = d.add_days(1);
            match self.session(d, kind) {
                Some(s) if t < s.close => return Some(s),
                Some(_) => {}
                // A trade date that closes before RTH would open has no RTH session.
                None if kind == SessionKind::Rth && self.has_rth() => {}
                None => return None,
            }
        }
    }

    /// The `kind` session whose span contains `t` (a holiday halt inside it included), or
    /// `None` between sessions.
    pub fn session_at(&self, t: UtcMillis, kind: SessionKind) -> Option<Session> {
        self.next_session(t, kind).filter(|s| s.contains(t))
    }

    /// The market (ETH) is open at `t`.
    pub fn is_open(&self, t: UtcMillis) -> bool {
        self.next_session(t, SessionKind::Eth)
            .is_some_and(|s| s.is_open(t))
    }

    /// The first instant after `t` at which the market opens (`None`: it never closes).
    pub fn next_open(&self, t: UtcMillis) -> Option<UtcMillis> {
        if matches!(self.spec.hours, Hours::Continuous { .. }) {
            return None;
        }
        let s = self.next_session(t, SessionKind::Eth)?;
        if t < s.open {
            return Some(s.open);
        }
        if let Some((_, end)) = s.halt
            && t < end
        {
            return Some(end);
        }
        self.next_session(s.close, SessionKind::Eth).map(|n| n.open)
    }

    /// The first instant after `t` at which the market closes or halts (`None`: never).
    pub fn next_close(&self, t: UtcMillis) -> Option<UtcMillis> {
        if matches!(self.spec.hours, Hours::Continuous { .. }) {
            return None;
        }
        let s = self.next_session(t, SessionKind::Eth)?;
        match s.halt {
            Some((start, _)) if t < start => Some(start),
            _ => Some(s.close),
        }
    }

    /// Sessions overlapping `[from, to)`, in order. Allocation-free iterator.
    pub fn sessions(&self, from: UtcMillis, to: UtcMillis, kind: SessionKind) -> Sessions<'_> {
        Sessions {
            cal: self,
            next: if from < to {
                self.next_session(from, kind)
            } else {
                None
            },
            to,
        }
    }
}

/// Iterator over sessions in a range ([`SessionCalendar::sessions`]).
pub struct Sessions<'a> {
    cal: &'a SessionCalendar,
    next: Option<Session>,
    to: UtcMillis,
}

impl Iterator for Sessions<'_> {
    type Item = Session;

    fn next(&mut self) -> Option<Session> {
        let s = self.next.take().filter(|s| s.open < self.to)?;
        self.next = self.cal.next_session(s.close, s.kind);
        Some(s)
    }
}

#[cfg(test)]
mod tests;
