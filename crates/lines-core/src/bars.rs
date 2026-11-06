//! OHLCV bars and live aggregation.
//!
//! Ownership model: history arrives as complete bars from the server (CQG `TimeBarReport`);
//! live trades are folded into the forming bar locally so the chart moves with every tick;
//! server bar updates for the same bucket overwrite the local bar (server is authoritative).

use crate::price::Price;
use crate::qty::Qty;
use crate::time::UtcMillis;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bar {
    /// Bucket start (inclusive).
    pub time: UtcMillis,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    /// Exact traded quantity (decision 0002).
    pub volume: Qty,
}

impl Bar {
    pub fn from_trade(time: UtcMillis, price: Price, volume: Qty) -> Bar {
        Bar {
            time,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
        }
    }

    /// See [`BarSeries::apply_trade_ordered`].
    #[inline]
    pub fn apply_trade_ordered(&mut self, price: Price, volume: Qty, first: bool, last: bool) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        if first {
            self.open = price;
        }
        if last {
            self.close = price;
        }
        self.volume = self.volume.saturating_add(volume);
    }

    #[inline]
    pub fn apply_trade(&mut self, price: Price, volume: Qty) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        self.volume = self.volume.saturating_add(volume);
    }
}

/// Bars of fixed period, sorted by time, unique per bucket.
#[derive(Debug, Clone)]
pub struct BarSeries {
    period_ms: i64,
    bars: Vec<Bar>,
    max_len: usize,
    /// Bumped on every mutation. The UI compares it to decide whether to redraw.
    revision: u64,
    /// Number of bars ever removed from the front (lets views keep a stable index anchor).
    trimmed: u64,
    /// The earliest bar time each recent revision touched (a ring of the last
    /// [`CHANGE_LOG`] mutations), so derived views (higher timeframes) recompute only from
    /// there: [`BarSeries::changed_since`].
    changes: [(u64, i64); CHANGE_LOG],
    change_head: usize,
}

/// Mutations remembered by [`BarSeries::changed_since`].
pub const CHANGE_LOG: usize = 32;

/// What changed in a series since a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    Nothing,
    /// Bars at or after this time (bucket start) may differ.
    From(UtcMillis),
    /// Too much to tell (more mutations than remembered, or cleared): rebuild everything.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeOutcome {
    UpdatedLast,
    NewBar,
    /// Trade belongs to an older bucket (late/out-of-order); applied in place.
    Late,
    /// Older than anything we keep; dropped.
    TooOld,
}

impl BarSeries {
    pub fn new(period_ms: i64, max_len: usize) -> Self {
        assert!(period_ms > 0 && max_len > 0);
        BarSeries {
            period_ms,
            // trim() lets len reach max_len + slack + 1 before draining; reserve exactly that.
            bars: Vec::with_capacity(max_len + max_len / 4 + 2),
            max_len,
            revision: 0,
            trimmed: 0,
            changes: [(0, 0); CHANGE_LOG],
            change_head: 0,
        }
    }

    /// Record that the current revision touched bars from `t` on.
    fn note_change(&mut self, t: i64) {
        self.changes[self.change_head] = (self.revision, t);
        self.change_head = (self.change_head + 1) % CHANGE_LOG;
    }

    /// What changed since `revision` (allocation-free). Trimming the front is reported by
    /// [`BarSeries::trimmed`], not here.
    pub fn changed_since(&self, revision: u64) -> Changed {
        if revision >= self.revision {
            return Changed::Nothing;
        }
        if self.revision - revision > CHANGE_LOG as u64 {
            return Changed::All;
        }
        let from = self
            .changes
            .iter()
            .filter(|(r, _)| *r > revision && *r <= self.revision)
            .map(|(_, t)| *t)
            .min();
        match from {
            Some(i64::MIN) | None => Changed::All,
            Some(t) => Changed::From(UtcMillis(t)),
        }
    }

