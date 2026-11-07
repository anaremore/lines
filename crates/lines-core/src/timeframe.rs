//! Chart timeframes (roadmap 9): higher timeframes derived for display from the store's
//! 1-minute bars.
//!
//! The store keeps 1-minute bars (history, stitch and the disk store are untouched). A chart
//! on another [`Timeframe`] shows a [`Derived`] series: exact aggregation of the 1-minute bars
//! in each bucket (open of the first, high and low the max and min, close of the last, volume
//! summed exactly as `Qty`).
//!
//! # Buckets
//! - Minutes and hours align to the **session open** ([`SessionCalendar`], ETH), so a 1 h bar
//!   on ES starts at the 17:00 CT open, through DST changes, not at UTC hours. A bucket never
//!   spans a session break: the daily close, or a holiday halt inside a session (each side
//!   aligns from its own start). A bucket cut short by a break is *partial* (the crosshair
//!   readout says so).
//! - Day: one session, its trade date. Week: the trade dates Monday to Friday of one week.
//! - Products without a calendar align to UTC (minutes and hours from the epoch, days at
//!   00:00 UTC, weeks from Monday 00:00 UTC).
//!
//! # Cost
//! [`Derived::update`] uses the series' change log ([`BarSeries::changed_since`]): a tick
//! re-aggregates only the forming bucket (or from the earliest bucket a history fill
//! touched); a front trim or a lost log rebuilds. Buffers are reserved for the series' whole
//! capacity, so steady-state updates never allocate (`tests/no_alloc.rs`).
//!
//! # Tick bars
//! [`Timeframe::Ticks`] bars need trades, not 1-minute bars: their [`Timeframe::source`] is
//! [`BarSource::Trades`] and they are built by [`crate::ticks::TickBars`] from counted trades
//! only. The chart sees them through the same [`BarsView`] (with [`BarsView::trades`] set).
//! Seconds, volume and range bars are phase 2 and will be further `Trades` variants.

use std::fmt;

use crate::bars::{Bar, BarSeries, Changed};
use crate::calendar::CivilDate;
use crate::sessions::{Session, SessionCalendar, SessionKind};
use crate::time::UtcMillis;

const MINUTE: i64 = UtcMillis::MINUTE;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Timeframe {
    /// 1, 2, 3, 5, 10, 15 or 30 minutes.
    Minutes(u16),
    /// 1, 2 or 4 hours.
    Hours(u16),
    Day,
    Week,
    /// Tick bars: `n` counted trades each (1..=[`crate::ticks::MAX_TICKS`]).
    Ticks(u32),
}

/// What a timeframe is built from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BarSource {
    /// The store's 1-minute bars (every timeframe today).
    OneMinute,
    /// Counted trades (tick bars; phase 2: seconds, volume and range bars).
    Trades,
}

impl Default for Timeframe {
    fn default() -> Self {
        Timeframe::ONE_MINUTE
    }
}

impl Timeframe {
    pub const ONE_MINUTE: Timeframe = Timeframe::Minutes(1);

    /// Every timeframe offered in lists (header, palette): time-based shortest first, then the
    /// tick presets. Any `Ticks(1..=100_000)` is valid too (typed).
    pub const ALL: [Timeframe; 17] = [
        Timeframe::Minutes(1),
        Timeframe::Minutes(2),
        Timeframe::Minutes(3),
        Timeframe::Minutes(5),
        Timeframe::Minutes(10),
        Timeframe::Minutes(15),
        Timeframe::Minutes(30),
        Timeframe::Hours(1),
        Timeframe::Hours(2),
        Timeframe::Hours(4),
        Timeframe::Day,
        Timeframe::Week,
        Timeframe::Ticks(100),
        Timeframe::Ticks(233),
        Timeframe::Ticks(500),
        Timeframe::Ticks(1000),
        Timeframe::Ticks(2000),
    ];

    /// Whether this is one of the offered timeframes (a listed one, or any tick count in
    /// `1..=MAX_TICKS`).
    pub fn is_valid(self) -> bool {
        match self {
            Timeframe::Ticks(n) => (1..=crate::ticks::MAX_TICKS).contains(&n),
            _ => Self::ALL.contains(&self),
        }
    }

    pub fn source(self) -> BarSource {
        match self {
            Timeframe::Ticks(_) => BarSource::Trades,
            _ => BarSource::OneMinute,
        }
    }

    /// The tick count of a tick timeframe.
    pub fn ticks(self) -> Option<u32> {
        match self {
            Timeframe::Ticks(n) => Some(n),
            _ => None,
        }
    }

    pub fn is_one_minute(self) -> bool {
        self == Self::ONE_MINUTE
    }

