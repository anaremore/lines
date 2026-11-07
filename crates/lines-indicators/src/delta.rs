//! Cumulative delta (roadmap 10e, decision 0017): the running sum of aggressor delta
//! (buying minus selling) from counted trades, as candles per bucket.
//!
//! - **Exact accumulators:** the base series supplies each bucket's [`DeltaSums`] (exact
//!   `Qty` sums, with the running extremes inside the bucket). A point's candle is the
//!   cumulative delta at the bucket's start (`open`), its extremes inside the bucket
//!   (`high`, `low`) and at its end (`close`): integers, never rounded.
//! - **Resets** ([`DeltaReset`]): each full session (ETH, the default), each regular session
//!   (RTH; no value outside it), or none (one running sum over everything the chart holds).
//! - **Approximate** points: a bucket without trade data (history) contributes its bar's
//!   tick-rule estimate ([`DeltaSums::bar`]), and every point from it to the next reset is
//!   flagged approximate, as VWAP does ("until trade data takes over").
//! - **Incremental:** as VWAP, an update recomputes only from the first changed bucket.

use lines_core::UtcMillis;
use lines_core::sessions::SessionCalendar;
use serde::{Deserialize, Serialize};

use crate::series::{DeltaSums, MarketSeries};
use crate::vwap::{Periods, Reset};

/// When cumulative delta starts over.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaReset {
    /// Each full (electronic) session.
    #[default]
    Eth,
    /// Each regular (day) session; no value outside it.
    Rth,
    /// Never: one running sum.
    None,
}

impl DeltaReset {
    pub fn label(&self) -> &'static str {
        match self {
            DeltaReset::Eth => "ETH",
            DeltaReset::Rth => "RTH",
            DeltaReset::None => "no reset",
        }
    }
}

/// A cumulative delta's identity: with the feed it keys the shared node.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeltaSpec {
    /// Optional in files (absent: ETH).
    #[serde(default)]
    pub reset: DeltaReset,
}

/// One candle of cumulative delta, at a bucket of the base series (quantity units).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DeltaPoint {
    pub time: UtcMillis,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    /// Some bucket since the reset came from bars, not trades.
    pub approx: bool,
    /// Identifies the reset period (candles of different periods aren't connected).
    pub period: i64,
    /// The newest bucket, still forming.
    pub forming: bool,
}

pub struct CumDelta {
    spec: DeltaSpec,
    period_ms: i64,
    periods: Option<Periods>,
    /// Aligned with the base buckets: (cumulative at the bucket's end, approx, period key).
    run: Vec<Option<(i64, bool, i64)>>,
    points: Vec<DeltaPoint>,
    points_before: Vec<u32>,
    seen_drops: u64,
    pub evaluations: u64,
}

impl CumDelta {
    pub fn new(
        spec: DeltaSpec,
        period_ms: i64,
        calendar: Option<&'static SessionCalendar>,
    ) -> CumDelta {
        let reset = match spec.reset {
            DeltaReset::Eth => Some(Reset::Eth),
            DeltaReset::Rth => Some(Reset::Rth),
            DeltaReset::None => None,
        };
        CumDelta {
            spec,
            period_ms,
            periods: reset.map(|r| Periods::new(r, calendar)),
            run: Vec::new(),
            points: Vec::new(),
            points_before: Vec::new(),
            seen_drops: 0,
            evaluations: 0,
        }
    }

    pub fn spec(&self) -> &DeltaSpec {
        &self.spec
    }

    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }

    pub fn points(&self) -> &[DeltaPoint] {
        &self.points
    }

    fn key(&mut self, t: UtcMillis) -> Option<i64> {
        match &mut self.periods {
            Some(p) => p.key(t),
            None => Some(0),
        }
    }

    /// Recompute from the first bucket at or after `from`; returns the index of the first
    /// point that may have changed (as [`crate::vwap::Vwap::update`]).
    pub fn update(&mut self, base: &MarketSeries, from: UtcMillis) -> usize {
        self.evaluations += 1;
        let drops = base.front_drops();
        if drops < self.seen_drops {
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
            let Some(key) = self.key(b.time) else {
                self.run.push(None);
                continue;
            };
            let (before, approx) = match self.run.last().copied().flatten() {
                Some((cum, approx, k)) if k == key => (cum, approx),
                _ => (0, false),
            };
            let d: DeltaSums = b.delta;
            let approx = approx || !b.exact;
            let close = before.saturating_add(d.delta());
            self.run.push(Some((close, approx, key)));
            self.points.push(DeltaPoint {
                time: b.time,
                open: before,
                high: before.saturating_add(d.high),
                low: before.saturating_add(d.low),
                close,
                approx,
                period: key,
                forming: i == last,
            });
        }
        if keep > 0
            && let Some(p) = self.points.get_mut(keep - 1)
        {
            p.forming = false;
            return keep - 1;
        }
        keep
    }
}

