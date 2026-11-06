//! State shared between the feed thread (writer) and the UI thread (reader).
//!
//! Rules that keep the UI from ever stalling on the feed (decision 0001):
//! - Sessions decode and convert *outside* the lock; the lock is held only to apply one
//!   frame's worth of normalized [`MarketEvent`]s (microseconds).
//! - Every tick is applied to the bars on the feed side, so nothing is lost if the UI is
//!   slow; the UI only *samples* the latest state.
//! - The UI checks [`ChartStore::revision`] (an atomic, no lock) once per frame and uses
//!   `try_lock`, so a contended frame simply redraws the previous scene.

use std::sync::atomic::{AtomicU64, Ordering};

use lines_core::{
    BarSeries, Capabilities, ContractKey, OrderBook, Price, Product, Stitcher, UtcMillis,
};
use parking_lot::{Mutex, MutexGuard};

use crate::provider::MarketEvent;

/// History back-fill state, shown as state (docs/design.md: no spinners; missing data shows
/// as missing).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HistoryState {
    /// The provider has no server-side history (e.g. a Sierra import replay).
    Unsupported,
    /// Requested on this connection, not complete yet.
    Loading,
    Complete,
    /// Delivered, but with known holes (`gaps` ranges are missing, not filled in).
    Incomplete {
        gaps: u64,
    },
}

impl HistoryState {
    /// Short label for status display; `None` when there is nothing to say.
    pub fn label(&self) -> Option<&'static str> {
        match self {
            HistoryState::Loading => Some("loading history"),
            HistoryState::Incomplete { .. } => Some("history incomplete"),
            HistoryState::Unsupported | HistoryState::Complete => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedStatus {
    Connecting,
    LoggingOn,
    Live,
    Reconnecting { attempt: u32, reason: String },
    ReplayFinished,
    Failed(String),
    Stopped,
}

impl FeedStatus {
    pub fn label(&self) -> &str {
        match self {
            FeedStatus::Connecting => "connecting",
            FeedStatus::LoggingOn => "subscribing",
            FeedStatus::Live => "live",
            FeedStatus::Reconnecting { .. } => "reconnecting",
            FeedStatus::ReplayFinished => "replay finished",
            FeedStatus::Failed(_) => "failed",
            FeedStatus::Stopped => "stopped",
        }
    }
}

/// Monotonic counters (the UI derives rates from deltas).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedStats {
    pub frames_in: u64,
    pub bytes_in: u64,
    /// Trades + best bid/offer updates.
    pub updates_in: u64,
    pub trades_in: u64,
    pub gaps: u64,
    pub rejected: u64,
    pub corrections_ignored: u64,
    pub decode_errors: u64,
    /// Frames (batches) after which the book had bid >= ask.
    pub book_crossed: u64,
    pub reconnects: u64,
}

#[derive(Debug)]
pub struct ChartData {
    pub product: Product,
    /// The contract all of the data below belongs to (decision 0013: stored bars, depth and
    /// recordings are always per contract). `None` = unknown, e.g. a pre-v3 recording.
    pub contract: Option<ContractKey>,
    /// Bars of contracts this chart rolled away from, newest last (bounded), kept for
    /// continuous display. Never adjusted.
    pub retired: Vec<(ContractKey, BarSeries)>,
    pub capabilities: Capabilities,
    /// Provider id ("cqg", "coinbase"); for status display, not for feature switches.
    pub provider: &'static str,
    pub series: BarSeries,
    pub last_trade: Option<Price>,
    pub bid: Option<Price>,
    pub ask: Option<Price>,
    /// Aggregated depth (empty unless the provider has the `depth` capability).
    pub book: OrderBook,
    /// Stitches history snapshots and live trades (`lines_core::stitch`).
    pub stitch: Stitcher,
    pub history: HistoryState,
    /// Exchange time of the newest trade/bar seen (the replay "clock").
    pub market_time: Option<UtcMillis>,
    pub status: FeedStatus,
    pub stats: FeedStats,
}

impl ChartData {
    /// Fold normalized events into the chart state.
    pub fn apply(&mut self, events: &[MarketEvent]) {
        let mut book_touched = false;
        for e in events {
            match *e {
                MarketEvent::Ready => self.status = FeedStatus::Live,
                MarketEvent::ContractChanged(next) => self.switch_contract(next),
                MarketEvent::History { ref bars, as_of } => {
                    self.stitch.history(&mut self.series, bars, as_of);
                    if let Some(b) = bars.last() {
                        self.bump_time(b.time);
                    }
                }
                MarketEvent::HistoryComplete => {
                    self.stitch.history_complete();
                    if self.history == HistoryState::Loading {
                        self.history = HistoryState::Complete;
                    }
                }
                MarketEvent::HistoryGap { from, to } => {
                    tracing::warn!("history missing from {from} to {to}");
                    self.history = match self.history {
                        HistoryState::Incomplete { gaps } => {
                            HistoryState::Incomplete { gaps: gaps + 1 }
                        }
                        _ => HistoryState::Incomplete { gaps: 1 },
                    };
                }
                MarketEvent::Trade { time, price, qty } => {
                    self.stitch.trade(&mut self.series, time, price, qty);
                    self.last_trade = Some(price);
                    self.stats.trades_in += 1;
                    self.stats.updates_in += 1;
                    self.bump_time(time);
                }
                MarketEvent::LastPrice(p) => self.last_trade = Some(p),
                MarketEvent::Bid(p) => {
                    self.bid = Some(p);
                    self.stats.updates_in += 1;
                }
                MarketEvent::Ask(p) => {
                    self.ask = Some(p);
                    self.stats.updates_in += 1;
                }
                MarketEvent::Gap { missed } => self.stats.gaps += missed,
                MarketEvent::Rejected { count } => self.stats.rejected += count,
                MarketEvent::CorrectionsIgnored { count } => {
                    self.stats.corrections_ignored += count
                }
                MarketEvent::BookClear => {
                    self.book.clear();
                    book_touched = true;
                }
                MarketEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                } => {
                    self.book.set(side, price, qty, orders);
                    book_touched = true;
                }
            }
        }
        if book_touched {
            // A frame is one atomic depth batch: the book is consistent only here.
            self.bid = self.book.best_bid().map(|(p, _)| p);
            self.ask = self.book.best_ask().map(|(p, _)| p);
            if self.book.is_crossed() {
                self.stats.book_crossed += 1;
            }
        }
    }