    /// The bucket length for minutes and hours; a nominal length for days (24 h) and weeks
    /// (7 days), used for spacing and extrapolating past the data. Tick bars have no length:
    /// a minute here (their view uses the bars' own average, [`crate::ticks::TickBars::view`]).
    pub fn nominal_ms(self) -> i64 {
        match self {
            Timeframe::Ticks(_) => MINUTE,
            Timeframe::Minutes(n) => i64::from(n) * MINUTE,
            Timeframe::Hours(n) => i64::from(n) * HOUR,
            Timeframe::Day => DAY,
            Timeframe::Week => 7 * DAY,
        }
    }

    /// Bars labelled by date rather than time of day.
    pub fn is_daily(self) -> bool {
        matches!(self, Timeframe::Day | Timeframe::Week)
    }

    /// `5`, `5m`, `1h`, `4h`, `d`, `1d`, `w`, `1w`, `500t` (any case). A bare number is
    /// minutes; ticks need their count. Only offered timeframes parse.
    pub fn parse(s: &str) -> Option<Timeframe> {
        let s = s.trim().to_ascii_lowercase();
        let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
        let (num, unit) = s.split_at(digits);
        if digits > 6 {
            return None;
        }
        let n: u32 = if num.is_empty() { 1 } else { num.parse().ok()? };
        let short = u16::try_from(n).ok();
        let tf = match unit {
            "" | "m" | "min" if !num.is_empty() => Timeframe::Minutes(short?),
            "h" | "hr" => Timeframe::Hours(short?),
            "d" if n == 1 => Timeframe::Day,
            "w" if n == 1 => Timeframe::Week,
            "t" if !num.is_empty() => Timeframe::Ticks(n),
            _ => return None,
        };
        tf.is_valid().then_some(tf)
    }
}

impl fmt::Display for Timeframe {
    /// `1m`, `5m`, `1h`, `1D`, `1W`, `500t`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Timeframe::Ticks(n) => write!(f, "{n}t"),
            Timeframe::Minutes(n) => write!(f, "{n}m"),
            Timeframe::Hours(n) => write!(f, "{n}h"),
            Timeframe::Day => f.write_str("1D"),
            Timeframe::Week => f.write_str("1W"),
        }
    }
}

/// One bucket: `[start, end)`, and whether a session break cut it short.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub start: UtcMillis,
    pub end: UtcMillis,
    pub partial: bool,
}

/// Maps instants to buckets of one timeframe. Caches the current session, so walking bars in
/// order costs a calendar query only at each new session. Allocation-free.
pub struct Bucketer<'a> {
    tf: Timeframe,
    cal: Option<&'a SessionCalendar>,
    session: Option<Session>,
    /// The last week bucket and the trade date it was computed for.
    week: Option<(CivilDate, Bucket)>,
}

