//! The base series every indicator on a feed starts from: [`MarketSeries`].
//!
//! It is fed the feed's normalized events ([`Input`]) in order and keeps, per bar bucket of
//! the chart period:
//! - the stitched bar (exactly the chart's bar: the same `lines_core::stitch` rule over the
//!   same events), and
//! - exact trade sums ([`Sums`]) of the trades that arrived as trades, and
//! - their aggressor delta ([`DeltaSums`]: buying minus selling, with the bucket's running
//!   extremes, for cumulative delta candles).
//!
//! A bucket is **exact** when every trade in it arrived as a trade: the bucket lies inside a
//! run where the trade stream was complete (after the newest history snapshot's `as_of`, with
//! no gap or disconnect). Otherwise its sums come from its bar's typical price and the bucket
//! is **approximate** (decision 0017: never silently mixed; consumers carry the flag).

use std::collections::BTreeMap;

use lines_core::{Aggressor, Bar, BarSeries, Price, Print, Qty, Stitcher, UtcMillis};

/// Exact integer sums for a volume-weighted average, scaled so trades and bar typical prices
/// share one form: a trade at `p` × `v` adds `3pv` and `9p²v`; a bar adds `(H+L+C)·v` and
/// `(H+L+C)²·v`. Then VWAP = `pv3 / 3v` and the variance = `p2v9 / 9v − VWAP²`.
///
/// Prices are catalog units and volumes quantity units (decision 0002): `i128` keeps a heavy
/// session of a high-priced product far from overflow (BTC: ~5·10²⁶ per day for `p2v9`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Sums {
    pub pv3: i128,
    pub p2v9: i128,
    pub v: i128,
}

impl Sums {
    pub fn trade(price: Price, qty: Qty) -> Sums {
        let (p, v) = (price.0 as i128, qty.0 as i128);
        Sums {
            pv3: 3 * p * v,
            p2v9: 9 * p * p * v,
            v,
        }
    }

    /// A bar as its typical price `(H+L+C)/3` × volume (the approximate fallback).
    pub fn bar(b: &Bar) -> Sums {
        let tp3 = b.high.0 as i128 + b.low.0 as i128 + b.close.0 as i128;
        let v = b.volume.0 as i128;
        Sums {
            pv3: tp3 * v,
            p2v9: tp3 * tp3 * v,
            v,
        }
    }

    pub fn add(&mut self, o: &Sums) {
        self.pv3 = self.pv3.saturating_add(o.pv3);
        self.p2v9 = self.p2v9.saturating_add(o.p2v9);
        self.v = self.v.saturating_add(o.v);
    }

    /// The volume-weighted mean, in price units.
    pub fn mean(&self) -> Option<f64> {
        (self.v > 0).then(|| self.pv3 as f64 / (3.0 * self.v as f64))
    }

    /// The volume-weighted standard deviation, in price units. Exact in `i128` while the
    /// products fit (`9v²·var = p2v9·v − pv3²`), else in `f64`.
    pub fn std_dev(&self) -> Option<f64> {
        if self.v <= 0 {
            return None;
        }
        let var = match (
            self.p2v9.checked_mul(self.v),
            self.pv3.checked_mul(self.pv3),
        ) {
            (Some(a), Some(b)) => (a - b).max(0) as f64 / (9.0 * (self.v as f64).powi(2)),
            _ => {
                let m = self.pv3 as f64 / (3.0 * self.v as f64);
                (self.p2v9 as f64 / (9.0 * self.v as f64) - m * m).max(0.0)
            }
        };
        Some(var.sqrt())
    }
}

/// Aggressor delta of one bucket: buying (buyer took the offer) minus selling (seller hit
/// the bid), exact `Qty` sums; `high`/`low` are the running delta's extremes inside the
/// bucket, counted from 0 at its start in arrival order. Trades without a side count in
/// neither.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeltaSums {
    pub buy: i64,
    pub sell: i64,
    pub high: i64,
    pub low: i64,
}

impl DeltaSums {
    pub fn delta(&self) -> i64 {
        self.buy.saturating_sub(self.sell)
    }

    pub fn trade(&mut self, qty: Qty, aggressor: Option<Aggressor>) {
        match aggressor {
            Some(Aggressor::Buy) => self.buy = self.buy.saturating_add(qty.0),
            Some(Aggressor::Sell) => self.sell = self.sell.saturating_add(qty.0),
            None => return,
        }
        let d = self.delta();
        self.high = self.high.max(d);
        self.low = self.low.min(d);
    }

