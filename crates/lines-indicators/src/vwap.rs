//! VWAP (decision 0017): volume-weighted average price with optional ±1/2/3σ bands.
//!
//! - **Resets** ([`Reset`]): the full session (ETH, the default), the regular session (RTH),
//!   the week, the month (of trade dates, from the product's sessions calendar), or **anchored**
//!   at a chosen bar. Several VWAPs can run on one chart (e.g. ETH and RTH).
//! - **Exact accumulators**: per bucket the base series supplies [`Sums`] (`i128`); a VWAP
//!   point is the running sum since its reset. The value itself is a derived analytic (`f64`),
//!   never snapped to the tick grid or used as an order price.
//! - **Approximate** points: a point is approximate if any bucket since the reset came from a
//!   bar's typical price rather than trades (history without trades). It becomes exact again
//!   at the next reset once trades cover it ("until trade data takes over").
//! - **Late** runs: a run is whole only when the data reaches back to its period's open
//!   (`Periods::open`, from the calendar) and nothing since is known missing
//!   ([`MarketSeries::missing_end`]: a provider's history gap the store doesn't cover). A hole
//!   inside the period starts the sums over after it (a new run, so the line breaks); data that
//!   starts after the open starts the run there. Its points carry [`Point::from`] until the
//!   next whole period: a partial VWAP is never read as the session's.
//! - **Incremental**: an update recomputes only from the first changed bucket, continuing the
//!   running sums from the bucket before it. A live tick touches one point; a history back-fill
//!   recomputes from the first bar it changed.

use lines_core::sessions::{SessionCalendar, SessionKind};
use lines_core::{CivilDate, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::series::{MarketSeries, Sums};

/// When a VWAP starts over.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reset {
    /// Each full (electronic) session; the default.
    Eth,
    /// Each regular (day) session; no value outside it.
    Rth,
    /// Each week of trade dates (Monday to Sunday).
    Weekly,
    /// Each month of trade dates.
    Monthly,
    /// From this bar (its start time, UTC ms) onwards; no value before it.
    Anchored { from: i64 },
}

impl Reset {
    pub fn label(&self) -> &'static str {
        match self {
            Reset::Eth => "ETH",
            Reset::Rth => "RTH",
            Reset::Weekly => "week",
            Reset::Monthly => "month",
            Reset::Anchored { .. } => "anchored",
        }
    }
}

/// Which σ bands to compute and draw (bit `k-1` set = ±kσ).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bands(pub u8);

impl Bands {
    pub const NONE: Bands = Bands(0);
    pub const ALL: Bands = Bands(0b111);

    pub fn has(self, k: u8) -> bool {
        (1..=3).contains(&k) && self.0 & (1 << (k - 1)) != 0
    }
}

/// A VWAP's identity: with the feed it keys the shared node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VwapSpec {
    pub reset: Reset,
    pub bands: Bands,
}

impl Default for VwapSpec {
    fn default() -> Self {
        VwapSpec {
            reset: Reset::Eth,
            bands: Bands::NONE,
        }
    }
}

/// One VWAP value, at a bucket of the base series.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Point {
    pub time: UtcMillis,
    /// Price units (catalog), as `f64`.
    pub vwap: f64,
    /// Volume-weighted standard deviation, price units (0 when bands are off).
    pub sd: f64,
    /// Some bucket since the reset came from bars, not trades.
    pub approx: bool,
    /// Identifies the run (the reset period, or its restart after a known hole): lines break
    /// where it changes.
    pub period: i64,
    /// The newest bucket, still forming.
    pub forming: bool,
    /// The run starts late (`Some`: the first time it covers, UTC ms): the data doesn't reach
    /// back to the period's open, or a known hole since it restarted the sums. `None`: every
    /// trade since the open is in it (exactly, or approximately from bars: `approx`).
    pub from: Option<UtcMillis>,
}

/// A run's running state: sums, approx flag, period key, run id (`Point::period`), late start.
type Run = (Sums, bool, i64, i64, Option<UtcMillis>);