    /// Display label of the active contract (`ESZ26`, `BTC/USD`), or `None` if unknown.
    pub fn contract_label(&self) -> Option<lines_core::FixedStr<24>> {
        self.contract.map(|k| k.label(&self.product))
    }

    /// A (re)connect starts: history, if the provider has it, is loading again.
    pub fn begin_connection(&mut self) {
        let expect = self.capabilities.history;
        self.stitch.begin_connection(expect);
        self.history = if expect {
            HistoryState::Loading
        } else {
            HistoryState::Unsupported
        };
    }

    /// Make `next` the active contract. The previous contract's bars are retired (kept for
    /// continuous charts), never mixed with the new contract's.
    pub fn switch_contract(&mut self, next: ContractKey) {
        if self.contract == Some(next) {
            return;
        }
        let period = self.series.period_ms();
        let cap = self.series.max_len();
        let old = std::mem::replace(&mut self.series, BarSeries::new(period, cap));
        if let Some(prev) = self.contract
            && !old.is_empty()
        {
            self.retired.push((prev, old));
            if self.retired.len() > MAX_RETIRED {
                self.retired.remove(0);
            }
        }
        self.contract = Some(next);
        self.stitch.reset();
        self.last_trade = None;
        self.bid = None;
        self.ask = None;
        self.book.clear();
    }

    fn bump_time(&mut self, t: UtcMillis) {
        if self.market_time.is_none_or(|m| t > m) {
            self.market_time = Some(t);
        }
    }
}

/// Retired contracts kept per chart (a year of quarterly rolls).
const MAX_RETIRED: usize = 4;

#[derive(Debug)]
pub struct ChartStore {
    data: Mutex<ChartData>,
    revision: AtomicU64,
}

impl ChartStore {
    pub fn new(
        product: Product,
        contract: Option<ContractKey>,
        capabilities: Capabilities,
        provider: &'static str,
        period_ms: i64,
        max_bars: usize,
    ) -> Self {
        ChartStore {
            data: Mutex::new(ChartData {
                product,
                contract,
                retired: Vec::new(),
                capabilities,
                provider,
                series: BarSeries::new(period_ms, max_bars),
                last_trade: None,
                bid: None,
                ask: None,
                book: OrderBook::default(),
                // Journal cap: ~ one busy hour of trades while history is outstanding.
                stitch: Stitcher::new(period_ms, 500_000),
                history: if capabilities.history {
                    HistoryState::Loading
                } else {
                    HistoryState::Unsupported
                },
                market_time: None,
                status: FeedStatus::Connecting,
                stats: FeedStats::default(),
            }),
            revision: AtomicU64::new(0),
        }
    }

    /// Lock-free change detector for the UI.
    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Feed side: mutate and publish.
    pub fn update<R>(&self, f: impl FnOnce(&mut ChartData) -> R) -> R {
        let r = f(&mut self.data.lock());
        self.revision.fetch_add(1, Ordering::Release);
        r
    }

    /// UI side: never blocks. `None` means the feed holds the lock right now.
    #[inline]
    pub fn try_read(&self) -> Option<MutexGuard<'_, ChartData>> {
        self.data.try_lock()
    }

    /// Blocking read, for tests and non-UI callers.
    pub fn read(&self) -> MutexGuard<'_, ChartData> {
        self.data.lock()
    }

    pub fn set_status(&self, status: FeedStatus) {
        self.update(|d| d.status = status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use lines_core::{Month, Qty};

    #[test]
    fn contract_change_retires_bars_instead_of_mixing() {
        let es = catalog::get(catalog::ES).unwrap();
        let z25 = ContractKey::monthly(es.id, 2025, Month::new(12).unwrap());
        let h26 = ContractKey::monthly(es.id, 2026, Month::new(3).unwrap());
        let store = ChartStore::new(es, Some(z25), Capabilities::default(), "test", 60_000, 100);
        let mut d = store.read();
        let trade = |p| MarketEvent::Trade {
            time: UtcMillis(0),
            price: Price(p),
            qty: Qty(1),
        };
        d.apply(&[trade(600_000), MarketEvent::Bid(Price(599_975))]);
        d.apply(&[MarketEvent::ContractChanged(z25)]);
        assert_eq!(d.series.len(), 1, "same contract: no-op");
        d.apply(&[MarketEvent::ContractChanged(h26), trade(605_000)]);
        assert_eq!(d.contract, Some(h26));
        assert_eq!(
            d.series.bars()[0].close,
            Price(605_000),
            "new contract starts clean"
        );
        assert_eq!(d.series.len(), 1);
        assert_eq!(d.bid, None);
        assert_eq!(d.retired.len(), 1);
        assert_eq!(d.retired[0].0, z25);
        assert_eq!(
            d.retired[0].1.bars()[0].close,
            Price(600_000),
            "old bars kept, unadjusted"
        );
    }
}