    /// A bar without trades (the approximate fallback): its whole volume on the side its
    /// close moved to (none for an unchanged bar), the classic bar-level tick rule.
    pub fn bar(b: &Bar) -> DeltaSums {
        let v = b.volume.0;
        let (buy, sell) = match b.close.cmp(&b.open) {
            std::cmp::Ordering::Greater => (v, 0),
            std::cmp::Ordering::Less => (0, v),
            std::cmp::Ordering::Equal => (0, 0),
        };
        let d = buy - sell;
        DeltaSums {
            buy,
            sell,
            high: d.max(0),
            low: d.min(0),
        }
    }
}

/// What the base series keeps of a bucket's trades.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct TradeSums {
    sums: Sums,
    delta: DeltaSums,
}

/// One bucket of the base series.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub time: UtcMillis,
    /// The stitched bar (the chart's bar).
    pub bar: Bar,
    /// Trade sums if exact, else the bar's typical-price sums.
    pub sums: Sums,
    /// Aggressor delta if exact, else the bar's tick-rule estimate.
    pub delta: DeltaSums,
    pub exact: bool,
}

/// A feed's normalized events, as the engine needs them (from `MarketEvent`).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// A (re)connect starts; `expect_history`: the provider sends history.
    Connect {
        expect_history: bool,
    },
    Trade {
        time: UtcMillis,
        price: Price,
        qty: Qty,
        /// Who took liquidity (`None`: the source doesn't say).
        aggressor: Option<Aggressor>,
    },
    /// A closed history snapshot (`lines_core::stitch`).
    History {
        bars: Vec<Bar>,
        as_of: Option<UtcMillis>,
    },
    HistoryComplete,
    /// A stream without history that delivers every trade from this time on: a replay's
    /// first recorded frame (the recording holds everything after it). Without it, trades
    /// count as complete only from the first trade, so the bucket that trade lands in, e.g.
    /// the session's first minute when the replay starts at the open, would be approximate
    /// and flag the whole session's VWAP. Ignored while history is expected or a run is on.
    TradesFrom(UtcMillis),
    /// Trades were dropped (the feed reported a sequence gap).
    Gap,
    /// The feed moved to another contract (a roll): start over.
    Reset,
    /// Bars from the local store's cache, before the feed connects (decision 0019): the
    /// chart's bars from the start, bar-derived until stored trades say otherwise.
    Cached(Vec<Bar>),
    /// Stored trades replayed at startup (`lines_feed::disk::orderflow`): they add to the
    /// trade sums but never to the bars (the cached bars hold them), and `covered` (inclusive
    /// ms) is complete. A live trade at or before the newest covered time is already counted.
    Stored {
        trades: Vec<Print>,
        covered: Vec<(i64, i64)>,
    },
}

/// Inclusive `[from, to]` ms ranges where the trade stream was complete.
#[derive(Clone, Debug, Default)]
struct Complete(Vec<(i64, i64)>);

impl Complete {
    fn add(&mut self, from: i64, to: i64) {
        if from > to {
            return;
        }
        let (mut lo, mut hi) = (from, to);
        self.0.retain(|&(a, b)| {
            let touch = a <= hi.saturating_add(1) && b.saturating_add(1) >= lo;
            if touch {
                lo = lo.min(a);
                hi = hi.max(b);
            }
            !touch
        });
        let at = self.0.partition_point(|&(a, _)| a < lo);
        self.0.insert(at, (lo, hi));
    }

    fn covers(&self, from: i64, to: i64) -> bool {
        self.0.iter().any(|&(a, b)| a <= from && to <= b)
    }

    fn forget_before(&mut self, t: i64) {
        self.0.retain(|&(_, b)| b >= t);
    }
}

pub struct MarketSeries {
    period_ms: i64,
    bars: BarSeries,
    stitch: Stitcher,
    seen_trimmed: u64,
    /// Exact trade sums per bucket start (trades that arrived as trades).
    trades: BTreeMap<i64, TradeSums>,
    /// The current run of complete trades starts here (`None`: no complete run now).
    run_start: Option<i64>,
    /// Waiting for history to say where trades become complete.
    awaiting_history: bool,
    /// After a gap: the next trade starts a new run.
    restart_on_trade: bool,
    newest_trade: Option<i64>,
    /// Newest stored trade time replayed (`Input::Stored`).
    stored_through: Option<i64>,
    complete: Complete,
    buckets: Vec<Bucket>,
    /// Evaluations (inputs batches applied), for tests and stats.
    pub batches: u64,
}