/// Where a bucket's reset period starts (its key), or `None` if it has no value (shared with
/// cumulative delta).
pub(crate) struct Periods {
    reset: Reset,
    cal: Option<&'static SessionCalendar>,
    /// The last session looked up, `[open, close)` → key (most lookups hit it).
    cache: Option<(i64, i64, i64)>,
    /// The last period whose open was looked up: key → open (UTC ms).
    open_cache: Option<(i64, i64)>,
}

impl Periods {
    pub(crate) fn new(reset: Reset, cal: Option<&'static SessionCalendar>) -> Periods {
        Periods {
            reset,
            cal,
            cache: None,
            open_cache: None,
        }
    }

    /// When the period `key` (from [`Periods::key`] at `t`) opened, UTC ms: its session's open
    /// (ETH/RTH, from the sessions calendar, so holidays and early opens are the exchange's),
    /// the week's or month's first session's open, or the anchor.
    pub(crate) fn open(&mut self, key: i64, t: UtcMillis) -> i64 {
        if let Some((k, open)) = self.open_cache
            && k == key
        {
            return open;
        }
        let first_session = |p: &Periods, from: CivilDate, days: i64| -> i64 {
            match p.cal {
                Some(c) => (0..days)
                    .find_map(|d| c.session(from.add_days(d), SessionKind::Eth))
                    .map_or(from.start_utc().0, |s| s.open.0),
                None => from.start_utc().0,
            }
        };
        let open = match self.reset {
            Reset::Eth | Reset::Rth => {
                let kind = if self.reset == Reset::Eth {
                    SessionKind::Eth
                } else {
                    SessionKind::Rth
                };
                match self.cal {
                    Some(c) => c.session_at(t, kind).map_or(t.0, |s| s.open.0),
                    None => CivilDate::from_utc(t).start_utc().0,
                }
            }
            Reset::Weekly => first_session(self, CivilDate::from_days(key), 7),
            Reset::Monthly => {
                // The key is `year * 12 + month` of trade dates: its first trade date's session.
                let (y, m) = ((key - 1).div_euclid(12), (key - 1).rem_euclid(12) + 1);
                let first =
                    CivilDate::new(y as u16, m as u8, 1).unwrap_or_else(|| CivilDate::from_utc(t));
                first_session(self, first, 31)
            }
            Reset::Anchored { from } => from,
        };
        self.open_cache = Some((key, open));
        open
    }

    pub(crate) fn key(&mut self, t: UtcMillis) -> Option<i64> {
        let trade_date = |p: &mut Periods, kind| -> Option<(i64, i64, i64)> {
            if let Some((open, close, key)) = p.cache
                && open <= t.0
                && t.0 < close
            {
                return Some((open, close, key));
            }
            let s = match p.cal {
                Some(c) => c.session_at(t, kind)?,
                None => {
                    let d = CivilDate::from_utc(t);
                    let open = d.start_utc().0;
                    p.cache = Some((open, open + 86_400_000, d.days()));
                    return p.cache;
                }
            };
            p.cache = Some((s.open.0, s.close.0, s.trade_date.days()));
            p.cache
        };
        match self.reset {
            Reset::Eth => trade_date(self, SessionKind::Eth).map(|x| x.2),
            Reset::Rth => {
                if self.cal.is_some_and(|c| !c.has_rth()) {
                    return None;
                }
                trade_date(self, SessionKind::Rth).map(|x| x.2)
            }
            Reset::Weekly => trade_date(self, SessionKind::Eth).map(|x| {
                let d = CivilDate::from_days(x.2);
                // Monday of the trade date's week (weekday(): 0 = Monday).
                x.2 - d.weekday() as i64
            }),
            Reset::Monthly => trade_date(self, SessionKind::Eth).map(|x| {
                let d = CivilDate::from_days(x.2);
                d.year as i64 * 12 + d.month as i64
            }),
            Reset::Anchored { from } => (t.0 >= from).then_some(from),
        }
    }
}

