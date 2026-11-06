//! Stitching history and live trades into one bar series (roadmap 4).
//!
//! # The rule
//!
//! A history snapshot is a set of bars plus `as_of`, a time with *closed* semantics: the bars
//! include **every** trade with `time <= as_of` and **no** trade after it. (CQG documents
//! exactly this for `TimeBarReport.up_to_utc_time`: "if there were several with the same
//! time-stamp equal to this time then all are included".)
//!
//! - A live trade with `time <= as_of` is already counted: it is **dropped**.
//! - A live trade with `time > as_of` is **added**.
//! - A boundary trade with `time == as_of` is therefore dropped. That is correct only because
//!   `as_of` is closed; providers that can't guarantee it must pick a closed `as_of` (Coinbase
//!   candles are closed at the bar boundary: `as_of = boundary - 1 ms`) or de-duplicate by
//!   trade identity before the trade reaches here (Coinbase REST trades vs websocket trades,
//!   by trade id).
//! - History may arrive *after* live trades have started (e.g. a REST response that is slower
//!   than the websocket). History bars overwrite the stored bars; live trades with
//!   `time > as_of` that had been applied to an overwritten bar are re-applied from a bounded
//!   journal, so nothing is lost and nothing counted twice.
//! - Trades can arrive out of time order (REST back-fill after newer live trades). Open and
//!   close follow trade *time*, not arrival order; high, low and volume are order-free.
//!
//! On every (re)connect the provider requests history from the start of the newest stored bar
//! (the last complete bar is never re-requested; the newest, possibly partial, one is
//! re-requested whole) and the snapshot overwrites it.

use std::collections::VecDeque;

use crate::bars::{Bar, BarSeries};
use crate::price::Price;
use crate::qty::Qty;
use crate::time::UtcMillis;

/// Buckets of recent trades kept for re-application once history has arrived.
const KEEP_BUCKETS: i64 = 3;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Trade {
    time: UtcMillis,
    price: Price,
    qty: Qty,
}

/// Earliest and latest trade time folded into one bar (for time-ordered open/close).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Span {
    bucket: UtcMillis,
    first: UtcMillis,
    last: UtcMillis,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StitchStats {
    /// Live trades dropped because a history snapshot already counted them.
    pub covered_dropped: u64,
    /// Live trades re-applied after history overwrote their bar.
    pub reapplied: u64,
    /// Journal entries discarded because the journal hit its cap while history was
    /// outstanding (those trades could be lost if history overwrites their bar later).
    pub journal_overflow: u64,
}

#[derive(Clone, Debug)]
pub struct Stitcher {
    period_ms: i64,
    as_of: Option<UtcMillis>,
    /// Live trades with `time > as_of`, in arrival order.
    journal: VecDeque<Trade>,
    journal_cap: usize,
    /// History requested on this connection and not yet complete: keep the whole journal.
    awaiting: bool,
    spans: VecDeque<Span>,
    pub stats: StitchStats,
}

impl Stitcher {
    pub fn new(period_ms: i64, journal_cap: usize) -> Self {
        Stitcher {
            period_ms,
            as_of: None,
            journal: VecDeque::new(),
            journal_cap: journal_cap.max(1),
            awaiting: false,
            spans: VecDeque::new(),
            stats: StitchStats::default(),
        }
    }

    /// Newest history snapshot time applied.
    pub fn as_of(&self) -> Option<UtcMillis> {
        self.as_of
    }

    pub fn journal_len(&self) -> usize {
        self.journal.len()
    }

    /// A connection (or reconnect) starts. `expect_history`: the provider will send history,
    /// so keep every live trade until it is complete.
    pub fn begin_connection(&mut self, expect_history: bool) {
        self.awaiting = expect_history;
    }

    /// The series was replaced (contract change): forget everything.
    pub fn reset(&mut self) {
        self.as_of = None;
        self.journal.clear();
        self.spans.clear();
    }

    fn span_mut(&mut self, bucket: UtcMillis) -> Option<&mut Span> {
        self.spans.iter_mut().find(|s| s.bucket == bucket)
    }