impl<'a> Bucketer<'a> {
    pub fn new(tf: Timeframe, cal: Option<&'a SessionCalendar>) -> Bucketer<'a> {
        Bucketer {
            tf,
            cal,
            session: None,
            week: None,
        }
    }

    fn session(&mut self, t: UtcMillis) -> Option<Session> {
        let cal = self.cal?;
        if let Some(s) = self.session
            && s.contains(t)
        {
            return Some(s);
        }
        self.session = cal.session_at(t, SessionKind::Eth);
        self.session
    }

    /// The bucket containing `t`.
    pub fn bucket(&mut self, t: UtcMillis) -> Bucket {
        let len = self.tf.nominal_ms();
        let session = self.session(t);
        match (self.tf, session) {
            (Timeframe::Minutes(_) | Timeframe::Hours(_), Some(s)) => {
                // The part of the session `t` is in: before or after a holiday halt.
                let (seg_start, seg_end) = match s.halt {
                    Some((_, b)) if t >= b => (b, s.close),
                    Some((a, _)) => (s.open, a),
                    None => (s.open, s.close),
                };
                let start = seg_start.0 + (t.0 - seg_start.0).div_euclid(len) * len;
                let end = (start + len).min(seg_end.0);
                Bucket {
                    start: UtcMillis(start),
                    end: UtcMillis(end),
                    partial: end - start < len,
                }
            }
            (Timeframe::Day, Some(s)) => Bucket {
                start: s.open,
                end: s.close,
                partial: false,
            },
            (Timeframe::Week, Some(s)) => match self.week {
                Some((d, b)) if d == s.trade_date => b,
                _ => {
                    let b = self.week(s);
                    self.week = Some((s.trade_date, b));
                    b
                }
            },
            // No calendar, or between sessions: UTC alignment.
            (Timeframe::Week, None) => {
                // Weeks from Monday 00:00 UTC (1970-01-05 was a Monday).
                let monday = 4 * DAY;
                let start = monday + (t.0 - monday).div_euclid(len) * len;
                Bucket {
                    start: UtcMillis(start),
                    end: UtcMillis(start + len),
                    partial: false,
                }
            }
            _ => {
                let start = t.0.div_euclid(len) * len;
                Bucket {
                    start: UtcMillis(start),
                    end: UtcMillis(start + len),
                    partial: false,
                }
            }
        }
    }

    /// The week of `s`'s trade date: from the first session of that week (Monday on) to the
    /// last (Friday back).
    fn week(&self, s: Session) -> Bucket {
        let Some(cal) = self.cal else {
            return Bucket {
                start: s.open,
                end: s.close,
                partial: false,
            };
        };
        let monday = s.trade_date.add_days(-i64::from(s.trade_date.weekday()));
        let first = (0..5)
            .filter_map(|i| cal.session(monday.add_days(i), SessionKind::Eth))
            .next()
            .unwrap_or(s);
        let last = (0..5)
            .rev()
            .filter_map(|i| cal.session(monday.add_days(i), SessionKind::Eth))
            .next()
            .unwrap_or(s);
        Bucket {
            start: first.open,
            end: last.close,
            partial: false,
        }
    }
}

/// The date a daily or weekly bar starting at `start` is labelled with: its trade date (a
/// week: the Monday of its trade dates). Without a calendar, the UTC date.
pub fn bar_date(tf: Timeframe, cal: Option<&SessionCalendar>, start: UtcMillis) -> CivilDate {
    let d = cal
        .and_then(|c| c.session_at(start, SessionKind::Eth))
        .map_or_else(|| CivilDate::from_utc(start), |s| s.trade_date);
    match tf {
        Timeframe::Week => d.add_days(-i64::from(d.weekday())),
        _ => d,
    }
}

/// Bars for display, with each bar's end (a timeframe's buckets vary in length).
#[derive(Copy, Clone, Debug)]
pub struct BarsView<'a> {
    pub bars: &'a [Bar],
    /// Bucket ends (`[time, end)`), one per bar; `None`: every bar is `period_ms` long.
    pub ends: Option<&'a [UtcMillis]>,
    pub period_ms: i64,
    /// Bars built from counted trades (tick bars): each bar's own trade data (last trade,
    /// count, per-price volume). `None` for time bars.
    pub trades: Option<&'a crate::ticks::TickBars>,
}