pub struct Vwap {
    spec: VwapSpec,
    period_ms: i64,
    periods: Periods,
    /// Aligned with the base buckets: each one's run state.
    run: Vec<Option<Run>>,
    /// Points for buckets that have a value, in time order.
    points: Vec<Point>,
    /// For each bucket, the number of points before it (to truncate `points` exactly).
    points_before: Vec<u32>,
    /// Base buckets dropped from the front so far (seen).
    seen_drops: u64,
    /// Updates performed (for sharing tests and stats).
    pub evaluations: u64,
}

impl Vwap {
    /// A VWAP over the base series of bar period `period_ms`.
    pub fn new(spec: VwapSpec, period_ms: i64, calendar: Option<&'static SessionCalendar>) -> Vwap {
        Vwap {
            spec,
            period_ms,
            periods: Periods::new(spec.reset, calendar),
            run: Vec::new(),
            points: Vec::new(),
            points_before: Vec::new(),
            seen_drops: 0,
            evaluations: 0,
        }
    }

    pub fn spec(&self) -> &VwapSpec {
        &self.spec
    }

    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// Recompute from the first bucket at or after `from`; returns the index of the first point
    /// that may have changed (points before it are untouched). That includes the point just
    /// before the recomputed ones, whose `forming` flag may have been cleared.
    pub fn update(&mut self, base: &MarketSeries, from: UtcMillis) -> usize {
        self.evaluations += 1;
        // Follow buckets the base dropped from its front (bounded memory).
        let drops = base.front_drops();
        if drops < self.seen_drops {
            // The base started over (a contract change): so do we.
            self.seen_drops = drops;
            self.run.clear();
            self.points.clear();
            self.points_before.clear();
        }
        if drops > self.seen_drops {
            let k = ((drops - self.seen_drops) as usize).min(self.run.len());
            self.seen_drops = drops;
            self.run.drain(..k);
            let gone = if k < self.points_before.len() {
                self.points_before[k] as usize
            } else {
                self.points.len()
            };
            self.points_before.drain(..k);
            self.points.drain(..gone.min(self.points.len()));
            for n in &mut self.points_before {
                *n -= gone as u32;
            }
        }
        let buckets = base.buckets();
        let start = buckets
            .partition_point(|b| b.time < from)
            .min(self.run.len());
        let keep = if start < self.points_before.len() {
            self.points_before[start] as usize
        } else {
            self.points.len()
        };
        self.run.truncate(start);
        self.points_before.truncate(start);
        self.points.truncate(keep);
        let last = buckets.len().saturating_sub(1);
        let p = self.period_ms;
        for (i, b) in buckets.iter().enumerate().skip(start) {
            self.points_before.push(self.points.len() as u32);
            let entry = self.periods.key(b.time).map(|key| {
                let end = b.time.0.saturating_add(p);
                match self.run.last().copied().flatten() {
                    // The same period, and nothing known missing since the previous bucket:
                    // the run goes on.
                    Some((mut s, approx, k, id, from))
                        if k == key
                            && base
                                .missing_end(buckets[i - 1].time.0.saturating_add(p), end)
                                .is_none() =>
                    {
                        s.add(&b.sums);
                        (s, approx || !b.exact, key, id, from)
                    }
                    // A known hole inside the period: the sums start over after it, and say so
                    // (never a VWAP that silently skips trades).
                    Some((_, _, k, _, _)) if k == key => {
                        let open = self.periods.open(key, b.time);
                        let late = base.missing_end(open, end).unwrap_or(b.time.0).max(open);
                        (b.sums, !b.exact, key, late, Some(UtcMillis(late)))
                    }
                    // A new period: whole if the data reaches back to its open and nothing in
                    // between is known missing.
                    _ => {
                        let open = self.periods.open(key, b.time);
                        let mut late = base.missing_end(open, end);
                        if let Some(known) = base.known_from()
                            && known > open
                        {
                            late = Some(late.map_or(known, |l| l.max(known)));
                        }
                        let late = late.map(|l| UtcMillis(l.max(open)));
                        let id = late.map_or(key, |l| l.0);
                        (b.sums, !b.exact, key, id, late)
                    }
                }
            });
            self.run.push(entry);
            if let Some((s, approx, _, period, from)) = entry
                && let Some(vwap) = s.mean()
            {
                let sd = if self.spec.bands == Bands::NONE {
                    0.0
                } else {
                    s.std_dev().unwrap_or(0.0)
                };
                self.points.push(Point {
                    time: b.time,
                    vwap,
                    sd,
                    approx,
                    period,
                    forming: i == last,
                    from,
                });
            }
        }
        // Only a point at the newest bucket is forming (new points were pushed that way); the
        // point before the recomputed ones may have been the newest until now.
        if keep > 0
            && let Some(p) = self.points.get_mut(keep - 1)
        {
            p.forming = false;
            // That point changed too (its flag): copies must take it again.
            return keep - 1;
        }
        keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::sessions::{ExchangeHours, HolidayTable, Hours, LocalTime, SessionSpec};

    /// CME equity hours with 2026's New Year's Day (closed) and Martin Luther King Jr. Day (a
    /// Monday halt: not a trade date) from an inline table.
    fn calendar() -> &'static SessionCalendar {
        let table = HolidayTable::parse(
            "format 1\nversion 2026-11-10\ncalendar cme-equity-index\n\
             coverage 2026 2026 unverified\nsource test\n\
             2026-01-01 closed unverified New Year's Day\n\
             2026-01-19 halt 12:00 unverified Martin Luther King Jr. Day\n",
        )
        .unwrap();
        let spec = SessionSpec {
            time_zone: "America/Chicago",
            hours: Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(17, 0),
                eth_close: LocalTime::hm(16, 0),
                rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
            }),
            holidays: Some("cme-equity-index"),
        };
        Box::leak(Box::new(SessionCalendar::new(spec, Some(&table)).unwrap()))
    }

    fn at(iso: &str) -> UtcMillis {
        UtcMillis::parse_iso(iso).unwrap()
    }

    fn open(reset: Reset, t: &str) -> UtcMillis {
        let mut p = Periods::new(reset, Some(calendar()));
        let t = at(t);
        let key = p.key(t).expect("a period");
        UtcMillis(p.open(key, t))
    }

    /// A week opens at its first session's open: Sunday 17:00 CT, also when that week's Monday
    /// is a holiday halt (its session is Tuesday's, opened Sunday evening).
    #[test]
    fn a_week_opens_at_its_first_session_holiday_monday_included() {
        assert_eq!(
            open(Reset::Weekly, "2026-11-12T15:00:00Z"),
            at("2026-11-08T23:00:00Z")
        );
        // MLK week: Monday 01-19 isn't a trade date; Tuesday's session opened Sunday 17:00 CT.
        assert_eq!(
            open(Reset::Weekly, "2026-01-21T15:00:00Z"),
            at("2026-01-18T23:00:00Z")
        );
        // Inside the halted Monday itself (its morning belongs to Tuesday's session).
        assert_eq!(
            open(Reset::Weekly, "2026-01-19T16:00:00Z"),
            at("2026-01-18T23:00:00Z")
        );
    }

    /// A month opens at its first trade date's session open: November 2026's at Sunday
    /// 2026-11-01 17:00 CST; January's first trade date is Friday the 2nd (New Year's Day is
    /// closed), its session opening on the 1st at 17:00 CST.
    #[test]
    fn a_month_opens_at_its_first_trade_dates_session() {
        assert_eq!(
            open(Reset::Monthly, "2026-11-12T15:00:00Z"),
            at("2026-11-01T23:00:00Z")
        );
        let jan = open(Reset::Monthly, "2026-01-20T15:00:00Z");
        let first = calendar()
            .session(CivilDate::new(2026, 1, 2).unwrap(), SessionKind::Eth)
            .unwrap();
        assert_eq!(jan, first.open);
        assert_eq!(jan, at("2026-01-01T23:00:00Z"));
    }
}