    /// Apply one trade with time-ordered open/close.
    fn fold(&mut self, series: &mut BarSeries, t: Trade) {
        let bucket = t.time.floor_to(self.period_ms);
        let has_bar = series.index_at(t.time).is_ok();
        let (first, last) = match self.span_mut(bucket) {
            Some(s) => {
                let r = (t.time < s.first, t.time >= s.last);
                s.first = s.first.min(t.time);
                s.last = s.last.max(t.time);
                r
            }
            None => {
                // Unknown bar (older than the tracked window but present): extremes and volume
                // only. A new bar sets everything regardless.
                if !has_bar || series.last().is_some_and(|b| b.time < bucket) {
                    self.track(bucket, t.time, t.time);
                }
                (false, !has_bar)
            }
        };
        series.apply_trade_ordered(t.time, t.price, t.qty, first, last);
    }

    fn track(&mut self, bucket: UtcMillis, first: UtcMillis, last: UtcMillis) {
        match self.span_mut(bucket) {
            Some(s) => {
                *s = Span {
                    bucket,
                    first,
                    last,
                }
            }
            None => {
                self.spans.push_back(Span {
                    bucket,
                    first,
                    last,
                });
                // Evict the *oldest bucket*, not the oldest insertion: a history page tracking
                // older buckets must never push out the forming bar's span.
                if self.spans.len() > KEEP_BUCKETS as usize + 2
                    && let Some(i) = (0..self.spans.len()).min_by_key(|&i| self.spans[i].bucket)
                {
                    self.spans.remove(i);
                }
            }
        }
    }

    /// A live trade. Returns whether it was applied.
    pub fn trade(
        &mut self,
        series: &mut BarSeries,
        time: UtcMillis,
        price: Price,
        qty: Qty,
    ) -> bool {
        if self.as_of.is_some_and(|s| time <= s) {
            self.stats.covered_dropped += 1;
            return false;
        }
        let t = Trade { time, price, qty };
        self.fold(series, t);
        self.journal.push_back(t);
        self.trim_journal(time);
        true
    }

    fn trim_journal(&mut self, newest: UtcMillis) {
        if !self.awaiting {
            let keep_from =
                UtcMillis(newest.floor_to(self.period_ms).0 - (KEEP_BUCKETS - 1) * self.period_ms);
            while self.journal.front().is_some_and(|t| t.time < keep_from) {
                self.journal.pop_front();
            }
        }
        while self.journal.len() > self.journal_cap {
            self.journal.pop_front();
            if self.awaiting {
                self.stats.journal_overflow += 1;
            }
        }
    }

    /// Apply a history snapshot. `as_of` is closed (see the module docs); `None` means the
    /// snapshot holds only complete bars, closed at the end of its last bar.
    pub fn history(&mut self, series: &mut BarSeries, bars: &[Bar], as_of: Option<UtcMillis>) {
        let as_of = match (as_of, bars.last()) {
            (Some(s), _) => s,
            (None, Some(b)) => UtcMillis(b.time.0 + self.period_ms - 1),
            (None, None) => return,
        };
        for b in bars {
            series.upsert(*b);
            // The bar holds every trade up to `as_of` in its bucket; first trade unknown.
            let end = UtcMillis(b.time.0 + self.period_ms - 1);
            self.track(b.time, UtcMillis(i64::MIN), end.min(as_of));
        }
        // Re-apply journaled trades newer than the snapshot whose bar was just overwritten,
        // in time order.
        let mut redo: Vec<Trade> = self
            .journal
            .iter()
            .copied()
            .filter(|t| {
                t.time > as_of
                    && bars
                        .binary_search_by_key(&t.time.floor_to(self.period_ms), |b| b.time)
                        .is_ok()
            })
            .collect();
        redo.sort_by_key(|t| t.time);
        for t in redo {
            self.fold(series, t);
            self.stats.reapplied += 1;
        }
        self.as_of = Some(self.as_of.map_or(as_of, |s| s.max(as_of)));
        let cutoff = self.as_of.expect("set above");
        self.journal.retain(|t| t.time > cutoff);
    }

