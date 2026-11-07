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
    /// Identifies the reset period: lines break where it changes.
    pub period: i64,
    /// The newest bucket, still forming.
    pub forming: bool,
}

/// Where a bucket's reset period starts (its key), or `None` if it has no value (shared with
/// cumulative delta).
pub(crate) struct Periods {
    reset: Reset,
    cal: Option<&'static SessionCalendar>,
    /// The last session looked up, `[open, close)` → key (most lookups hit it).
    cache: Option<(i64, i64, i64)>,
}

impl Periods {
    pub(crate) fn new(reset: Reset, cal: Option<&'static SessionCalendar>) -> Periods {
        Periods {
            reset,
            cal,
            cache: None,
        }
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
    /// Aligned with the base buckets: running sums, approx flag, period key.
    run: Vec<Option<(Sums, bool, i64)>>,
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
            periods: Periods {
                reset: spec.reset,
                cal: calendar,
                cache: None,
            },
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
        for (i, b) in buckets.iter().enumerate().skip(start) {
            self.points_before.push(self.points.len() as u32);
            let entry =
                self.periods
                    .key(b.time)
                    .map(|key| match self.run.last().copied().flatten() {
                        Some((mut s, approx, k)) if k == key => {
                            s.add(&b.sums);
                            (s, approx || !b.exact, key)
                        }
                        _ => (b.sums, !b.exact, key),
                    });
            self.run.push(entry);
            if let Some((s, approx, period)) = entry
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
