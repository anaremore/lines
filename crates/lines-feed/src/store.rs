//! State shared between the feed thread (writer) and the UI thread (reader).
//!
//! Rules that keep the UI from ever stalling on the feed (decision 0001):
//! - Sessions decode and convert *outside* the lock; the lock is held only to apply one
//!   frame's worth of normalized [`MarketEvent`]s (microseconds).
//! - Every tick is applied to the bars on the feed side, so nothing is lost if the UI is
//!   slow; the UI only *samples* the latest state.
//! - The UI checks [`ChartStore::revision`] (an atomic, no lock) once per frame and uses
//!   `try_lock`, so a contended frame simply redraws the previous scene.
//! - [`ChartStore::chart_revision`] moves on everything *except* depth that leaves the best
//!   bid and offer alone: charts and time & sales follow it, so a busy book (hundreds of depth
//!   batches a second) repaints only the DOM.
//!
//! Stale (docs/design.md §7): every update stamps a monotonic "last data" time, also an
//! atomic, so panes can tell a live feed that went quiet from one that is ticking without
//! taking the lock: [`ChartStore::stale_for`].

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lines_core::latency::{ClockOffset, LatencyWindow};
use lines_core::sessions::SessionKind;
use lines_core::{
    BarSeries, Capabilities, ContractKey, OrderBook, Price, Print, Product, SessionProfiles,
    Stitcher, Tape, UtcMillis, VolumeAtPrice,
};
use parking_lot::{Mutex, MutexGuard};

use crate::provider::MarketEvent;
use crate::ticks::{TickSet, TickSource};

/// Inner holes in the counted trades shorter than this (open-market time) are not reported:
/// footprint bars are a minute wide, and a session's first trade lands just after its open.
pub const MIN_GAP_MS: i64 = UtcMillis::MINUTE;

/// Holes in the counted trades over a span ([`ChartData::counted_gaps`]).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CountedGaps {
    /// Some counted trades fall in the span.
    pub any: bool,
    /// The span starts before the counted trades: they start here (`partial from`).
    pub data_from: Option<i64>,
    /// The first hole inside the span, inclusive ms.
    pub first_gap: Option<(i64, i64)>,
    /// Holes after the first.
    pub more_gaps: u32,
}

/// What the startup replay of stored order flow took (`ChartData::apply_stored`): a tick-bar
/// rebuild (`crate::ticks`) reads exactly these stored trades again, in the same order.
#[derive(Clone, Debug, Default)]
pub(crate) struct StoredReplay {
    /// The contract's store directory they came from (set by the feed's warm start).
    pub dir: Option<crate::disk::ContractDir>,
    /// The replayed ranges: each trade date's covered part, as `disk::orderflow` gave them.
    pub covered: crate::disk::Ranges,
    /// Trades replayed.
    pub trades: u64,
}

/// Exchange → receive latency of this feed's trades and the clock-offset bound (roadmap 6b),
/// recorded only while the latency HUD is shown (`lines_core::latency::enabled`).
#[derive(Debug)]
pub struct FeedLatency {
    /// Receive time − the trade's exchange time.
    pub exch_recv: LatencyWindow,
    /// `min(receive − exchange)`: bounds the local clock's lead over the exchange's.
    pub offset: ClockOffset,
    /// Receive times are this machine's wall clock (live). A replay's are the recording's,
    /// which describe the recording machine's clock, not this one.
    pub wall_clock: bool,
    /// The newest receive time (ms): the windows' "now" for display.
    pub at_ms: i64,
}

/// A trade received more than this after its exchange time is back-fill, not latency.
pub const BACKFILL_AFTER_MS: i64 = 60_000;

impl FeedLatency {
    fn new() -> Self {
        FeedLatency {
            exch_recv: LatencyWindow::new(),
            offset: ClockOffset::default(),
            wall_clock: false,
            at_ms: i64::MIN,
        }
    }
}

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
    /// Why this feed has no depth, from the source ([`MarketEvent::DepthUnavailable`]).
    pub depth_note: Option<String>,
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
    /// The underlying's option chains (decision 0028), from `MarketEvent::OptionQuote`.
    /// Their own revision tells panes what changed; they never move `chart_epoch`.
    pub chains: lines_core::OptionChains,
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
    /// Moves when [`ChartData::apply`] changed anything but depth below the touch (trades,
    /// bars, quotes, status, counters shown in headers). See [`ChartStore::chart_revision`].
    pub chart_epoch: u64,
    /// Feed latency (roadmap 6b): `None` until the latency HUD is first shown.
    pub latency: Option<Box<FeedLatency>>,
    /// Stored order flow is being replayed into `tape`, `vap` and `profiles` (feed startup,
    /// `disk::orderflow`): panes show bars first and say so.
    pub orderflow_loading: bool,
    /// Where `vap`, `profiles` and `tape` hold every trade: the stored ranges replayed at
    /// startup and the closed live runs (see [`ChartData::counted_ranges`]).
    counted: crate::disk::Ranges,
    /// The current live run of counted trades, `[start, newest]` (closed at a reconnect or
    /// gap).
    live_run: Option<(i64, i64)>,
    /// Newest stored trade time replayed: a live trade at or before it is already counted.
    pub(crate) stored_through: Option<i64>,
    /// Whether each `Trade` of the last applied frame was counted (in order): the local
    /// store's writer stores exactly those (`disk::writer`).
    pub frame_counted: Vec<bool>,
    /// How many trades `tape`, `vap` and `profiles` took for this contract (stored ones
    /// replayed at startup, then live ones): the tape holds them all while this is at most its
    /// capacity.
    pub counted_n: u64,
    /// What the startup replay took from the local store (`apply_stored`), so a tick-bar
    /// rebuild reads exactly the same stored trades again (`crate::ticks`).
    pub(crate) stored: StoredReplay,
    /// Tick bars the charts on this store asked for (`crate::ticks`), fed the same counted
    /// trades (stored and live).
    pub ticks: TickSet,
    /// Where tick bars are rebuilt from when a chart switches to one mid-feed (the replay's
    /// file, or the local store), if anywhere.
    pub tick_source: Option<TickSource>,
    /// How many times a replay jumped back on this store (`ChartData::rewind`): panes that
    /// keep their own copy of what the store showed (the DOM trail) start over when it moves.
    pub rewinds: u64,
}