    /// Maximum bars kept (older ones are trimmed).
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }
    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }
    /// Bars the series can hold without reallocating (it never holds more).
    pub fn capacity(&self) -> usize {
        self.bars.capacity()
    }
    pub fn len(&self) -> usize {
        self.bars.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }
    pub fn last(&self) -> Option<&Bar> {
        self.bars.last()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn trimmed(&self) -> u64 {
        self.trimmed
    }

    pub fn clear(&mut self) {
        self.bars.clear();
        self.revision += 1;
        self.note_change(i64::MIN);
    }

    /// Fold one trade into the series.
    pub fn apply_trade(&mut self, time: UtcMillis, price: Price, volume: Qty) -> TradeOutcome {
        self.apply_trade_ordered(time, price, volume, false, true)
    }

    /// Fold a trade whose time order within its bar is known, for trades that may arrive out
    /// of order (e.g. REST back-fill after newer live trades): `first` = it is the earliest
    /// trade of its bar so far (sets the open), `last` = the latest (sets the close). High,
    /// low and volume always update. A trade that starts a new bar sets everything.
    pub fn apply_trade_ordered(
        &mut self,
        time: UtcMillis,
        price: Price,
        volume: Qty,
        first: bool,
        last: bool,
    ) -> TradeOutcome {
        let bucket = time.floor_to(self.period_ms);
        self.revision += 1;
        self.note_change(bucket.0);
        match self.bars.last_mut() {
            Some(bar) if bar.time == bucket => {
                bar.apply_trade_ordered(price, volume, first, last);
                TradeOutcome::UpdatedLast
            }
            Some(bar) if bar.time > bucket => {
                match self.bars.binary_search_by_key(&bucket, |b| b.time) {
                    Ok(i) => {
                        self.bars[i].apply_trade_ordered(price, volume, first, last);
                        TradeOutcome::Late
                    }
                    Err(0) if self.bars.len() >= self.max_len => TradeOutcome::TooOld,
                    Err(i) => {
                        self.bars.insert(i, Bar::from_trade(bucket, price, volume));
                        self.trim();
                        TradeOutcome::Late
                    }
                }
            }
            _ => {
                self.bars.push(Bar::from_trade(bucket, price, volume));
                self.trim();
                TradeOutcome::NewBar
            }
        }
    }

    /// Insert or overwrite a complete bar (server history or server bar update).
    pub fn upsert(&mut self, bar: Bar) {
        debug_assert_eq!(
            bar.time,
            bar.time.floor_to(self.period_ms),
            "bar not aligned to period"
        );
        self.revision += 1;
        self.note_change(bar.time.0);
        match self.bars.last() {
            Some(last) if last.time < bar.time => self.bars.push(bar),
            None => self.bars.push(bar),
            _ => match self.bars.binary_search_by_key(&bar.time, |b| b.time) {
                Ok(i) => self.bars[i] = bar,
                Err(i) => self.bars.insert(i, bar),
            },
        }
        self.trim();
    }

    /// Index of the bar containing `time`, or where it would be inserted.
    pub fn index_at(&self, time: UtcMillis) -> Result<usize, usize> {
        let bucket = time.floor_to(self.period_ms);
        self.bars.binary_search_by_key(&bucket, |b| b.time)
    }

    /// Keep at most `max_len` bars. Trims in chunks so the O(n) shift is rare.
    fn trim(&mut self) {
        let slack = self.max_len / 4 + 1;
        if self.bars.len() > self.max_len + slack {
            let drop = self.bars.len() - self.max_len;
            self.bars.drain(..drop);
            self.trimmed += drop as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: i64 = UtcMillis::MINUTE;

    fn t(ms: i64) -> UtcMillis {
        UtcMillis(ms)
    }

    #[test]
    fn aggregates_trades_into_buckets() {
        let mut s = BarSeries::new(M, 100);
        assert_eq!(
            s.apply_trade(t(1_000), Price(100), Qty(1)),
            TradeOutcome::NewBar
        );
        assert_eq!(
            s.apply_trade(t(2_000), Price(105), Qty(2)),
            TradeOutcome::UpdatedLast
        );
        assert_eq!(
            s.apply_trade(t(3_000), Price(95), Qty(3)),
            TradeOutcome::UpdatedLast
        );
        assert_eq!(
            s.apply_trade(t(59_999), Price(101), Qty(4)),
            TradeOutcome::UpdatedLast
        );
        assert_eq!(
            s.apply_trade(t(60_000), Price(102), Qty(5)),
            TradeOutcome::NewBar
        );
        assert_eq!(s.len(), 2);
        assert_eq!(
            s.bars()[0],
            Bar {
                time: t(0),
                open: Price(100),
                high: Price(105),
                low: Price(95),
                close: Price(101),
                volume: Qty(10)
            }
        );
        assert_eq!(s.bars()[1].time, t(60_000));
    }

    #[test]
    fn late_trade_updates_old_bar_and_gaps_are_filled_in_order() {
        let mut s = BarSeries::new(M, 100);
        s.apply_trade(t(0), Price(10), Qty(1));
        s.apply_trade(t(3 * M), Price(13), Qty(1));
        assert_eq!(
            s.apply_trade(t(1_000), Price(20), Qty(1)),
            TradeOutcome::Late
        );
        assert_eq!(s.bars()[0].high, Price(20));
        assert_eq!(
            s.apply_trade(t(M + 5), Price(11), Qty(1)),
            TradeOutcome::Late
        );
        let times: Vec<_> = s.bars().iter().map(|b| b.time.0).collect();
        assert_eq!(times, vec![0, M, 3 * M]);
    }

    #[test]
    fn server_bar_overwrites_local() {
        let mut s = BarSeries::new(M, 100);
        s.apply_trade(t(0), Price(10), Qty(1));
        let server = Bar {
            time: t(0),
            open: Price(9),
            high: Price(12),
            low: Price(8),
            close: Price(10),
            volume: Qty(50),
        };
        s.upsert(server);
        assert_eq!(s.bars(), &[server]);
        // Live trades continue on top of the server bar.
        s.apply_trade(t(10), Price(13), Qty(2));
        assert_eq!(s.bars()[0].high, Price(13));
        assert_eq!(s.bars()[0].volume, Qty(52));
    }

    #[test]
    fn history_merge_out_of_order() {
        let mut s = BarSeries::new(M, 100);
        for i in [5i64, 1, 3, 2, 4, 3] {
            s.upsert(Bar::from_trade(t(i * M), Price(i), Qty(1)));
        }
        let times: Vec<_> = s.bars().iter().map(|b| b.time.0 / M).collect();
        assert_eq!(times, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn trims_to_capacity_without_reallocating() {
        let mut s = BarSeries::new(M, 8);
        let cap = s.bars.capacity();
        for i in 0..1000 {
            s.apply_trade(t(i * M), Price(i), Qty(1));
        }
        assert!(s.len() <= 8 + 8 / 4 + 1);
        assert_eq!(s.last().unwrap().time, t(999 * M));
        assert_eq!(s.bars.capacity(), cap);
        assert_eq!(s.trimmed() as usize + s.len(), 1000);
    }

    #[test]
    fn revision_bumps() {
        let mut s = BarSeries::new(M, 8);
        let r = s.revision();
        s.apply_trade(t(0), Price(1), Qty(1));
        assert!(s.revision() > r);
    }

    #[test]
    fn changes_are_reported_from_the_earliest_bar_touched() {
        let mut s = BarSeries::new(M, 1000);
        for i in 0..10 {
            s.apply_trade(t(i * M), Price(100), Qty(1));
        }
        let seen = s.revision();
        assert_eq!(s.changed_since(seen), Changed::Nothing);
        s.apply_trade(t(9 * M + 5), Price(101), Qty(1));
        assert_eq!(s.changed_since(seen), Changed::From(t(9 * M)));
        // A late trade and a history bar further back: the earliest wins.
        s.apply_trade(t(3 * M + 1), Price(99), Qty(1));
        s.upsert(Bar::from_trade(t(5 * M), Price(98), Qty(2)));
        assert_eq!(s.changed_since(seen), Changed::From(t(3 * M)));
        // More mutations than remembered, or a clear: everything.
        let old = s.revision();
        for i in 0..(CHANGE_LOG as i64 + 1) {
            s.apply_trade(t(20 * M + i), Price(100), Qty(1));
        }
        assert_eq!(s.changed_since(old), Changed::All);
        let r = s.revision();
        s.clear();
        assert_eq!(s.changed_since(r), Changed::All);
    }
}