impl MarketSeries {
    pub fn new(period_ms: i64, max_buckets: usize) -> MarketSeries {
        MarketSeries {
            period_ms,
            bars: BarSeries::new(period_ms, max_buckets),
            stitch: Stitcher::new(period_ms, 500_000),
            seen_trimmed: 0,
            trades: BTreeMap::new(),
            run_start: None,
            awaiting_history: false,
            restart_on_trade: false,
            newest_trade: None,
            stored_through: None,
            complete: Complete::default(),
            buckets: Vec::with_capacity(max_buckets.min(1 << 16)),
            batches: 0,
        }
    }

    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }

    pub fn buckets(&self) -> &[Bucket] {
        &self.buckets
    }

    /// Buckets dropped from the front so far (the series keeps at most `max_buckets`).
    pub fn front_drops(&self) -> u64 {
        self.seen_trimmed
    }

    /// End the current run of complete trades at `to`.
    fn close_run(&mut self, to: Option<i64>) {
        if let (Some(from), Some(to)) = (self.run_start.take(), to) {
            self.complete.add(from, to);
        }
    }

    /// Apply a batch in order; returns the earliest bucket time that changed.
    pub fn apply(&mut self, inputs: &[Input]) -> Option<UtcMillis> {
        self.batches += 1;
        let p = self.period_ms;
        let mut dirty: Option<i64> = None;
        let mark = |t: i64, dirty: &mut Option<i64>| {
            let b = t.div_euclid(p) * p;
            *dirty = Some(dirty.map_or(b, |d| d.min(b)));
        };
        for input in inputs {
            match input {
                Input::Connect { expect_history } => {
                    // Trades missed while disconnected: the run (and its last bucket) ends.
                    self.close_run(self.newest_trade);
                    if let Some(t) = self.newest_trade {
                        mark(t, &mut dirty);
                    }
                    self.stitch.begin_connection(*expect_history);
                    self.awaiting_history = *expect_history;
                    self.restart_on_trade = !*expect_history;
                }
                Input::Trade {
                    time,
                    price,
                    qty,
                    aggressor,
                } => {
                    if self.restart_on_trade && !self.awaiting_history {
                        self.restart_on_trade = false;
                        self.run_start = Some(time.0);
                    }
                    if self.stitch.trade(&mut self.bars, *time, *price, *qty)
                        && self.stored_through.is_none_or(|s| time.0 > s)
                    {
                        let b = time.0.div_euclid(p) * p;
                        let t = self.trades.entry(b).or_default();
                        t.sums.add(&Sums::trade(*price, *qty));
                        t.delta.trade(*qty, *aggressor);
                        self.newest_trade =
                            Some(self.newest_trade.map_or(time.0, |n| n.max(time.0)));
                        mark(time.0, &mut dirty);
                    }
                }
                Input::History { bars, as_of } => {
                    self.stitch.history(&mut self.bars, bars, *as_of);
                    if let Some(b) = bars.first() {
                        mark(b.time.0, &mut dirty);
                    }
                    if let Some(a) = as_of {
                        // The trades before the snapshot's first bar were complete and stay
                        // so; the snapshot replaced its own buckets (now bar-derived); trades
                        // after its `as_of` are complete from here on.
                        let cut = bars.first().map_or(a.0 + 1, |b| b.time.0);
                        if let Some(start) = self.run_start
                            && start < cut
                        {
                            self.complete.add(start, cut - 1);
                        }
                        mark(a.0, &mut dirty);
                        self.run_start = Some(a.0 + 1);
                        self.awaiting_history = false;
                        self.restart_on_trade = false;
                    }
                }
                Input::HistoryComplete => self.stitch.history_complete(),
                Input::TradesFrom(t) => {
                    if self.restart_on_trade && !self.awaiting_history {
                        self.restart_on_trade = false;
                        self.run_start = Some(t.0);
                    }
                }
                Input::Gap => {
                    self.close_run(self.newest_trade.map(|t| t - 1));
                    if let Some(t) = self.newest_trade {
                        mark(t, &mut dirty);
                    }
                    self.restart_on_trade = true;
                }
                Input::Reset => {
                    *self = MarketSeries::new(self.period_ms, self.bars.max_len());
                    dirty = Some(i64::MIN);
                }
                Input::Cached(bars) => {
                    for b in bars {
                        self.bars.upsert(*b);
                    }
                    if let Some(b) = bars.first() {
                        mark(b.time.0, &mut dirty);
                    }
                }
                Input::Stored { trades, covered } => {
                    for t in trades {
                        let b = t.time.0.div_euclid(p) * p;
                        let s = self.trades.entry(b).or_default();
                        s.sums.add(&Sums::trade(t.price, t.qty));
                        s.delta.trade(t.qty, t.aggressor);
                        mark(t.time.0, &mut dirty);
                    }
                    for &(a, b) in covered {
                        self.complete.add(a, b);
                        self.stored_through = Some(self.stored_through.map_or(b, |s| s.max(b)));
                        mark(a, &mut dirty);
                    }
                }
            }
        }
        if let Some(d) = dirty {
            self.rebuild_from(d);
        }
        dirty.map(UtcMillis)
    }

    fn exact(&self, b: i64) -> bool {
        let end = b + self.period_ms - 1;
        self.run_start.is_some_and(|r| r <= b) || self.complete.covers(b, end)
    }

    /// Re-derive buckets from `from` onwards (positions before it are unchanged).
    fn rebuild_from(&mut self, from: i64) {
        let trimmed = self.bars.trimmed();
        if trimmed > self.seen_trimmed {
            let k = ((trimmed - self.seen_trimmed) as usize).min(self.buckets.len());
            self.buckets.drain(..k);
            self.seen_trimmed = trimmed;
            if let Some(first) = self.bars.bars().first() {
                let t = first.time.0;
                self.trades = self.trades.split_off(&t);
                self.complete.forget_before(t);
            }
        }
        let at = self.bars.bars().partition_point(|b| b.time.0 < from);
        self.buckets.truncate(at);
        for i in self.buckets.len()..self.bars.len() {
            let bar = self.bars.bars()[i];
            let exact = self.exact(bar.time.0);
            let (sums, delta) = match self.trades.get(&bar.time.0) {
                Some(s) if exact => (s.sums, s.delta),
                _ => (Sums::bar(&bar), DeltaSums::bar(&bar)),
            };
            self.buckets.push(Bucket {
                time: bar.time,
                bar,
                sums,
                delta,
                exact: exact && self.trades.contains_key(&bar.time.0),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(min: i64, ms: i64) -> UtcMillis {
        UtcMillis(min * 60_000 + ms)
    }

    fn trade(min: i64, ms: i64, p: i64, q: i64) -> Input {
        Input::Trade {
            time: t(min, ms),
            price: Price(p),
            qty: Qty(q),
            aggressor: None,
        }
    }

    fn side(min: i64, ms: i64, q: i64, a: Aggressor) -> Input {
        Input::Trade {
            time: t(min, ms),
            price: Price(100),
            qty: Qty(q),
            aggressor: Some(a),
        }
    }

    #[test]
    fn delta_is_exact_with_running_extremes_and_bars_are_estimates() {
        use Aggressor::{Buy, Sell};
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[
            Input::Connect {
                expect_history: false,
            },
            Input::TradesFrom(t(0, 0)),
            side(0, 1, 3, Buy),
            side(0, 2, 5, Sell),
            side(0, 3, 4, Buy),
            trade(0, 4, 100, 9), // no side: neither
            side(1, 0, 2, Sell),
        ]);
        let b = m.buckets();
        assert!(b[0].exact);
        // Running delta 3, -2, 2: high 3, low -2, close 2.
        assert_eq!(
            b[0].delta,
            DeltaSums {
                buy: 7,
                sell: 5,
                high: 3,
                low: -2
            }
        );
        assert_eq!(b[0].delta.delta(), 2);
        assert_eq!(b[1].delta.delta(), -2);
        // A bar from history: its volume on the side of its close.
        let up = Bar {
            time: UtcMillis(0),
            open: Price(1),
            high: Price(3),
            low: Price(1),
            close: Price(2),
            volume: Qty(10),
        };
        assert_eq!(DeltaSums::bar(&up).delta(), 10);
        let flat = Bar {
            close: up.open,
            ..up
        };
        assert_eq!(DeltaSums::bar(&flat), DeltaSums::default());
    }

    #[test]
    fn sums_are_exact_and_bars_are_typical_price() {
        let mut s = Sums::trade(Price(100), Qty(2));
        s.add(&Sums::trade(Price(103), Qty(1)));
        assert_eq!(s.mean(), Some(101.0));
        // Var = (2·1 + 1·4)/3 = 2.
        assert!((s.std_dev().unwrap() - 2f64.sqrt()).abs() < 1e-12);
        let b = Bar {
            time: UtcMillis(0),
            open: Price(1),
            high: Price(12),
            low: Price(6),
            close: Price(9),
            volume: Qty(4),
        };
        assert_eq!(Sums::bar(&b).mean(), Some(9.0));
        assert_eq!(Sums::default().mean(), None);
    }

    #[test]
    fn history_buckets_are_approximate_live_buckets_exact() {
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[Input::Connect {
            expect_history: true,
        }]);
        // A live trade before history arrives: not known complete yet.
        m.apply(&[trade(2, 30_000, 105, 1)]);
        assert!(!m.buckets()[0].exact);
        let hist = Bar {
            time: t(0, 0),
            open: Price(100),
            high: Price(102),
            low: Price(99),
            close: Price(101),
            volume: Qty(10),
        };
        let dirty = m.apply(&[
            Input::History {
                bars: vec![hist],
                as_of: Some(t(2, 0)),
            },
            Input::HistoryComplete,
        ]);
        assert_eq!(dirty, Some(t(0, 0)));
        // Bucket 2 started before the snapshot's as_of + 1: approximate; minute 3 is exact.
        m.apply(&[trade(3, 1, 106, 2)]);
        let b = m.buckets();
        assert_eq!(b.len(), 3);
        assert!(!b[0].exact && !b[1].exact && b[2].exact);
        assert_eq!(b[2].sums, Sums::trade(Price(106), Qty(2)));
        assert_eq!(b[0].sums, Sums::bar(&hist));
        // A gap: the bucket it happened in is no longer exact; the next full one is.
        m.apply(&[Input::Gap, trade(3, 500, 107, 1), trade(4, 0, 108, 1)]);
        let b = m.buckets();
        assert!(!b[2].exact, "gap inside minute 3");
        assert!(b[3].exact, "minute 4 is complete again");
    }

    #[test]
    fn a_stream_complete_from_its_start_is_exact_from_its_first_bucket() {
        let mut m = MarketSeries::new(60_000, 100);
        // A replay whose first frame is at the minute (e.g. the session open); its first
        // trade comes 250 ms later.
        m.apply(&[
            Input::Connect {
                expect_history: false,
            },
            Input::TradesFrom(t(0, 0)),
            trade(0, 250, 100, 2),
            trade(1, 0, 101, 1),
        ]);
        let b = m.buckets();
        assert!(b[0].exact && b[1].exact);
        assert_eq!(b[0].sums, Sums::trade(Price(100), Qty(2)));
        // A start mid-minute leaves that minute approximate (trades before it are unknown).
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[
            Input::Connect {
                expect_history: false,
            },
            Input::TradesFrom(t(0, 1)),
            trade(0, 250, 100, 2),
            trade(1, 0, 101, 1),
        ]);
        assert!(!m.buckets()[0].exact && m.buckets()[1].exact);
        // While history is expected it means nothing: the snapshot's `as_of` decides.
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[
            Input::Connect {
                expect_history: true,
            },
            Input::TradesFrom(t(0, 0)),
            trade(0, 250, 100, 2),
        ]);
        assert!(!m.buckets()[0].exact);
    }

    #[test]
    fn without_history_the_first_partial_bucket_is_approximate() {
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[
            Input::Connect {
                expect_history: false,
            },
            trade(0, 10_000, 100, 1),
            trade(1, 0, 101, 1),
        ]);
        let b = m.buckets();
        assert!(!b[0].exact && b[1].exact);
        // A reconnect: the minute in progress is cut short.
        m.apply(&[
            Input::Connect {
                expect_history: false,
            },
            trade(3, 5, 102, 1),
            trade(4, 0, 103, 1),
        ]);
        let b = m.buckets();
        assert!(!b[1].exact, "minute 1 was interrupted by the disconnect");
        assert!(!b[2].exact && b[3].exact);
    }

    /// Ten minutes of trades, two a minute, both sides.
    fn session() -> Vec<Print> {
        (0..20)
            .map(|i| Print {
                time: t(i / 2, (i % 2) * 30_000 + 5),
                price: Price(100 + (i * 7) % 5),
                qty: Qty(1 + i % 3),
                aggressor: Some(if i % 3 == 0 {
                    Aggressor::Sell
                } else {
                    Aggressor::Buy
                }),
            })
            .collect()
    }

    fn live_inputs(trades: &[Print]) -> Vec<Input> {
        let mut v = vec![
            Input::Connect {
                expect_history: false,
            },
            Input::TradesFrom(t(0, 0)),
        ];
        v.extend(trades.iter().map(|p| Input::Trade {
            time: p.time,
            price: p.price,
            qty: p.qty,
            aggressor: p.aggressor,
        }));
        v
    }

    /// Stored order flow (cached bars + stored trades, `Input::Cached` / `Input::Stored`)
    /// builds exactly the buckets the same trades build live.
    #[test]
    fn stored_trades_equal_live_trades() {
        let trades = session();
        let mut live = MarketSeries::new(60_000, 100);
        live.apply(&live_inputs(&trades));
        let bars: Vec<Bar> = live.buckets().iter().map(|b| b.bar).collect();
        let mut stored = MarketSeries::new(60_000, 100);
        stored.apply(&[
            Input::Cached(bars),
            Input::Stored {
                trades: trades.clone(),
                covered: vec![(t(0, 0).0, t(10, 0).0 - 1)],
            },
        ]);
        assert_eq!(stored.buckets(), live.buckets());
        assert!(stored.buckets().iter().all(|b| b.exact));
    }

    /// A hole in the stored coverage keeps only its buckets approximate (bar-derived).
    #[test]
    fn a_coverage_gap_keeps_only_its_buckets_approximate() {
        let trades = session();
        let mut live = MarketSeries::new(60_000, 100);
        live.apply(&live_inputs(&trades));
        let bars: Vec<Bar> = live.buckets().iter().map(|b| b.bar).collect();
        // Minutes 4 and 5 are not covered (their trades were never stored complete).
        let kept: Vec<Print> = trades
            .iter()
            .copied()
            .filter(|p| !(t(4, 0)..t(6, 0)).contains(&p.time))
            .collect();
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[
            Input::Cached(bars),
            Input::Stored {
                trades: kept,
                covered: vec![(t(0, 0).0, t(4, 0).0 - 1), (t(6, 0).0, t(10, 0).0 - 1)],
            },
        ]);
        let exact: Vec<bool> = m.buckets().iter().map(|b| b.exact).collect();
        assert_eq!(
            exact,
            [true, true, true, true, false, false, true, true, true, true]
        );
        assert_eq!(m.buckets()[4].sums, Sums::bar(&m.buckets()[4].bar));
        assert_eq!(m.buckets()[7], live.buckets()[7]);
    }

    /// After stored trades, a snapshot older than the store doesn't count them again.
    #[test]
    fn no_double_count_across_the_stored_to_live_boundary() {
        let trades = session();
        let mut live = MarketSeries::new(60_000, 100);
        live.apply(&live_inputs(&trades));
        let bars: Vec<Bar> = live.buckets().iter().map(|b| b.bar).collect();
        let (first, rest) = trades.split_at(10); // minutes 0-4 stored
        let mut m = MarketSeries::new(60_000, 100);
        m.apply(&[
            Input::Cached(bars[..5].to_vec()),
            Input::Stored {
                trades: first.to_vec(),
                covered: vec![(t(0, 0).0, t(5, 0).0 - 1)],
            },
            Input::Connect {
                expect_history: true,
            },
            // The provider's snapshot ends inside the stored span (minute 3).
            Input::History {
                bars: bars[..4].to_vec(),
                as_of: Some(t(4, 0)),
            },
        ]);
        // Minute 4's trades again (already stored), then minutes 5-9 live.
        let again: Vec<Input> = first[8..]
            .iter()
            .chain(rest)
            .map(|p| Input::Trade {
                time: p.time,
                price: p.price,
                qty: p.qty,
                aggressor: p.aggressor,
            })
            .collect();
        m.apply(&again);
        let total = |s: &MarketSeries| s.buckets().iter().map(|b| b.sums.v).sum::<i128>();
        assert_eq!(total(&m), total(&live), "every trade once");
        assert_eq!(m.buckets()[4].sums, live.buckets()[4].sums);
        assert!(m.buckets()[4].exact && m.buckets()[9].exact);
    }
}
