//! The chart's bars as an engine series (roadmap 10a): what bar studies (moving averages,
//! Bollinger Bands, and later ATR and RSI) are computed on.
//!
//! A bar study works on the bars the chart shows, whatever they are: EMA 20 on a 5-minute
//! chart is twenty 5-minute bars, on a 500-tick chart twenty 500-tick bars. So the engine
//! keeps a [`Spec::Bars`](crate::Spec::Bars) node per (feed, [`ChartBars`]):
//! - **Time bars** (1 minute and every timeframe built from it): [`TimeBars`] derives them in
//!   the engine from the feed's base series ([`MarketSeries`]), with the same
//!   `lines_core::timeframe::Derived` aggregation over the same stitched 1-minute bars the
//!   chart uses, so they are the chart's bars exactly.
//! - **Trade-built bars** (tick, volume, range, seconds): the chart's store builds them from
//!   counted trades (with rebuilds for charts that ask late); the engine can't rebuild them
//!   (it keeps sums, not trades), so the runtime copies them into an [`ExternalBars`] node
//!   ([`Graph::set_bars`](crate::Graph::set_bars)), only the bars that changed.
//!
//! Either way the node's output is [`BarsOut`]: the bars and how many were dropped from the
//! front so far (bounded memory), so consumers keep their own arrays aligned.

use lines_core::sessions::SessionCalendar;
use lines_core::timeframe::BarSource;
use lines_core::{Bar, Derived, TickReset, Timeframe, UtcMillis};

use crate::series::MarketSeries;

/// Which bars of a feed a bar study runs on: the chart's timeframe and, for trade-built bars,
/// whether they reset at the session open. Normalized by [`ChartBars::new`], so two charts
/// showing the same bars share one node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChartBars {
    pub tf: Timeframe,
    pub reset: TickReset,
}

impl Default for ChartBars {
    fn default() -> Self {
        ChartBars::new(Timeframe::ONE_MINUTE, TickReset::Session)
    }
}

impl ChartBars {
    /// `reset` matters only for trade-built bars (time bars always break at sessions).
    pub fn new(tf: Timeframe, reset: TickReset) -> ChartBars {
        let reset = if tf.source() == BarSource::Trades {
            reset
        } else {
            TickReset::Session
        };
        ChartBars { tf, reset }
    }

    /// Built from counted trades (the store's tick builder), not from 1-minute bars.
    pub fn from_trades(&self) -> bool {
        self.tf.source() == BarSource::Trades
    }
}

/// A bars node's output.
#[derive(Copy, Clone, Debug)]
pub struct BarsOut<'a> {
    pub bars: &'a [Bar],
    /// Bars dropped from the front so far; a smaller value than last seen means the series
    /// started over (a contract change, a rebuilt tick series).
    pub trimmed: u64,
}

/// Time bars derived in the engine from the base series (a timeframe built from 1-minute bars,
/// other than 1 minute itself: studies on 1 minute read the base series directly).
pub struct TimeBars {
    period_ms: i64,
    tf: Timeframe,
    cal: Option<&'static SessionCalendar>,
    derived: Derived,
    capacity: usize,
    /// The base series' epoch this follows (it changes when the base starts over).
    epoch: u64,
    pub evaluations: u64,
}

impl TimeBars {
    pub fn new(
        period_ms: i64,
        tf: Timeframe,
        cal: Option<&'static SessionCalendar>,
        capacity: usize,
    ) -> TimeBars {
        debug_assert_eq!(tf.source(), BarSource::OneMinute, "{tf} is built from trades");
        TimeBars {
            period_ms,
            tf,
            cal,
            derived: Derived::new(tf, capacity),
            capacity,
            epoch: 0,
            evaluations: 0,
        }
    }

    /// The base series' bar period (the input spec).
    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }

    /// Bring up to date with the base series (`Derived` re-aggregates only what changed).
    pub fn update(&mut self, base: &MarketSeries) {
        self.evaluations += 1;
        if base.epoch() != self.epoch {
            // The base started over (a roll): so do the derived bars.
            self.epoch = base.epoch();
            self.derived = Derived::new(self.tf, self.capacity);
        }
        self.derived.update(base.bar_series(), self.cal);
    }

    pub fn output(&self) -> BarsOut<'_> {
        BarsOut {
            bars: self.derived.bars(),
            trimmed: self.derived.trimmed(),
        }
    }
}

/// Bars set from outside the graph: a copy of the store's trade-built bars.
pub struct ExternalBars {
    bars: Vec<Bar>,
    trimmed: u64,
    /// Updates that changed something (tests, stats).
    pub syncs: u64,
}

impl ExternalBars {
    pub fn new(capacity: usize) -> ExternalBars {
        ExternalBars {
            bars: Vec::with_capacity(capacity.min(1 << 20)),
            trimmed: 0,
            syncs: 0,
        }
    }