    /// The provider has sent all history for this connection.
    pub fn history_complete(&mut self) {
        self.awaiting = false;
        if let Some(t) = self.journal.back().map(|t| t.time) {
            self.trim_journal(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: i64 = 60_000;

    fn t(ms: i64) -> UtcMillis {
        UtcMillis(ms)
    }
    fn bar(time: i64, o: i64, h: i64, l: i64, c: i64, v: i64) -> Bar {
        Bar {
            time: t(time),
            open: Price(o),
            high: Price(h),
            low: Price(l),
            close: Price(c),
            volume: Qty(v),
        }
    }
    fn setup() -> (Stitcher, BarSeries) {
        (Stitcher::new(M, 10_000), BarSeries::new(M, 1000))
    }

    /// Reference: aggregate a full trade list (the "exchange truth").
    fn truth(trades: &[(i64, i64, i64)]) -> Vec<Bar> {
        let mut s = BarSeries::new(M, 1000);
        let mut sorted = trades.to_vec();
        sorted.sort_by_key(|x| x.0);
        for (time, p, q) in sorted {
            s.apply_trade(t(time), Price(p), Qty(q));
        }
        s.bars().to_vec()
    }

    /// History as the server would build it from `trades` up to and including `as_of`.
    fn snapshot(trades: &[(i64, i64, i64)], as_of: i64) -> Vec<Bar> {
        let upto: Vec<_> = trades.iter().copied().filter(|x| x.0 <= as_of).collect();
        truth(&upto)
    }

    #[test]
    fn clean_backfill_then_live() {
        let all = [
            (1_000, 100, 1),
            (61_000, 101, 2),
            (90_000, 102, 3),
            (95_000, 99, 1),
            (125_000, 103, 4),
        ];
        let (mut st, mut s) = setup();
        st.begin_connection(true);
        st.history(&mut s, &snapshot(&all, 92_000), Some(t(92_000)));
        st.history_complete();
        for &(time, p, q) in all.iter().filter(|x| x.0 > 92_000) {
            assert!(st.trade(&mut s, t(time), Price(p), Qty(q)));
        }
        assert_eq!(s.bars(), truth(&all).as_slice());
    }

    #[test]
    fn boundary_trade_exactly_at_as_of_is_counted_once() {
        // Two trades share the snapshot millisecond; the closed snapshot includes both.
        let all = [
            (60_500, 100, 1),
            (61_000, 105, 2),
            (61_000, 104, 3),
            (61_001, 106, 4),
        ];
        let (mut st, mut s) = setup();
        st.begin_connection(true);
        st.history(&mut s, &snapshot(&all, 61_000), Some(t(61_000)));
        st.history_complete();
        // Live feed replays the boundary trades too (overlap) and then the next one.
        assert!(
            !st.trade(&mut s, t(61_000), Price(105), Qty(2)),
            "at as_of: already counted"
        );
        assert!(!st.trade(&mut s, t(61_000), Price(104), Qty(3)));
        assert!(
            st.trade(&mut s, t(61_001), Price(106), Qty(4)),
            "1 ms later: new"
        );
        assert_eq!(s.bars(), truth(&all).as_slice());
        assert_eq!(s.bars()[0].volume, Qty(10));
        assert_eq!(st.stats.covered_dropped, 2);
    }

    #[test]
    fn history_arriving_after_live_trades_started() {
        let all = [
            (10_000, 100, 1),
            (70_000, 101, 2),
            (80_000, 103, 1),
            (85_000, 98, 5),
            (90_000, 102, 1),
            (130_000, 104, 2),
        ];
        let (mut st, mut s) = setup();
        st.begin_connection(true);
        // Websocket delivers trades from 80 s on before the REST snapshot (as_of 84 999) lands.
        for &(time, p, q) in all.iter().filter(|x| x.0 >= 80_000) {
            st.trade(&mut s, t(time), Price(p), Qty(q));
        }
        st.history(&mut s, &snapshot(&all, 84_999), Some(t(84_999)));
        st.history_complete();
        // The 80 s trade was in both: counted once. 85 s and 90 s were re-applied on top.
        assert_eq!(s.bars(), truth(&all).as_slice());
        assert_eq!(st.stats.reapplied, 2);
    }

    #[test]
    fn reconnect_mid_bar() {
        let all = [
            (61_000, 100, 1),
            (65_000, 101, 1),
            (70_000, 99, 1),
            (75_000, 103, 1),
            (80_000, 102, 1),
            (121_000, 104, 1),
        ];
        let (mut st, mut s) = setup();
        st.begin_connection(true);
        st.history(&mut s, &snapshot(&all, 62_000), Some(t(62_000)));
        st.history_complete();
        st.trade(&mut s, t(65_000), Price(101), Qty(1));
        // Disconnected: 70 s and 75 s never arrive live. Reconnect requests from the newest
        // stored bar (60 s) and gets a snapshot as of 78 s.
        st.begin_connection(true);
        let from_newest: Vec<_> = snapshot(&all, 78_000)
            .into_iter()
            .filter(|b| b.time.0 >= 60_000)
            .collect();
        st.history(&mut s, &from_newest, Some(t(78_000)));
        st.history_complete();
        st.trade(&mut s, t(75_000), Price(103), Qty(1)); // late duplicate: dropped
        st.trade(&mut s, t(80_000), Price(102), Qty(1));
        st.trade(&mut s, t(121_000), Price(104), Qty(1));
        assert_eq!(s.bars(), truth(&all).as_slice());
    }

    #[test]
    fn out_of_order_trades_keep_time_ordered_open_and_close() {
        let (mut st, mut s) = setup();
        st.begin_connection(false);
        // Newer trades first (websocket), then older ones (REST back-fill).
        st.trade(&mut s, t(30_000), Price(105), Qty(1));
        st.trade(&mut s, t(40_000), Price(107), Qty(1));
        st.trade(&mut s, t(10_000), Price(100), Qty(1));
        st.trade(&mut s, t(20_000), Price(103), Qty(1));
        assert_eq!(s.bars(), &[bar(0, 100, 107, 100, 107, 4)]);
    }

    #[test]
    fn history_for_older_buckets_keeps_the_forming_bars_order() {
        // Live trades start bar 5 (5:00); a candle page then covers buckets 0..=4 (as_of just
        // before bar 5); then older REST trades of bar 5 arrive, then newer live ones.
        let (mut st, mut s) = setup();
        st.begin_connection(true);
        st.trade(&mut s, t(5 * M + 30_000), Price(205), Qty(1));
        let page: Vec<Bar> = (0..5).map(|i| bar(i * M, 100, 101, 99, 100, 1)).collect();
        st.history(&mut s, &page, Some(t(5 * M - 1)));
        st.trade(&mut s, t(5 * M), Price(200), Qty(1)); // older: becomes the open
        st.trade(&mut s, t(5 * M + 10_000), Price(210), Qty(1));
        st.trade(&mut s, t(5 * M + 40_000), Price(207), Qty(1)); // newest: the close
        st.history_complete();
        assert_eq!(s.bars().last(), Some(&bar(5 * M, 200, 210, 200, 207, 4)));
    }

    #[test]
    fn empty_history_changes_nothing() {
        let (mut st, mut s) = setup();
        st.begin_connection(true);
        st.trade(&mut s, t(1_000), Price(100), Qty(1));
        st.history(&mut s, &[], None);
        st.history_complete();
        assert_eq!(st.as_of(), None);
        assert_eq!(s.bars(), &[bar(0, 100, 100, 100, 100, 1)]);
        assert!(st.trade(&mut s, t(2_000), Price(101), Qty(1)));
    }

    #[test]
    fn complete_bars_only_snapshot_is_closed_at_its_last_bar_end() {
        let (mut st, mut s) = setup();
        st.history(&mut s, &[bar(0, 100, 101, 99, 100, 5)], None);
        assert_eq!(st.as_of(), Some(t(59_999)));
        assert!(!st.trade(&mut s, t(59_999), Price(1), Qty(1)));
        assert!(st.trade(&mut s, t(60_000), Price(102), Qty(1)));
    }

    #[test]
    fn journal_is_bounded() {
        let mut st = Stitcher::new(M, 5);
        let mut s = BarSeries::new(M, 1000);
        st.begin_connection(true);
        for i in 0..20 {
            st.trade(&mut s, t(i * 1000), Price(100), Qty(1));
        }
        assert_eq!(st.journal_len(), 5);
        assert_eq!(
            st.stats.journal_overflow, 15,
            "overflow while awaiting history is reported"
        );
        // After history completes, only the newest buckets are kept.
        let mut st = Stitcher::new(M, 10_000);
        st.begin_connection(false);
        for i in 0..1000 {
            st.trade(&mut s, t(i * 10_000), Price(100), Qty(1));
        }
        assert!(st.journal_len() <= (KEEP_BUCKETS as usize) * 6);
    }
}
