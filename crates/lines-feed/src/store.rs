//! State shared between the feed thread (writer) and the UI thread (reader).
//!
//! Rules that keep the UI from ever stalling on the feed (decision 0001):
//! - Sessions decode and convert *outside* the lock; the lock is held only to apply one
//!   frame's worth of normalized [`MarketEvent`]s (microseconds).
//! - Every tick is applied to the bars on the feed side, so nothing is lost if the UI is
//!   slow; the UI only *samples* the latest state.
//! - The UI checks [`ChartStore::revision`] (an atomic, no lock) once per frame and uses
//!   `try_lock`, so a contended frame simply redraws the previous scene.
//!
//! Stale (docs/design.md §7): every update stamps a monotonic "last data" time, also an
//! atomic, so panes can tell a live feed that went quiet from one that is ticking without
//! taking the lock: [`ChartStore::stale_for`].

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lines_core::{
    BarSeries, Capabilities, ContractKey, OrderBook, Price, Print, Product, SessionProfiles,
    Stitcher, Tape, UtcMillis, VolumeAtPrice,
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
    /// Book messages whose rebuilt book matched the exchange's checksum (Kraken spot).
    pub book_checksums_ok: u64,
    /// Book messages whose rebuilt book did not match: each triggers a resync.
    pub book_checksum_mismatches: u64,
    /// Fresh book snapshots requested (checksum mismatch or book sequence gap).
    pub book_resyncs: u64,
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
    /// Time & sales: the newest live prints (bounded ring).
    pub tape: Tape,
    /// Traded volume per price since data start for this contract (trading sessions are not
    /// modelled yet). Only trades the stitch counts as new, so a re-delivered trade is never
    /// counted twice.
    pub vap: VolumeAtPrice,
    /// Volume profiles per ETH/RTH session (the product's session calendar) and per bar, from
    /// the same counted trades as `vap`, split by aggressor (`lines_core::profile`).
    pub profiles: SessionProfiles,
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
                MarketEvent::Trade {
                    time,
                    price,
                    qty,
                    aggressor,
                } => {
                    if self.stitch.trade(&mut self.series, time, price, qty) {
                        self.tape.push(Print {
                            time,
                            price,
                            qty,
                            aggressor,
                        });
                        self.vap.add(price, qty);
                        self.profiles.add(time, price, qty, aggressor);
                    }
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
                MarketEvent::BookChecksumOk => self.stats.book_checksums_ok += 1,
                MarketEvent::BookChecksumMismatch => self.stats.book_checksum_mismatches += 1,
                MarketEvent::BookResync => self.stats.book_resyncs += 1,
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

    /// Bars from the local store's cache, shown before the feed connects (decision 0019).
    /// The feed then requests history from where they end and the stitch merges it.
    pub fn load_cached(&mut self, bars: &[lines_core::Bar]) {
        for b in bars {
            self.series.upsert(*b);
        }
        if let Some(b) = bars.last() {
            self.bump_time(b.time);
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
        self.tape.clear();
        self.vap.clear();
        self.profiles.clear();
    }

    fn bump_time(&mut self, t: UtcMillis) {
        if self.market_time.is_none_or(|m| t > m) {
            self.market_time = Some(t);
        }
    }
}

/// Prints kept for time & sales (a few minutes of a busy open).
pub const TAPE_PRINTS: usize = 8192;

/// Retired contracts kept per chart (a year of quarterly rolls).
const MAX_RETIRED: usize = 4;

#[derive(Debug)]
pub struct ChartStore {
    data: Mutex<ChartData>,
    revision: AtomicU64,
    /// Monotonic ms (see [`mono_ms`]) of the last update; 0 = never.
    last_data: AtomicU64,
    /// The feed is live (status `Live`).
    live: AtomicBool,
    stale_after_ms: AtomicU64,
    /// Market time (newest trade) at the last update, `i64::MIN` = none yet.
    market_ms: AtomicI64,
    /// The product's trading hours: a closed market is never stale.
    calendar: Option<&'static lines_core::sessions::SessionCalendar>,
    /// This store's feed in the indicator engine (unique per process).
    feed_id: u64,
}

static NEXT_FEED_ID: AtomicU64 = AtomicU64::new(1);

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Monotonic milliseconds since the first use (never 0).
fn mono_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64 + 1
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
        let calendar = crate::catalog::sessions(product.id);
        ChartStore {
            calendar,
            feed_id: NEXT_FEED_ID.fetch_add(1, Ordering::Relaxed),
            market_ms: AtomicI64::new(i64::MIN),
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
                tape: Tape::new(TAPE_PRINTS),
                vap: VolumeAtPrice::default(),
                profiles: SessionProfiles::new(calendar, period_ms, max_bars),
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
            last_data: AtomicU64::new(0),
            live: AtomicBool::new(false),
            stale_after_ms: AtomicU64::new(5_000),
        }
    }

    /// Identifies this store's feed to the indicator engine (`crate::indicators`).
    pub fn feed_id(&self) -> u64 {
        self.feed_id
    }

    /// How long a live feed may be silent before it is stale.
    pub fn set_stale_after(&self, d: Duration) {
        self.stale_after_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// `Some(time since the last data)` when the feed is live, its market open (per the
    /// product's sessions calendar), and nothing
    /// has arrived for longer than its expected cadence; `None` otherwise. Lock-free.
    pub fn stale_for(&self) -> Option<Duration> {
        if !self.live.load(Ordering::Acquire) {
            return None;
        }
        let last = self.last_data.load(Ordering::Acquire);
        let quiet = mono_ms().saturating_sub(last);
        if last == 0 || quiet <= self.stale_after_ms.load(Ordering::Relaxed) {
            return None;
        }
        // Is the market open *now*, on the feed's own clock: the last market time plus the
        // silence (a replay keeps its recorded time; live, that is about the wall clock).
        let market = self.market_ms.load(Ordering::Acquire);
        let now = if market == i64::MIN {
            crate::now_utc()
        } else {
            UtcMillis(market.saturating_add(quiet as i64))
        };
        if self.calendar.is_some_and(|c| !c.is_open(now)) {
            return None;
        }
        Some(Duration::from_millis(quiet))
    }

    /// Lock-free change detector for the UI.
    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Feed side: mutate and publish.
    pub fn update<R>(&self, f: impl FnOnce(&mut ChartData) -> R) -> R {
        let mut d = self.data.lock();
        let r = f(&mut d);
        let live = d.status == FeedStatus::Live;
        let market = d.market_time.map_or(i64::MIN, |t| t.0);
        drop(d);
        self.market_ms.store(market, Ordering::Release);
        self.last_data.store(mono_ms(), Ordering::Release);
        self.live.store(live, Ordering::Release);
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

impl Drop for ChartStore {
    fn drop(&mut self) {
        crate::indicators::store_dropped(self.feed_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use lines_core::{Month, Qty};

    /// docs/design.md §7: silence while the market is closed is not staleness.
    #[test]
    fn quiet_is_stale_only_while_the_market_is_open() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        store.set_stale_after(Duration::ZERO);
        let at = |iso: &str| UtcMillis::parse_iso(iso).unwrap();
        let trade_at = |t: UtcMillis| MarketEvent::Trade {
            time: t,
            price: Price(600_000),
            qty: Qty(1),
            aggressor: None,
        };
        // Saturday: CME closed.
        store.update(|d| {
            d.status = FeedStatus::Live;
            d.apply(&[trade_at(at("2026-11-07T15:00:00Z"))]);
        });
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(store.stale_for(), None, "closed: not stale");
        // Wednesday mid-session: open.
        store.update(|d| d.apply(&[trade_at(at("2026-11-11T15:00:00Z"))]));
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.stale_for().is_some(), "open and quiet: stale");
    }

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
            aggressor: None,
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

    #[test]
    fn trades_feed_the_session_profiles_and_a_roll_clears_them() {
        use lines_core::{Aggressor, SessionKind};
        let es = catalog::get(catalog::ES).unwrap();
        let z26 = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        let h27 = ContractKey::monthly(es.id, 2027, Month::new(3).unwrap());
        let store = ChartStore::new(es, Some(z26), Capabilities::default(), "test", 60_000, 100);
        let mut d = store.read();
        let trade = |at: &str, p, q, aggressor| MarketEvent::Trade {
            time: UtcMillis::parse_iso(at).unwrap(),
            price: Price(p),
            qty: Qty(q),
            aggressor,
        };
        // 13:00 UTC = 07:00 CT (ETH only); 15:00 UTC = 09:00 CT (RTH too).
        d.apply(&[
            trade("2026-11-05T13:00:00", 600_000, 2, Some(Aggressor::Sell)),
            trade("2026-11-05T15:00:00", 600_025, 3, Some(Aggressor::Buy)),
        ]);
        let eth = &d.profiles.sessions(SessionKind::Eth)[0];
        assert_eq!(eth.volume.total(), Qty(5));
        assert_eq!(eth.volume.get(Price(600_000)).sell, Qty(2));
        assert_eq!(
            d.profiles.sessions(SessionKind::Rth)[0].volume.total(),
            Qty(3)
        );
        assert_eq!(d.vap.total(), Qty(5));
        d.apply(&[MarketEvent::ContractChanged(h27)]);
        assert!(d.profiles.sessions(SessionKind::Eth).is_empty());
        assert_eq!(d.profiles.data_start(), None);
    }
}