impl<'a> BarsView<'a> {
    /// 1-minute (or any fixed-period) bars.
    pub fn fixed(bars: &'a [Bar], period_ms: i64) -> BarsView<'a> {
        BarsView {
            bars,
            ends: None,
            period_ms,
            trades: None,
        }
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    /// Bar `i`'s end.
    pub fn end(&self, i: usize) -> UtcMillis {
        match self.ends {
            Some(e) => e[i],
            None => UtcMillis(self.bars[i].time.0 + self.period_ms),
        }
    }

    /// The slot `t` falls in, as [`crate::geometry::time_slot`] (crosshair sync): the bar
    /// whose bucket contains `t` (`None` in a gap between buckets), extrapolated by whole
    /// nominal periods before the first bar and after the last.
    pub fn slot(&self, t: UtcMillis) -> Option<i64> {
        if self.ends.is_none() {
            return crate::geometry::time_slot(self.bars, t, self.period_ms);
        }
        let (first, last) = (self.bars.first()?, self.bars.len() - 1);
        let p = self.period_ms.max(1);
        if t < first.time {
            return Some((t.0 - first.time.0).div_euclid(p));
        }
        let end = self.end(last);
        if t >= end {
            return Some(last as i64 + 1 + (t.0 - end.0) / p);
        }
        self.bar_at(t).map(|i| i as i64)
    }

    /// The bar whose bucket contains `t`, if any (`None` in a gap between buckets).
    pub fn bar_at(&self, t: UtcMillis) -> Option<usize> {
        let i = self.bars.partition_point(|b| b.time <= t).checked_sub(1)?;
        (t < self.end(i)).then_some(i)
    }
}

/// A timeframe's bars, derived from a 1-minute series and kept up to date incrementally.
#[derive(Clone, Debug)]
pub struct Derived {
    tf: Timeframe,
    bars: Vec<Bar>,
    ends: Vec<UtcMillis>,
    partial: Vec<bool>,
    /// The source revision and trim count this reflects (`None`: never built).
    seen: Option<(u64, u64)>,
    /// Bars dropped from the front so far (the source trimmed), as [`BarSeries::trimmed`].
    trimmed: u64,
    period_ms: i64,
    /// The bucketer's session caches, kept across updates (a tick costs no calendar query).
    cache: (Option<Session>, Option<(CivilDate, Bucket)>),
    /// The newest source bar's time and the forming bucket's aggregate *before* it (`None`:
    /// it opened the bucket), so a tick on the newest bar costs O(1) whatever the bucket's
    /// length.
    tail: Option<(UtcMillis, Option<Bar>)>,
}

impl Derived {
    /// For a source of at most `capacity` bars (reserved now: updates never allocate). Only
    /// for timeframes built from 1-minute bars (tick bars are [`crate::ticks::TickBars`]).
    pub fn new(tf: Timeframe, capacity: usize) -> Derived {
        debug_assert_eq!(
            tf.source(),
            BarSource::OneMinute,
            "{tf} is not built from bars"
        );
        Derived {
            tf,
            bars: Vec::with_capacity(capacity),
            ends: Vec::with_capacity(capacity),
            partial: Vec::with_capacity(capacity),
            seen: None,
            trimmed: 0,
            period_ms: tf.nominal_ms(),
            cache: (None, None),
            tail: None,
        }
    }

    pub fn timeframe(&self) -> Timeframe {
        self.tf
    }

    pub fn view(&self) -> BarsView<'_> {
        BarsView {
            bars: &self.bars,
            ends: Some(&self.ends),
            period_ms: self.period_ms,
            trades: None,
        }
    }

    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    /// Bars dropped from the front so far, so a chart can keep its view still.
    pub fn trimmed(&self) -> u64 {
        self.trimmed
    }

    /// Whether bar `i` was cut short by a session break.
    pub fn is_partial(&self, i: usize) -> bool {
        self.partial.get(i).copied().unwrap_or(false)
    }

    /// Bring up to date with `src`. Returns whether anything changed.
    pub fn update(&mut self, src: &BarSeries, cal: Option<&SessionCalendar>) -> bool {
        let from = match self.seen {
            Some((rev, trimmed)) if trimmed == src.trimmed() => match src.changed_since(rev) {
                Changed::Nothing => return false,
                Changed::From(t) => Some(t),
                Changed::All => None,
            },
            _ => None,
        };
        self.seen = Some((src.revision(), src.trimmed()));
        let src_bars = src.bars();
        // Fast path: only the newest source bar changed (a live tick).
        if let (Some(t), Some((tail_t, before)), Some(newest), Some(last)) =
            (from, self.tail, src_bars.last(), self.bars.last_mut())
            && t == tail_t
            && newest.time == t
        {
            *last = match before {
                Some(p) => merge(p, newest),
                None => Bar {
                    time: last.time,
                    ..*newest
                },
            };
            return true;
        }
        // Drop the buckets from the one containing `from` on, and re-aggregate from its
        // start; everything before is unchanged.
        let src_start = match from {
            Some(t) => {
                let k = self.ends.partition_point(|e| *e <= t);
                let restart = self.bars.get(k).map_or(t, |b| b.time.min(t));
                self.truncate(k);
                src_bars.partition_point(|b| b.time < restart)
            }
            None => {
                // A rebuild: count the buckets that are gone from the front (a trim).
                if let Some(first) = src_bars.first() {
                    let start = Bucketer::new(self.tf, cal).bucket(first.time).start;
                    self.trimmed += self.bars.partition_point(|b| b.time < start) as u64;
                }
                self.truncate(0);
                0
            }
        };
        let mut bucketer = Bucketer::new(self.tf, cal);
        (bucketer.session, bucketer.week) = self.cache;
        self.tail = None;
        for b in &src_bars[src_start..] {
            let k = bucketer.bucket(b.time);
            match self.bars.last_mut() {
                Some(last) if last.time == k.start => {
                    self.tail = Some((b.time, Some(*last)));
                    *last = merge(*last, b);
                }
                _ => {
                    self.tail = Some((b.time, None));
                    self.bars.push(Bar {
                        time: k.start,
                        ..*b
                    });
                    self.ends.push(k.end);
                    self.partial.push(k.partial);
                }
            }
        }
        self.cache = (bucketer.session, bucketer.week);
        true
    }

    fn truncate(&mut self, n: usize) {
        if n < self.bars.len() {
            self.tail = None;
        }
        self.bars.truncate(n);
        self.ends.truncate(n);
        self.partial.truncate(n);
    }
}

/// `a` followed by `b` in the same bucket.
fn merge(a: Bar, b: &Bar) -> Bar {
    Bar {
        time: a.time,
        open: a.open,
        high: a.high.max(b.high),
        low: a.low.min(b.low),
        close: b.close,
        volume: a.volume.saturating_add(b.volume),
    }
}

#[cfg(test)]
mod tests;