#[cfg(test)]
mod tests {
    use lines_core::{Aggressor, Price, Qty};

    use super::*;
    use crate::series::Input;

    const MIN: i64 = 60_000;

    fn trade(t: i64, q: i64, a: Option<Aggressor>) -> Input {
        Input::Trade {
            time: UtcMillis(t),
            price: Price(600_000),
            qty: Qty(q),
            aggressor: a,
        }
    }

    fn base(inputs: &[Input]) -> MarketSeries {
        let mut m = MarketSeries::new(MIN, 1_000);
        m.apply(&[
            Input::Connect {
                expect_history: false,
            },
            Input::TradesFrom(UtcMillis(0)),
        ]);
        m.apply(inputs);
        m
    }

    #[test]
    fn candles_accumulate_exactly_without_reset() {
        use Aggressor::{Buy, Sell};
        let m = base(&[
            trade(1, 5, Some(Buy)),
            trade(2, 8, Some(Sell)), // running -3
            trade(3, 1, Some(Buy)),  // -2
            trade(MIN + 1, 10, Some(Buy)),
            trade(MIN + 2, 4, Some(Sell)),
            trade(MIN + 3, 7, None), // no side: ignored
        ]);
        let mut d = CumDelta::new(
            DeltaSpec {
                reset: DeltaReset::None,
            },
            MIN,
            None,
        );
        d.update(&m, UtcMillis(i64::MIN));
        let p = d.points();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].open, p[0].high, p[0].low, p[0].close), (0, 5, -3, -2));
        assert_eq!((p[1].open, p[1].high, p[1].low, p[1].close), (-2, 8, -2, 4));
        assert!(!p[0].approx && !p[1].approx);
        assert!(!p[0].forming && p[1].forming);
    }

    #[test]
    fn approximate_until_the_next_reset_and_incremental() {
        use Aggressor::{Buy, Sell};
        let mut m = MarketSeries::new(MIN, 1_000);
        m.apply(&[Input::Connect {
            expect_history: true,
        }]);
        // History minute 0 (up bar, volume 10: estimate +10), then live trades from minute 1.
        let hist = lines_core::Bar {
            time: UtcMillis(0),
            open: Price(100),
            high: Price(102),
            low: Price(99),
            close: Price(101),
            volume: Qty(10),
        };
        m.apply(&[
            Input::History {
                bars: vec![hist],
                as_of: Some(UtcMillis(MIN - 1)),
            },
            Input::HistoryComplete,
        ]);
        m.apply(&[trade(MIN + 5, 3, Some(Buy))]);
        let mut d = CumDelta::new(
            DeltaSpec {
                reset: DeltaReset::None,
            },
            MIN,
            None,
        );
        d.update(&m, UtcMillis(i64::MIN));
        let p = d.points();
        assert_eq!((p[0].close, p[0].approx), (10, true));
        assert_eq!(
            (p[1].close, p[1].approx),
            (13, true),
            "approximate since its reset"
        );
        // A new trade in the forming minute: only the last point changes.
        let from = m.apply(&[trade(MIN + 9, 5, Some(Sell))]).unwrap();
        let first = d.update(&m, from);
        // Points before the recomputed one are untouched (the one just before is reported too:
        // its `forming` flag may change, as VWAP's).
        assert_eq!(first, 0);
        assert_eq!(d.points()[0].close, 10);
        assert_eq!(d.points()[1].close, 8);
        assert_eq!(d.evaluations, 2);
    }
}