impl ChartData {
    /// Empty state for one feed (a store's, or a rebuild's scratch copy: `crate::ticks`).
    pub fn new(
        product: Product,
        contract: Option<ContractKey>,
        capabilities: Capabilities,
        provider: &'static str,
        period_ms: i64,
        max_bars: usize,
    ) -> ChartData {
        let calendar = crate::catalog::sessions(product.id);
        ChartData {
            product,
            contract,
            retired: Vec::new(),
            capabilities,
            depth_note: None,
            provider,
            series: BarSeries::new(period_ms, max_bars),
            last_trade: None,
            bid: None,
            ask: None,
            book: OrderBook::default(),
            tape: Tape::new(TAPE_PRINTS),
            chains: lines_core::OptionChains::default(),
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
            chart_epoch: 0,
            latency: None,
            orderflow_loading: false,
            counted: crate::disk::Ranges::default(),
            live_run: None,
            stored_through: None,
            frame_counted: Vec::new(),
            counted_n: 0,
            stored: StoredReplay::default(),
            ticks: TickSet::default(),
            tick_source: None,
            rewinds: 0,
        }
    }

    /// Record exchange → receive latency for this frame's trades, received at `recv`
    /// (`wall_clock`: this machine's clock, not a recording's). Back-fill (trades received
    /// more than [`BACKFILL_AFTER_MS`] late) is not latency and is skipped.
    pub fn record_latency(&mut self, events: &[MarketEvent], recv: UtcMillis, wall_clock: bool) {
        let l = self
            .latency
            .get_or_insert_with(|| Box::new(FeedLatency::new()));
        l.wall_clock = wall_clock;
        l.at_ms = l.at_ms.max(recv.0);
        for e in events {
            if let MarketEvent::Trade { time, .. } = e {
                let d = recv.0.saturating_sub(time.0);
                if d > BACKFILL_AFTER_MS {
                    continue;
                }
                // A negative transit is the clock's offset, not latency: the offset shows it.
                l.exch_recv.record(recv.0, d.max(0) as u64 * 1000);
                l.offset.record(recv.0, time.0);
            }
        }
    }

