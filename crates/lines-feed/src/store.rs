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

use lines_core::{BarSeries, Capabilities, Instrument, Price, UtcMillis};
use parking_lot::{Mutex, MutexGuard};

use crate::provider::MarketEvent;

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
    pub reconnects: u64,
}

#[derive(Debug)]
pub struct ChartData {
    pub instrument: Instrument,
    pub capabilities: Capabilities,
    /// Provider id ("cqg", "coinbase"); for status display, not for feature switches.
    pub provider: &'static str,
    pub series: BarSeries,
    pub last_trade: Option<Price>,
    pub bid: Option<Price>,
    pub ask: Option<Price>,
    /// Exchange time of the newest trade/bar seen (the replay "clock").
    pub market_time: Option<UtcMillis>,
    pub status: FeedStatus,
    pub stats: FeedStats,
}

impl ChartData {
    /// Fold normalized events into the chart state.
    pub fn apply(&mut self, events: &[MarketEvent]) {
        for e in events {
            match *e {
                MarketEvent::Ready => self.status = FeedStatus::Live,
                MarketEvent::ContractChanged => {
                    self.series.clear();
                    self.last_trade = None;
                    self.bid = None;
                    self.ask = None;
                }
                MarketEvent::Bar(bar) => {
                    self.series.upsert(bar);
                    self.bump_time(bar.time);
                }
                MarketEvent::Trade { time, price, qty } => {
                    self.series.apply_trade(time, price, qty);
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
            }
        }
    }

    fn bump_time(&mut self, t: UtcMillis) {
        if self.market_time.is_none_or(|m| t > m) {
            self.market_time = Some(t);
        }
    }
}

#[derive(Debug)]
pub struct ChartStore {
    data: Mutex<ChartData>,
    revision: AtomicU64,
}

impl ChartStore {
    pub fn new(
        instrument: Instrument,
        capabilities: Capabilities,
        provider: &'static str,
        period_ms: i64,
        max_bars: usize,
    ) -> Self {
        ChartStore {
            data: Mutex::new(ChartData {
                instrument,
                capabilities,
                provider,
                series: BarSeries::new(period_ms, max_bars),
                last_trade: None,
                bid: None,
                ask: None,
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