    pub fn output(&self) -> BarsOut<'_> {
        BarsOut {
            bars: &self.bars,
            trimmed: self.trimmed,
        }
    }

    /// Follow `src` (the store's bars, `trimmed` dropped from its front so far). Returns the
    /// first changed bar's time (`i64::MIN`: everything, the series started over), or `None`
    /// if nothing changed.
    ///
    /// A live trade changes only the newest bar or appends: the bars before our newest are
    /// compared at the ends of the kept run (its first and its last-but-one bar), and only
    /// from our newest on are bars compared and copied. A rebuild (the store replaced its
    /// builder: a lower trim count, or the kept run's ends differ) copies everything. Bars
    /// are built deterministically from counted trades, so two series that agree on those
    /// bars agree on the run between them.
    pub fn sync(&mut self, src: &[Bar], trimmed: u64) -> Option<UtcMillis> {
        let dropped = trimmed.checked_sub(self.trimmed).map(|k| k as usize);
        let continues = dropped.is_some_and(|k| {
            k <= self.bars.len() && {
                let kept = &self.bars[k..];
                let n = kept.len();
                n <= 1 || (src.len() + 1 >= n && src[0] == kept[0] && src[n - 2] == kept[n - 2])
            }
        });
        if !continues {
            self.bars.clear();
            self.bars.extend_from_slice(src);
            self.trimmed = trimmed;
            self.syncs += 1;
            return Some(UtcMillis(i64::MIN));
        }
        let k = dropped.unwrap_or(0);
        self.bars.drain(..k);
        self.trimmed = trimmed;
        let from = self.bars.len().saturating_sub(1);
        let first = (from..self.bars.len().max(src.len()))
            .find(|&i| self.bars.get(i) != src.get(i))?;
        self.bars.truncate(first);
        self.bars.extend_from_slice(&src[first.min(src.len())..]);
        self.syncs += 1;
        Some(match src.get(first) {
            Some(b) => b.time,
            // The source lost its newest bars (shouldn't happen): recompute the tail.
            None => self.bars.last().map_or(UtcMillis(i64::MIN), |b| b.time),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{Price, Qty};

    fn bar(t: i64, c: i64) -> Bar {
        Bar {
            time: UtcMillis(t),
            open: Price(c),
            high: Price(c + 1),
            low: Price(c - 1),
            close: Price(c),
            volume: Qty(1),
        }
    }

    #[test]
    fn external_bars_copy_only_what_changed() {
        let mut e = ExternalBars::new(16);
        let mut src: Vec<Bar> = (0..5).map(|i| bar(i * 10, 100 + i)).collect();
        assert_eq!(e.sync(&src, 0), Some(UtcMillis(0)));
        assert_eq!(e.output().bars, &src[..]);
        assert_eq!(e.sync(&src, 0), None, "nothing changed");
        // A tick on the newest bar.
        src[4].close = Price(200);
        assert_eq!(e.sync(&src, 0), Some(UtcMillis(40)));
        // A new bar.
        src.push(bar(50, 7));
        assert_eq!(e.sync(&src, 0), Some(UtcMillis(50)));
        assert_eq!(e.output().bars, &src[..]);
        // Two dropped from the front.
        src.drain(..2);
        src.push(bar(60, 8));
        assert_eq!(e.sync(&src, 2), Some(UtcMillis(60)));
        assert_eq!(e.output().bars, &src[..]);
        assert_eq!(e.output().trimmed, 2);
        // A rebuild with more history: everything again.
        let rebuilt: Vec<Bar> = (-3..7).map(|i| bar(i * 10, 50 + i)).collect();
        assert_eq!(e.sync(&rebuilt, 0), Some(UtcMillis(i64::MIN)));
        assert_eq!(e.output().bars, &rebuilt[..]);
        assert_eq!(e.output().trimmed, 0);
        // Emptied (a roll): everything (nothing).
        assert_eq!(e.sync(&[], 0), Some(UtcMillis(i64::MIN)));
        assert!(e.output().bars.is_empty());
    }

    #[test]
    fn chart_bars_normalize_the_reset_of_time_bars() {
        assert_eq!(
            ChartBars::new(Timeframe::Minutes(5), TickReset::Continuous),
            ChartBars::new(Timeframe::Minutes(5), TickReset::Session)
        );
        assert_ne!(
            ChartBars::new(Timeframe::Ticks(500), TickReset::Continuous),
            ChartBars::new(Timeframe::Ticks(500), TickReset::Session)
        );
        assert!(ChartBars::new(Timeframe::Range(10), TickReset::Session).from_trades());
        assert!(!ChartBars::default().from_trades());
    }
}