    /// Fold normalized events into the chart state.
    pub fn apply(&mut self, events: &[MarketEvent]) {
        let mut book_touched = false;
        // Anything a chart or time & sales shows changed (not only depth).
        let mut chart = false;
        // Quotes count only if they changed (compared after the batch): a feed that repeats
        // its BBO with every depth batch doesn't repaint charts.
        let quote = (self.bid, self.ask, self.last_trade);
        self.frame_counted.clear();
        for e in events {
            chart |= !matches!(
                e,
                MarketEvent::BookClear
                    | MarketEvent::BookLevel { .. }
                    | MarketEvent::BookChecksumOk
                    | MarketEvent::BookChecksumMismatch
                    | MarketEvent::BookResync
                    | MarketEvent::Bid(_)
                    | MarketEvent::Ask(_)
                    | MarketEvent::LastPrice(_)
                    // The chains have their own revision: charts don't repaint for them.
                    | MarketEvent::OptionQuote { .. }
            );
            match *e {
                MarketEvent::Ready => self.status = FeedStatus::Live,
                MarketEvent::ContractChanged(next) => self.switch_contract(next),
                MarketEvent::History { ref bars, as_of } => {
                    self.stitch.history(&mut self.series, bars, as_of);
                    if let Some(b) = bars.last() {
                        self.bump_time(b.time);
                    }
                    // Trades after the snapshot are counted from here on (the stitch drops
                    // the rest as already in its bars).
                    if let Some(a) = as_of {
                        self.close_run();
                        self.live_run = Some((a.0 + 1, a.0));
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
                    let counted = self.stitch.trade(&mut self.series, time, price, qty)
                        // Replayed from the store already (a snapshot older than the store).
                        && self.stored_through.is_none_or(|s| time.0 > s);
                    self.frame_counted.push(counted);
                    if counted {
                        self.tape.push(Print {
                            time,
                            price,
                            qty,
                            aggressor,
                        });
                        self.vap.add(price, qty);
                        self.profiles.add(time, price, qty, aggressor);
                        self.live_run = Some(match self.live_run {
                            Some((a, b)) => (a, b.max(time.0)),
                            None => (time.0, time.0),
                        });
                        self.counted_n += 1;
                        self.ticks.push(Print {
                            time,
                            price,
                            qty,
                            aggressor,
                        });
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
                MarketEvent::Gap { missed } => {
                    self.stats.gaps += missed;
                    self.close_run();
                }
                MarketEvent::BookChecksumOk => self.stats.book_checksums_ok += 1,
                MarketEvent::BookChecksumMismatch => self.stats.book_checksum_mismatches += 1,
                MarketEvent::BookResync => self.stats.book_resyncs += 1,
                MarketEvent::Rejected { count } => self.stats.rejected += count,
                MarketEvent::CorrectionsIgnored { count } => {
                    self.stats.corrections_ignored += count
                }
                MarketEvent::OptionQuote { key, quote } => {
                    // Not counted as feed updates (the underlying's stats); a quote past the
                    // chains' bounds is counted as rejected.
                    if !self.chains.apply(key, quote) {
                        self.stats.rejected += 1;
                    }
                }
                MarketEvent::BookClear => {
                    self.book.clear();
                    book_touched = true;
                }
                MarketEvent::DepthUnavailable { ref reason } => {
                    self.capabilities.depth = false;
                    self.depth_note = Some(reason.clone());
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
        if chart || (self.bid, self.ask, self.last_trade) != quote {
            self.chart_epoch += 1;
        }
    }

    /// Stored order flow (`disk::orderflow`, feed startup): `trades` add to the tape, VAP and
    /// profiles exactly as counted live trades do, but never to the bars (the cached bars
    /// already hold them). `covered`: where `trades` is every trade.
    /// Tick-bar builders take them too, as counted trades (`crate::ticks`).
    pub fn apply_stored(&mut self, trades: &[Print], covered: &[(i64, i64)]) {
        for p in trades {
            self.tape.push(*p);
            self.vap.add(p.price, p.qty);
            self.profiles.add(p.time, p.price, p.qty, p.aggressor);
            self.counted_n += 1;
            self.ticks.push(*p);
            // The newest replayed trade (exchange time; never the local clock).
            self.stored_through = Some(self.stored_through.map_or(p.time.0, |s| s.max(p.time.0)));
        }
        self.stored.trades += trades.len() as u64;
        for &(a, b) in covered {
            self.counted.add(a, b);
            self.stored.covered.add(a, b);
        }
        if !trades.is_empty() {
            self.chart_epoch += 1;
        }
    }

    /// Where `vap`, `profiles` and `tape` hold every trade: stored ranges and live runs
    /// (inclusive ms). A profile or footprint over a span outside these is partial.
    pub fn counted_ranges(&self) -> crate::disk::Ranges {
        let mut r = self.counted.clone();
        if let Some((a, b)) = self.live_run {
            r.add(a, b);
        }
        r
    }

    /// Where the counted trades leave holes inside `[from, to]` (inclusive ms), without
    /// allocating (profiles and footprints ask every frame). A hole after the newest counted
    /// trade is not reported (those trades simply haven't happened or arrived yet). An inner
    /// hole counts only where the market was open (the product's sessions: a daily break is
    /// not missing data) and only from [`MIN_GAP_MS`] of open time.
    pub fn counted_gaps(&self, from: i64, to: i64) -> CountedGaps {
        let mut g = CountedGaps::default();
        if from > to {
            return g;
        }
        let stored = self.counted.ranges();
        let mut live = self.live_run;
        let mut i = 0;
        let mut cursor = from;
        let mut any = false;
        // The stored ranges are sorted and disjoint; the live run merges in by its start.
        let mut block: Option<(i64, i64)> = None;
        loop {
            let next = match (stored.get(i), live) {
                (Some(&s), Some(l)) if l.0 < s.0 => {
                    live = None;
                    Some(l)
                }
                (Some(&s), _) => {
                    i += 1;
                    Some(s)
                }
                (None, Some(l)) => {
                    live = None;
                    Some(l)
                }
                (None, None) => None,
            };
            // Merge touching ranges into one block before looking for a hole.
            match (block, next) {
                (Some((a, b)), Some((c, d))) if c <= b.saturating_add(1) => {
                    block = Some((a, b.max(d)));
                    continue;
                }
                _ => {}
            }
            if let Some((a, b)) = block
                && b >= from
            {
                if a > to {
                    break;
                }
                if a > cursor {
                    if cursor == from && !any {
                        g.data_from = Some(a);
                    } else if let Some(hole) = self.open_part(cursor, a - 1) {
                        if g.first_gap.is_none() {
                            g.first_gap = Some(hole);
                        } else {
                            g.more_gaps += 1;
                        }
                    }
                }
                any = true;
                cursor = cursor.max(b.saturating_add(1));
            }
            block = next;
            if block.is_none() {
                break;
            }
        }
        g.any = any;
        g
    }

    /// The part of the hole `[a, b]` when the market was open, if at least [`MIN_GAP_MS`]
    /// (the whole hole when the product has no session calendar).
    fn open_part(&self, a: i64, b: i64) -> Option<(i64, i64)> {
        let Some(cal) = self.profiles.calendar() else {
            return (b - a + 1 >= MIN_GAP_MS).then_some((a, b));
        };
        let (mut first, mut last, mut open) = (None, None, 0i64);
        for s in cal.sessions(
            UtcMillis(a),
            UtcMillis(b.saturating_add(1)),
            SessionKind::Eth,
        ) {
            let (x, y) = (s.open.0.max(a), (s.close.0 - 1).min(b));
            if x <= y {
                first.get_or_insert(x);
                last = Some(y);
                open += y - x + 1;
            }
        }
        (open >= MIN_GAP_MS).then_some((first?, last?))
    }

    /// End the live run of counted trades (a reconnect or gap: trades may be missing).
    fn close_run(&mut self) {
        if let Some((a, b)) = self.live_run.take() {
            self.counted.add(a, b);
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
        self.close_run();
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
        self.counted = crate::disk::Ranges::default();
        self.live_run = None;
        self.stored_through = None;
        self.orderflow_loading = false;
        self.counted_n = 0;
        self.stored = StoredReplay::default();
        self.ticks.clear();
    }

    /// A replay jumped back (`crate::replay_control`): everything the file delivered goes,
    /// as at a roll but with nothing retired, and the file is delivered again from its start.
    /// Tick-bar requests stay (their bars are rebuilt); status, counters and the tick source
    /// stay.
    pub fn rewind(&mut self) {
        let mut fresh = ChartData::new(
            self.product.clone(),
            self.contract,
            self.capabilities,
            self.provider,
            self.series.period_ms(),
            self.series.max_len(),
        );
        fresh.retired = std::mem::take(&mut self.retired);
        fresh.depth_note = self.depth_note.take();
        fresh.status = self.status.clone();
        fresh.stats = std::mem::take(&mut self.stats);
        fresh.latency = self.latency.take();
        fresh.tick_source = self.tick_source.take();
        fresh.ticks = std::mem::take(&mut self.ticks);
        fresh.ticks.clear();
        fresh.chart_epoch = self.chart_epoch.wrapping_add(1);
        fresh.rewinds = self.rewinds + 1;
        // The series and the tape go on, emptied, not replaced: their revision and print count
        // keep rising, so a chart's derived series (another timeframe) sees a change and
        // rebuilds, and a counter of prints never mistakes the new ones for seen.
        std::mem::swap(&mut fresh.series, &mut self.series);
        fresh.series.clear();
        std::mem::swap(&mut fresh.tape, &mut self.tape);
        fresh.tape.clear();
        *self = fresh;
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
    /// Like `revision`, but not moved by depth that left the best bid/offer alone.
    chart_revision: AtomicU64,
    /// Monotonic ms (see [`mono_ms`]) of the last update; 0 = never.
    last_data: AtomicU64,
    /// Monotonic µs (see [`mono_us`]) of the last update that moved `chart_revision`; 0 =
    /// never. For receive → paint (a depth frame below the touch doesn't reset it).
    last_chart_us: AtomicU64,
    /// The feed is live (status `Live`).
    live: AtomicBool,
    stale_after_ms: AtomicU64,
    /// Market time (newest trade) at the last update, `i64::MIN` = none yet.
    market_ms: AtomicI64,
    /// The newest feed frame's own time (the replay clock, or the wall clock live),
    /// `i64::MIN` = none yet. See [`ChartStore::feed_clock`].
    frame_ms: AtomicI64,
    /// The product's trading hours: a closed market is never stale.
    calendar: Option<&'static lines_core::sessions::SessionCalendar>,
    /// This store's feed in the indicator engine (unique per process).
    feed_id: u64,
    /// Start / stop recording this feed (`live_record`).
    pub recording: crate::live_record::RecordCtl,
    /// A replay the bar controls: its clock is this store's market clock.
    replay: std::sync::OnceLock<std::sync::Arc<crate::replay_control::ReplayControl>>,
}

static NEXT_FEED_ID: AtomicU64 = AtomicU64::new(1);

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Monotonic milliseconds since the first use (never 0).
fn mono_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64 + 1
}

/// Monotonic microseconds on the same epoch as [`mono_ms`] (never 0).
pub fn mono_us() -> u64 {
    EPOCH.elapsed().as_micros() as u64 + 1
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
            recording: Default::default(),
            market_ms: AtomicI64::new(i64::MIN),
            frame_ms: AtomicI64::new(i64::MIN),
            data: Mutex::new(ChartData::new(
                product,
                contract,
                capabilities,
                provider,
                period_ms,
                max_bars,
            )),
            revision: AtomicU64::new(0),
            chart_revision: AtomicU64::new(0),
            last_data: AtomicU64::new(0),
            last_chart_us: AtomicU64::new(0),
            live: AtomicBool::new(false),
            stale_after_ms: AtomicU64::new(5_000),
            replay: std::sync::OnceLock::new(),
        }
    }

    /// The replay control this store's feed plays by (set once, when the feed starts).
    pub fn set_replay(&self, control: std::sync::Arc<crate::replay_control::ReplayControl>) {
        let _ = self.replay.set(control);
    }

    /// The replay control this store's feed plays by, if the replay bar drives it.
    pub fn replay(&self) -> Option<&std::sync::Arc<crate::replay_control::ReplayControl>> {
        self.replay.get()
    }

    /// Feed side: a replay jumped back. The data goes ([`ChartData::rewind`]) and so do the
    /// clocks, which otherwise never go back.
    pub fn rewind(&self) {
        self.update(|d| d.rewind());
        self.market_ms.store(i64::MIN, Ordering::Release);
        self.frame_ms.store(i64::MIN, Ordering::Release);
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
        let last = self.last_data.load(Ordering::Acquire);
        if last == 0 {
            return None;
        }
        // A replay's silence is on its own clock: it doesn't grow while paused.
        if let Some(c) = self.replay.get() {
            let frame = self
                .frame_ms
                .load(Ordering::Acquire)
                .max(self.market_ms.load(Ordering::Acquire));
            let quiet = match c.clock() {
                Some(t) if frame != i64::MIN => t.0.saturating_sub(frame).max(0) as u64,
                _ => 0,
            };
            return self.stale_after_quiet(Duration::from_millis(quiet));
        }
        self.stale_after_quiet(Duration::from_millis(mono_ms().saturating_sub(last)))
    }

    /// [`Self::stale_for`] if the feed has been silent for `quiet` (no data since then): the
    /// decision without the clock, so it is tested deterministically.
    pub fn stale_after_quiet(&self, quiet: Duration) -> Option<Duration> {
        if !self.live.load(Ordering::Acquire) || self.last_data.load(Ordering::Acquire) == 0 {
            return None;
        }
        let quiet = quiet.as_millis() as u64;
        if quiet <= self.stale_after_ms.load(Ordering::Relaxed) {
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

    /// Feed side: the time of the frame being applied (the replay clock, or the wall clock
    /// live). Lock-free; call before applying the frame.
    pub fn set_frame_time(&self, t: UtcMillis) {
        self.frame_ms.fetch_max(t.0, Ordering::AcqRel);
    }

    /// The feed's own clock (lock-free), for things that animate on market time (the DOM
    /// trail): the newest of the last frame's time and the market time, plus the silence
    /// since the last update while the feed is live. Frozen when it isn't (a finished replay,
    /// screenshots), so replays stay deterministic. `None` before any data.
    pub fn feed_clock(&self) -> Option<UtcMillis> {
        if let Some(c) = self.replay.get() {
            return c.clock();
        }
        let t = self
            .frame_ms
            .load(Ordering::Acquire)
            .max(self.market_ms.load(Ordering::Acquire));
        if t == i64::MIN {
            return None;
        }
        if !self.live.load(Ordering::Acquire) {
            return Some(UtcMillis(t));
        }
        let last = self.last_data.load(Ordering::Acquire);
        let quiet = if last == 0 {
            0
        } else {
            mono_ms().saturating_sub(last)
        };
        Some(UtcMillis(t.saturating_add(quiet as i64)))
    }

    /// Microseconds since the last update that moved [`Self::chart_revision`] was published
    /// (receive → paint for a chart that paints it now), lock-free; `None` before any.
    pub fn chart_data_age_us(&self) -> Option<u64> {
        match self.last_chart_us.load(Ordering::Acquire) {
            0 => None,
            t => Some(mono_us().saturating_sub(t)),
        }
    }

    /// The market clock as the trading core's market clock runs it (replays): the newest
    /// frame or market time plus the silence since the last update, live or not, so a
    /// finished replay's clock keeps running like the core's. `None` before any data.
    pub fn market_clock(&self) -> Option<UtcMillis> {
        // The replay bar's clock: it stands still while paused.
        if let Some(c) = self.replay.get() {
            return c.clock();
        }
        let last = self.last_data.load(Ordering::Acquire);
        let quiet = if last == 0 {
            0
        } else {
            mono_ms().saturating_sub(last)
        };
        self.market_clock_after_quiet(Duration::from_millis(quiet))
    }

    /// [`ChartStore::market_clock`] after `quiet` without data (the decision, clock injected).
    pub fn market_clock_after_quiet(&self, quiet: Duration) -> Option<UtcMillis> {
        let t = self
            .frame_ms
            .load(Ordering::Acquire)
            .max(self.market_ms.load(Ordering::Acquire));
        (t != i64::MIN)
            .then(|| UtcMillis(t.saturating_add(quiet.as_millis().min(i64::MAX as u128) as i64)))
    }

    /// Lock-free change detector for the UI.
    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Lock-free change detector for panes that don't show depth (charts, time & sales):
    /// moves on every change except depth below the touch.
    #[inline]
    pub fn chart_revision(&self) -> u64 {
        self.chart_revision.load(Ordering::Acquire)
    }

    /// Feed side: mutate and publish (both revisions move).
    pub fn update<R>(&self, f: impl FnOnce(&mut ChartData) -> R) -> R {
        self.publish(f, true)
    }

    /// Derived data changed (tick bars rebuilt on a helper thread, the warm start's end): both
    /// revisions move so panes repaint, but nothing arrived from the feed, so the feed's clocks
    /// ([`Self::market_clock`], [`Self::feed_clock`]) and its staleness keep counting from the
    /// last real data (a landing rebuild must never move the trading clock back).
    pub fn update_derived<R>(&self, f: impl FnOnce(&mut ChartData) -> R) -> R {
        let r = f(&mut self.data.lock());
        self.chart_revision.fetch_add(1, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Release);
        r
    }

    /// When the last feed data was published (`mono_ms`), for tests of the clocks.
    #[cfg(test)]
    pub(crate) fn last_data_ms(&self) -> u64 {
        self.last_data.load(Ordering::Acquire)
    }

    /// Feed side: apply one feed frame. The chart revision moves only if the frame changed
    /// more than depth below the touch ([`ChartData::chart_epoch`]).
    pub fn update_frame<R>(&self, f: impl FnOnce(&mut ChartData) -> R) -> R {
        self.publish(f, false)
    }

    fn publish<R>(&self, f: impl FnOnce(&mut ChartData) -> R, chart: bool) -> R {
        let mut d = self.data.lock();
        let epoch = d.chart_epoch;
        let r = f(&mut d);
        let chart = chart || d.chart_epoch != epoch;
        let live = d.status == FeedStatus::Live;
        let market = d.market_time.map_or(i64::MIN, |t| t.0);
        drop(d);
        self.market_ms.store(market, Ordering::Release);
        self.last_data.store(mono_ms(), Ordering::Release);
        self.live.store(live, Ordering::Release);
        if chart {
            self.last_chart_us.store(mono_us(), Ordering::Release);
            self.chart_revision.fetch_add(1, Ordering::Release);
        }
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
    use lines_core::{Aggressor, Month, Qty, Side};

    fn es_store(history: bool) -> ChartStore {
        let es = catalog::get(catalog::ES).unwrap();
        let caps = Capabilities {
            history,
            ..Capabilities::default()
        };
        ChartStore::new(es, None, caps, "test", 60_000, 1000)
    }

    fn print(t: i64, q: i64) -> Print {
        Print {
            time: UtcMillis(t),
            price: Price(600_000),
            qty: Qty(q),
            aggressor: Some(Aggressor::Buy),
        }
    }

    fn live(t: i64, q: i64) -> MarketEvent {
        MarketEvent::Trade {
            time: UtcMillis(t),
            price: Price(600_000),
            qty: Qty(q),
            aggressor: Some(Aggressor::Buy),
        }
    }

    /// A source refusing depth mid-feed: the book empties, the reason stays, trades go on.
    #[test]
    fn depth_refused_by_the_source_says_why_and_trades_go_on() {
        let store = ChartStore::new(
            crate::catalog::get(crate::catalog::ES).unwrap(),
            None,
            Capabilities {
                depth: true,
                ..Capabilities::default()
            },
            "test",
            60_000,
            100,
        );
        let level = MarketEvent::BookLevel {
            side: Side::Bid,
            price: Price(600_000),
            qty: Qty(3),
            orders: 1,
        };
        store.update_frame(|d| d.apply(&[level]));
        let reason = "not on your plan".to_owned();
        let refused = MarketEvent::DepthUnavailable { reason };
        let revision = store.revision();
        store.update_frame(|d| d.apply(&[refused]));
        assert_ne!(store.revision(), revision);
        let trade = MarketEvent::Trade {
            time: UtcMillis(1_000),
            price: Price(600_025),
            qty: Qty(1),
            aggressor: None,
        };
        store.update_frame(|d| d.apply(&[trade]));
        let d = store.read();
        assert!(!d.capabilities.depth);
        assert_eq!(d.depth_note.as_deref(), Some("not on your plan"));
        assert_eq!(d.book.best_bid(), None);
        assert_eq!(d.bid, None);
        assert_eq!(d.last_trade, Some(Price(600_025)));
    }

    /// Decision 0028: a frame of option quotes lands in the chains (their revision moves) but
    /// never repaints charts: `chart_epoch` and the chart revision stay put.
    #[test]
    fn option_quotes_fill_the_chains_without_repainting_charts() {
        let spy = catalog::get(catalog::SPY).unwrap();
        let store = ChartStore::new(spy, None, Capabilities::default(), "test", 60_000, 1000);
        let key = lines_core::OptionKey {
            expiry: lines_core::CivilDate::new(2026, 11, 9).unwrap(),
            strike: Price(68_400),
            right: lines_core::Right::Call,
        };
        let quote = lines_core::OptionQuote {
            bid: Some(Price(80)),
            ..Default::default()
        };
        let (rev, chart_rev) = (store.revision(), store.chart_revision());
        let epoch = store.read().chart_epoch;
        store.update_frame(|d| d.apply(&[MarketEvent::OptionQuote { key, quote }]));
        let d = store.read();
        assert_eq!(d.chains.revision(), 1);
        assert_eq!(d.chart_epoch, epoch);
        assert_eq!(d.stats.updates_in, 0, "not the underlying's updates");
        drop(d);
        assert!(store.revision() > rev, "the chain pane sees it");
        assert_eq!(store.chart_revision(), chart_rev, "charts don't repaint");
        // Past the chains' bounds: counted as rejected.
        let far = |i: i64| MarketEvent::OptionQuote {
            key: lines_core::OptionKey {
                expiry: lines_core::CivilDate::new(2027, 1, 1).unwrap().add_days(i),
                ..key
            },
            quote,
        };
        let events: Vec<_> = (0..lines_core::chain::MAX_EXPIRIES as i64)
            .map(far)
            .collect();
        store.update_frame(|d| d.apply(&events));
        assert_eq!(store.read().stats.rejected, 1, "the 401st expiry");
    }

    /// The stored → live boundary: a history snapshot older than the newest stored trade
    /// doesn't count the stored trades again; after it, every live trade counts once.
    #[test]
    fn stored_then_live_counts_every_trade_once() {
        let store = es_store(true);
        let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
        store.update(|d| {
            d.apply_stored(
                &[print(t0 + 1000, 1), print(t0 + 2000, 2)],
                &[(t0, t0 + 2999)],
            )
        });
        {
            let d = store.read();
            assert_eq!(d.vap.total(), Qty(3));
            assert!(d.series.is_empty(), "stored trades never touch the bars");
        }
        store.update(|d| {
            d.begin_connection();
            d.apply(&[MarketEvent::History {
                bars: vec![],
                as_of: Some(UtcMillis(t0 + 1500)),
            }]);
        });
        // Re-sent after the snapshot (stored already), then new.
        store.update(|d| d.apply(&[live(t0 + 2000, 2), live(t0 + 5000, 4)]));
        let d = store.read();
        assert_eq!(d.vap.total(), Qty(1 + 2 + 4));
        assert_eq!(d.tape.len(), 3);
        assert_eq!(
            d.profiles
                .footprint()
                .range(UtcMillis(t0), UtcMillis(t0 + 60_000))
                .map(|(_, v)| v.total().0)
                .sum::<i64>(),
            7
        );
        // Stored [t0, t0+2999] and live from the snapshot on: no hole.
        assert_eq!(
            d.counted_gaps(t0, t0 + 5000),
            CountedGaps {
                any: true,
                ..CountedGaps::default()
            }
        );
    }

    /// The warm-start floor is the newest replayed trade, never the coverage's end (which can
    /// follow a fast local clock): a live trade after the last stored one counts.
    #[test]
    fn the_stored_floor_is_the_newest_stored_trade() {
        let store = es_store(true);
        let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
        // Coverage claimed 90 s past the last stored trade (a clock running ahead).
        store.update(|d| d.apply_stored(&[print(t0 + 1000, 1)], &[(t0, t0 + 91_000)]));
        store.update(|d| {
            d.begin_connection();
            d.apply(&[MarketEvent::History {
                bars: vec![],
                as_of: Some(UtcMillis(t0 + 500)),
            }]);
            d.apply(&[live(t0 + 1000, 1), live(t0 + 30_000, 5)]);
        });
        let d = store.read();
        assert_eq!(
            d.vap.total(),
            Qty(1 + 5),
            "the re-sent stored trade once, the new one counts"
        );
        assert_eq!(d.frame_counted, vec![false, true]);
    }

    /// Holes in the counted trades: where the data starts (`partial from`) and the inner
    /// gaps, but never the market's daily break.
    #[test]
    fn counted_gaps_skip_closed_market_time() {
        let store = es_store(true);
        let at = |s: &str| UtcMillis::parse_iso(s).unwrap().0;
        store.update(|d| {
            // Monday's session to 21:00, a 20-minute hole, then to the 22:00 close; Tuesday's
            // session from its 23:00 open (the 22:00–23:00 break between is closed).
            let a = at("2026-11-10T14:00:00Z");
            d.apply_stored(&[], &[(a, at("2026-11-10T21:00:00Z") - 1)]);
            d.apply_stored(
                &[],
                &[(at("2026-11-10T21:20:00Z"), at("2026-11-10T22:00:00Z") - 1)],
            );
            d.apply_stored(
                &[],
                &[(at("2026-11-10T23:00:00Z"), at("2026-11-11T01:00:00Z"))],
            );
        });
        let d = store.read();
        let g = d.counted_gaps(at("2026-11-10T13:00:00Z"), at("2026-11-11T01:00:00Z"));
        assert!(g.any);
        assert_eq!(
            g.data_from,
            Some(at("2026-11-10T14:00:00Z")),
            "partial from 14:00"
        );
        assert_eq!(
            g.first_gap,
            Some((at("2026-11-10T21:00:00Z"), at("2026-11-10T21:20:00Z") - 1))
        );
        assert_eq!(g.more_gaps, 0, "the 22:00–23:00 break is no gap");
        // A hole under a minute of open market isn't reported.
        drop(d);
        store.update(|d| {
            d.apply_stored(
                &[],
                &[(
                    at("2026-11-11T01:00:00Z") + 30_000,
                    at("2026-11-11T02:00:00Z"),
                )],
            )
        });
        let d = store.read();
        let g = d.counted_gaps(at("2026-11-10T21:30:00Z"), at("2026-11-11T02:00:00Z"));
        assert_eq!((g.data_from, g.first_gap, g.more_gaps), (None, None, 0));
        // Nothing counted in the span: nothing to say.
        assert!(
            !d.counted_gaps(at("2026-11-12T00:00:00Z"), at("2026-11-12T01:00:00Z"))
                .any
        );
    }

    /// Exchange → receive per trade on the receive clock; back-fill skipped; the clock
    /// offset bound is the smallest receive − exchange (negative: the local clock is behind).
    #[test]
    fn trade_latency_and_the_clock_offset() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        let trade = |t: i64| MarketEvent::Trade {
            time: UtcMillis(t),
            price: Price(600_000),
            qty: Qty(1),
            aggressor: None,
        };
        store.update_frame(|d| {
            assert!(d.latency.is_none(), "nothing until the HUD is shown");
            // Received at 100 000: 30 ms and 5 ms late, one back-filled (2 min old), one
            // stamped 20 ms in the future (the local clock is behind).
            d.record_latency(
                &[trade(99_970), trade(99_995), trade(-20_000), trade(100_020)],
                UtcMillis(100_000),
                true,
            );
        });
        let d = store.read();
        let l = d.latency.as_ref().unwrap();
        assert!(l.wall_clock);
        assert_eq!(l.at_ms, 100_000);
        let s = l.exch_recv.summary(100_000);
        assert_eq!(s.n, 3, "back-fill is not latency");
        assert!((30_000..=37_500).contains(&s.p99_us.unwrap()), "{s:?}");
        assert_eq!(l.offset.bound_ms(100_000), Some(-20));
    }

    #[test]
    fn chart_data_age_counts_from_chart_updates_only() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        assert_eq!(store.chart_data_age_us(), None);
        store.update_frame(|_| ()); // nothing a chart shows changed
        assert_eq!(store.chart_data_age_us(), None, "not a chart update");
        store.update(|_| ());
        assert!(store.chart_data_age_us().is_some());
    }

    /// The feed clock: frame time or market time, whichever is newer; runs on (by the
    /// silence) only while live, frozen otherwise.
    #[test]
    fn feed_clock_is_frozen_unless_live() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        assert_eq!(store.feed_clock(), None);
        store.set_frame_time(UtcMillis(5_000));
        store.update_frame(|_| ());
        assert_eq!(
            store.feed_clock(),
            Some(UtcMillis(5_000)),
            "not live: frozen"
        );
        store.set_frame_time(UtcMillis(4_000));
        assert_eq!(
            store.feed_clock(),
            Some(UtcMillis(5_000)),
            "never backwards"
        );
        store.set_status(FeedStatus::Live);
        std::thread::sleep(Duration::from_millis(30));
        assert!(store.feed_clock().unwrap().0 >= 5_020, "live: runs on");
        store.set_status(FeedStatus::ReplayFinished);
        assert_eq!(store.feed_clock(), Some(UtcMillis(5_000)));
    }

    /// Depth that leaves the touch alone repaints only the DOM (the chart revision stays);
    /// a new best bid, a trade or any other update moves both.
    #[test]
    fn chart_revision_ignores_depth_below_the_touch() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        let level = |side, p, q| MarketEvent::BookLevel {
            side,
            price: Price(p),
            qty: Qty(q),
            orders: 1,
        };
        store.update_frame(|d| {
            d.apply(&[
                MarketEvent::BookClear,
                level(Side::Bid, 600_000, 5),
                level(Side::Ask, 600_025, 5),
            ])
        });
        let (r, c) = (store.revision(), store.chart_revision());
        assert_eq!(store.read().bid, Some(Price(600_000)));
        // Size at the touch and a level behind it: the quote is unchanged.
        store.update_frame(|d| {
            d.apply(&[
                level(Side::Bid, 600_000, 9),
                level(Side::Bid, 599_975, 3),
                MarketEvent::BookChecksumOk,
            ])
        });
        assert_eq!(store.revision(), r + 1);
        assert_eq!(store.chart_revision(), c, "depth below the touch");
        // A better bid moves the quote.
        store.update_frame(|d| d.apply(&[level(Side::Bid, 600_025 - 25, 0)]));
        assert_eq!(store.read().bid, Some(Price(599_975)));
        assert_eq!(store.chart_revision(), c + 1, "best bid changed");
        let trade = MarketEvent::Trade {
            time: UtcMillis(1),
            price: Price(600_000),
            qty: Qty(1),
            aggressor: None,
        };
        store.update_frame(|d| d.apply(&[trade]));
        assert_eq!(store.chart_revision(), c + 2, "trade");
        // A repeated quote (BBO sent with every depth batch) is not a change.
        store.update_frame(|d| d.apply(&[MarketEvent::Bid(Price(599_975))]));
        assert_eq!(store.chart_revision(), c + 2, "same bid again");
        store.update_frame(|d| d.apply(&[MarketEvent::Ask(Price(600_050))]));
        assert_eq!(store.chart_revision(), c + 3, "new ask");
        store.set_status(FeedStatus::Live);
        assert_eq!(store.chart_revision(), c + 4, "any other update moves both");
    }

    /// docs/design.md §7, without the clock: silent longer than `stale_after` while live and
    /// the market open is stale; not live, no data yet, or a closed market is not.
    #[test]
    fn staleness_decision_is_deterministic() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        store.set_stale_after(Duration::from_millis(400));
        let ms = Duration::from_millis;
        let trade_at = |iso: &str| MarketEvent::Trade {
            time: UtcMillis::parse_iso(iso).unwrap(),
            price: Price(600_000),
            qty: Qty(1),
            aggressor: None,
        };
        assert_eq!(store.stale_after_quiet(ms(10_000)), None, "no data yet");
        // Wednesday mid-session, connecting: not live yet.
        store.update(|d| d.apply(&[trade_at("2026-11-11T15:00:00Z")]));
        assert_eq!(store.stale_after_quiet(ms(10_000)), None, "not live");
        store.update(|d| d.status = FeedStatus::Live);
        assert_eq!(store.stale_after_quiet(ms(400)), None, "at the limit: fine");
        assert_eq!(store.stale_after_quiet(ms(401)), Some(ms(401)));
        // The market's clock moves on with the silence: 15:00 + 7 h = 22:00Z, the close.
        assert!(
            store.stale_after_quiet(ms(6 * 3_600_000)).is_some(),
            "still open"
        );
        assert_eq!(
            store.stale_after_quiet(ms(7 * 3_600_000 + 1_000)),
            None,
            "closed by then"
        );
        // A finished replay is not live.
        store.update(|d| d.status = FeedStatus::ReplayFinished);
        assert_eq!(store.stale_after_quiet(ms(10_000)), None);
    }

    /// A replay's jump back empties the store but keeps the series' revision and the tape's
    /// print count rising (a derived series and the DOM trail see the change), and counts it.
    #[test]
    fn a_rewind_empties_but_counts_on() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        let t = UtcMillis::parse_iso("2026-11-11T15:00:00Z").unwrap();
        store.update(|d| {
            d.apply(&[MarketEvent::Trade {
                time: t,
                price: Price(600_000),
                qty: Qty(1),
                aggressor: None,
            }])
        });
        let (rev, pushed) = {
            let d = store.read();
            assert_eq!(d.series.len(), 1);
            (d.series.revision(), d.tape.pushed())
        };
        store.rewind();
        let d = store.read();
        assert!(d.series.is_empty() && d.tape.is_empty());
        assert!(
            d.series.revision() > rev,
            "a derived series sees the change"
        );
        assert_eq!(d.tape.pushed(), pushed, "the print count goes on");
        assert_eq!(d.rewinds, 1);
        assert_eq!(store.market_clock(), None, "the clocks go too");
    }

    /// The core's market clock keeps running after a replay finishes; so does the store's
    /// `market_clock` (the feed clock freezes, for deterministic pictures).
    #[test]
    fn market_clock_runs_on_after_the_last_data() {
        let es = catalog::get(catalog::ES).unwrap();
        let store = ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10);
        let ms = Duration::from_millis;
        assert_eq!(store.market_clock_after_quiet(ms(5)), None, "no data yet");
        let t = UtcMillis::parse_iso("2026-11-11T15:00:00Z").unwrap();
        store.update(|d| {
            d.apply(&[MarketEvent::Trade {
                time: t,
                price: Price(600_000),
                qty: Qty(1),
                aggressor: None,
            }])
        });
        store.update(|d| d.status = FeedStatus::ReplayFinished);
        let base = store.market_clock_after_quiet(ms(0)).unwrap();
        assert!(base.0 >= t.0);
        assert_eq!(
            store.market_clock_after_quiet(ms(300_000)),
            Some(UtcMillis(base.0 + 300_000)),
            "not live: still runs"
        );
        assert_eq!(
            store.feed_clock(),
            Some(base),
            "the feed clock stays frozen"
        );
    }

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
