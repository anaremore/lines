//! The indicator runtime (decision 0017): one indicator thread (never the UI or a feed thread)
//! runs the series graph (`lines_indicators::Graph`) for every feed.
//!
//! - **Inputs**: every feed's normalized events reach the engine through an [`EngineTap`]
//!   (an `EventSink` next to the store writer), from the feed's first frame, so each feed's
//!   base series is complete whenever an indicator is added later.
//! - **Outputs**: a consumer (a chart pane) [`Engine::acquire`]s a spec and gets an
//!   [`IndicatorHandle`]: an [`OutputCell`] the engine updates in place (only the points that
//!   changed) and bumps an atomic revision, like the chart store. The UI polls the revision and
//!   `try_lock`s in paint; it never waits on the engine. Acquiring is a message, not a call.
//! - **Sharing**: identical (feed, spec) requests share one graph node; dropping the last
//!   handle releases it. A feed's base series lives as long as its chart store (a finished
//!   replay stays on screen and can still get indicators).
//! - **Deterministic**: the engine processes each feed's batches in feed order.
//! - **Trade-built bars** (tick, volume, range, seconds): bar studies on them read the bars the
//!   chart's store builds (the engine keeps sums, not trades, so it can't rebuild them). A
//!   consumer passes a [`TradeBarsSource`]; the engine copies what changed into the shared bars
//!   node after each batch of that feed, every [`BARS_POLL`] while idle (a rebuild lands
//!   without new trades), and before answering [`Engine::sync`]. Only the copy holds the
//!   store's lock; the studies compute after it is released.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use lines_core::{ProductId, UtcMillis};
use lines_indicators::graph::{SeriesKey, Spec, make_builtin};
use lines_indicators::{ChartBars, FeedId, Graph, Input, Output, Point};
use parking_lot::{Mutex, MutexGuard};

use crate::connection::EventSink;
use crate::provider::MarketEvent;
use crate::store::ChartStore;
use crate::ticks::TickKey;

/// How often the engine copies trade-built bars while no batch arrives.
pub const BARS_POLL: Duration = Duration::from_millis(250);

/// Where a bar study on trade-built bars reads them: a chart store's tick builder. Weak: the
/// engine never keeps a store alive.
#[derive(Clone, Debug)]
pub struct TradeBarsSource {
    store: Weak<ChartStore>,
    key: TickKey,
}

impl TradeBarsSource {
    pub fn new(store: &Arc<ChartStore>, key: TickKey) -> TradeBarsSource {
        TradeBarsSource {
            store: Arc::downgrade(store),
            key,
        }
    }
}

/// What a consumer reads.
#[derive(Debug, Default)]
pub struct OutputData {
    /// Line indicators (VWAP).
    pub points: Vec<Point>,
    /// Candle indicators (cumulative delta).
    pub candles: Vec<lines_indicators::DeltaPoint>,
    /// The spec couldn't be built (shown inline, never silently empty).
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct OutputCell {
    revision: AtomicU64,
    data: Mutex<OutputData>,
}

impl OutputCell {
    /// Lock-free change detector.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Never blocks: `None` if the engine is writing this instant.
    pub fn try_read(&self) -> Option<MutexGuard<'_, OutputData>> {
        self.data.try_lock()
    }

    /// Blocking read (tests, non-UI callers).
    pub fn read(&self) -> MutexGuard<'_, OutputData> {
        self.data.lock()
    }

    fn sync_from(&self, out: Output<'_>, first: usize) {
        fn sync<T: Copy>(dst: &mut Vec<T>, src: &[T], first: usize) {
            let first = first.min(dst.len()).min(src.len());
            dst.truncate(first);
            dst.extend_from_slice(&src[first..]);
        }
        {
            let mut d = self.data.lock();
            match out {
                Output::Line(points) => sync(&mut d.points, points, first),
                Output::Candles(c) => sync(&mut d.candles, c, first),
                Output::Market(_) | Output::Bars(_) => return,
            }
        }
        self.revision.fetch_add(1, Ordering::Release);
    }
}

/// Where a feed's series come from.
#[derive(Copy, Clone, Debug)]
pub struct FeedInfo {
    pub feed: FeedId,
    pub product: ProductId,
    pub period_ms: i64,
    pub max_bars: usize,
}

enum Msg {
    FeedStart(FeedInfo),
    FeedEnd(FeedId),
    Events(FeedId, Vec<Input>),
    Acquire(FeedInfo, Spec, Arc<OutputCell>, Option<TradeBarsSource>),
    Release(FeedId, Spec, Arc<OutputCell>),
    Sync(Sender<()>),
}

/// The process-wide indicator engine.
pub struct Engine {
    tx: Mutex<Sender<Msg>>,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// The engine (started on first use).
pub fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("lines-indicators".into())
            .spawn(move || run(rx));
        if let Err(e) = spawned {
            tracing::error!("indicator thread failed to start: {e}");
        }
        Engine { tx: Mutex::new(tx) }
    })
}

/// A chart store went away: its feed's base series can go too (once no indicator uses it).
/// Its data outlives the feed task (a finished replay is still on screen), so this, not the
/// end of the feed, releases it.
pub(crate) fn store_dropped(feed: FeedId) {
    if let Some(e) = ENGINE.get() {
        e.send(Msg::FeedEnd(feed));
    }
}

impl Engine {
    fn send(&self, m: Msg) {
        let _ = self.tx.lock().send(m);
    }

    /// Ask for a series on a feed. Returns at once; the handle's cell fills (or reports an
    /// error) shortly after, from the engine thread.
    pub fn acquire(&self, info: FeedInfo, spec: Spec) -> IndicatorHandle {
        self.acquire_on(info, spec, None)
    }

    /// As [`Engine::acquire`], for a bar study on trade-built bars: `bars` is where they are
    /// built (ignored for other specs).
    pub fn acquire_on(
        &self,
        info: FeedInfo,
        spec: Spec,
        bars: Option<TradeBarsSource>,
    ) -> IndicatorHandle {
        let cell = Arc::new(OutputCell::default());
        self.send(Msg::Acquire(info, spec.clone(), cell.clone(), bars));
        IndicatorHandle {
            feed: info.feed,
            spec,
            cell,
        }
    }

    /// The tap for one feed's events (the feed driver owns it). `replay`: the feed replays a
    /// recording, which holds every trade from its first frame.
    pub fn tap(&'static self, info: FeedInfo, expect_history: bool, replay: bool) -> EngineTap {
        self.send(Msg::FeedStart(info));
        EngineTap {
            engine: self,
            info,
            expect_history,
            complete_from_start: replay && !expect_history,
            first_frame: false,
        }
    }

    /// Wait until everything sent so far has been processed (tests, screenshot mode).
    pub fn sync(&self) {
        let (tx, rx) = mpsc::channel();
        self.send(Msg::Sync(tx));
        let _ = rx.recv();
    }
}

/// A consumer's hold on a series. Dropping it releases the node (if nobody else uses it).
pub struct IndicatorHandle {
    feed: FeedId,
    spec: Spec,
    cell: Arc<OutputCell>,
}

impl IndicatorHandle {
    pub fn cell(&self) -> &OutputCell {
        &self.cell
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }
}

impl Drop for IndicatorHandle {
    fn drop(&mut self) {
        engine().send(Msg::Release(
            self.feed,
            self.spec.clone(),
            self.cell.clone(),
        ));
    }
}

/// Forwards one feed's events to the engine.
pub struct EngineTap {
    engine: &'static Engine,
    info: FeedInfo,
    expect_history: bool,
    /// A replay: the stream holds every trade from its first frame (`Input::TradesFrom`).
    complete_from_start: bool,
    /// The next frame is the connection's first.
    first_frame: bool,
}

impl EngineTap {
    /// The feed moved to another contract (a roll): its series start over.
    pub fn reset(&mut self) {
        self.engine
            .send(Msg::Events(self.info.feed, vec![Input::Reset]));
    }
}

impl EngineTap {
    /// The local store's cached bars (before the feed connects): the engine's series starts
    /// from the same bars as the chart.
    pub fn cached(&mut self, bars: Vec<lines_core::Bar>) {
        if !bars.is_empty() {
            self.engine
                .send(Msg::Events(self.info.feed, vec![Input::Cached(bars)]));
        }
    }

    /// Stored order flow replayed at startup (`disk::orderflow`): one trade date's trades and
    /// where they are complete.
    pub fn stored(&mut self, trades: Vec<lines_core::Print>, covered: Vec<(i64, i64)>) {
        if !trades.is_empty() || !covered.is_empty() {
            self.engine.send(Msg::Events(
                self.info.feed,
                vec![Input::Stored { trades, covered }],
            ));
        }
    }
}

impl EventSink for EngineTap {
    fn begin(
        &mut self,
        newest_bar: Option<UtcMillis>,
        _window_start: UtcMillis,
    ) -> Option<UtcMillis> {
        self.engine.send(Msg::Events(
            self.info.feed,
            vec![Input::Connect {
                expect_history: self.expect_history,
            }],
        ));
        self.first_frame = true;
        newest_bar
    }

    fn frame(&mut self, events: &[MarketEvent], now: UtcMillis) {
        let mut inputs = Vec::new();
        // A replay without history: every trade from its first recorded frame (the replay
        // clock) is in the recording, so VWAP is exact from that time, not from its first
        // trade (a replay starting at the session open is exact for the whole session).
        if std::mem::take(&mut self.first_frame) && self.complete_from_start {
            inputs.push(Input::TradesFrom(now));
        }
        for e in events {
            inputs.push(match e {
                MarketEvent::Trade {
                    time,
                    price,
                    qty,
                    aggressor,
                } => Input::Trade {
                    time: *time,
                    price: *price,
                    qty: *qty,
                    aggressor: *aggressor,
                },
                MarketEvent::History { bars, as_of } => Input::History {
                    bars: bars.clone(),
                    as_of: *as_of,
                },
                MarketEvent::HistoryComplete => Input::HistoryComplete,
                MarketEvent::Gap { .. } => Input::Gap,
                MarketEvent::ContractChanged(_) => Input::Reset,
                _ => continue,
            });
        }
        if !inputs.is_empty() {
            self.engine.send(Msg::Events(self.info.feed, inputs));
        }
    }
}

/// Both sinks see every call (the store writer and the engine tap).
pub struct Tee<'a>(pub &'a mut dyn EventSink, pub &'a mut dyn EventSink);

impl EventSink for Tee<'_> {
    fn begin(
        &mut self,
        newest_bar: Option<UtcMillis>,
        window_start: UtcMillis,
    ) -> Option<UtcMillis> {
        let from = self.0.begin(newest_bar, window_start);
        self.1.begin(newest_bar, window_start);
        from
    }

    fn frame(&mut self, events: &[MarketEvent], now: UtcMillis) {
        self.0.frame(events, now);
        self.1.frame(events, now);
    }

    fn end(&mut self, now: UtcMillis) {
        self.0.end(now);
        self.1.end(now);
    }
}

struct State {
    graph: Graph,
    /// Base-series references held for running feeds.
    feeds: HashMap<FeedId, (FeedInfo, usize)>,
    /// Consumers' cells per node.
    cells: HashMap<usize, Vec<Arc<OutputCell>>>,
    /// Trade-built bars nodes the engine copies from a store, with the references it holds.
    sources: HashMap<usize, (TradeBarsSource, u32)>,
    /// Consumers (by cell) whose acquire took a reference on a trade-built bars node.
    holds: HashMap<usize, usize>,
}

/// The trade-built bars a spec reads, if any.
fn trade_bars(spec: &Spec) -> Option<(i64, ChartBars)> {
    match spec {
        Spec::Average {
            period_ms, bars, ..
        } if bars.from_trades() => Some((*period_ms, *bars)),
        _ => None,
    }
}

impl State {
    fn acquire(&mut self, info: FeedInfo, spec: Spec) -> Result<usize, String> {
        let cal = crate::catalog::sessions(info.product);
        let max = info.max_bars;
        let mut make = |k: &SeriesKey| make_builtin(k, cal, max);
        self.graph
            .acquire(
                SeriesKey {
                    feed: info.feed,
                    spec,
                },
                &mut make,
            )
            .map_err(|e| e.to_string())
    }

    /// Hand changed outputs to their consumers.
    fn publish(&self, changed: &[lines_indicators::graph::Changed]) {
        for c in changed {
            let (Some(cells), Some(out)) = (self.cells.get(&c.node), self.graph.output(c.node))
            else {
                continue;
            };
            for cell in cells {
                cell.sync_from(out, c.first);
            }
        }
    }

    /// Copy what changed of trade-built bars (all sources, or one feed's) and update what
    /// depends on them.
    fn sync_bars(&mut self, feed: Option<FeedId>) {
        let ids: Vec<usize> = self.sources.keys().copied().collect();
        for id in ids {
            if feed.is_some_and(|f| self.graph.key(id).is_none_or(|k| k.feed != f)) {
                continue;
            }
            let Some((src, _)) = self.sources.get(&id) else {
                continue;
            };
            let Some(store) = src.store.upgrade() else {
                continue;
            };
            let key = src.key;
            let from = {
                let d = store.read();
                match d.ticks.bars(key) {
                    Some(tb) => self.graph.sync_bars(id, tb.bars(), tb.trimmed()),
                    None => None,
                }
            };
            if let Some(from) = from {
                let changed = self.graph.bars_changed(id, from);
                self.publish(&changed);
            }
        }
    }

    fn find(&self, feed: FeedId, spec: &Spec) -> Option<usize> {
        self.cells.keys().copied().find(|&id| {
            self.graph
                .key(id)
                .is_some_and(|k| k.feed == feed && &k.spec == spec)
        })
    }
}

/// How long the engine may wait for its next message: `None` blocks (no study reads
/// trade-built bars); otherwise until `BARS_POLL` after the last full copy, which runs whatever
/// other feeds send meanwhile (a busy feed never starves a quiet one's rebuilt bars).
fn bars_wait(copying: bool, since_full: Duration) -> Option<Duration> {
    copying.then(|| BARS_POLL.saturating_sub(since_full))
}

fn run(rx: Receiver<Msg>) {
    let mut st = State {
        graph: Graph::new(),
        feeds: HashMap::new(),
        cells: HashMap::new(),
        sources: HashMap::new(),
        holds: HashMap::new(),
    };
    let mut last_full = Instant::now();
    loop {
        // Every `BARS_POLL`, all trade-built bars are copied, however busy any feed is.
        if !st.sources.is_empty() && last_full.elapsed() >= BARS_POLL {
            st.sync_bars(None);
            last_full = Instant::now();
        }
        let next = match bars_wait(!st.sources.is_empty(), last_full.elapsed()) {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(wait) => rx.recv_timeout(wait),
        };
        let msg = match next {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Msg::FeedStart(info) => {
                // Held from the feed's start until its store is dropped.
                if st.feeds.contains_key(&info.feed) {
                    continue;
                }
                let base = Spec::Market {
                    period_ms: info.period_ms,
                };
                match st.acquire(info, base) {
                    Ok(id) => {
                        st.feeds.insert(info.feed, (info, id));
                    }
                    Err(e) => tracing::error!("indicator base for feed {}: {e}", info.feed),
                }
            }
            Msg::FeedEnd(feed) => {
                if let Some((_, id)) = st.feeds.remove(&feed) {
                    st.graph.release(id);
                }
            }
            Msg::Events(feed, inputs) => {
                let changed = st.graph.push(feed, &inputs);
                st.publish(&changed);
                st.sync_bars(Some(feed));
            }
            Msg::Acquire(info, spec, cell, source) => {
                // Trade-built bars: hold the bars node and fill it before the study is made,
                // so it starts from every bar the chart has.
                if let (Some((period_ms, bars)), Some(source)) = (trade_bars(&spec), source) {
                    match st.acquire(info, Spec::Bars { period_ms, bars }) {
                        Ok(id) => {
                            st.sources.entry(id).or_insert((source, 0)).1 += 1;
                            st.holds.insert(Arc::as_ptr(&cell) as usize, id);
                            st.sync_bars(Some(info.feed));
                        }
                        Err(e) => tracing::error!("trade bars for feed {}: {e}", info.feed),
                    }
                }
                match st.acquire(info, spec) {
                    Ok(id) => {
                        if let Some(out) = st.graph.output(id) {
                            cell.sync_from(out, 0);
                        }
                        st.cells.entry(id).or_default().push(cell);
                    }
                    Err(e) => {
                        cell.data.lock().error = Some(e);
                        cell.revision.fetch_add(1, Ordering::Release);
                    }
                }
            }
            Msg::Release(feed, spec, cell) => {
                // Only a consumer that took a bars reference gives one back.
                if let Some(id) = st.holds.remove(&(Arc::as_ptr(&cell) as usize))
                    && let Some(e) = st.sources.get_mut(&id)
                {
                    e.1 -= 1;
                    if e.1 == 0 {
                        st.sources.remove(&id);
                    }
                    st.graph.release(id);
                }
                if let Some(id) = st.find(feed, &spec) {
                    if let Some(v) = st.cells.get_mut(&id) {
                        v.retain(|c| !Arc::ptr_eq(c, &cell));
                        if v.is_empty() {
                            st.cells.remove(&id);
                        }
                    }
                    st.graph.release(id);
                }
            }
            Msg::Sync(reply) => {
                st.sync_bars(None);
                let _ = reply.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full copy of trade-built bars is due `BARS_POLL` after the last one, counted from
    /// that copy, not from the last message: a feed sending every few ms can't postpone it.
    #[test]
    fn the_bar_copy_is_due_whatever_other_feeds_send() {
        assert_eq!(
            bars_wait(false, Duration::ZERO),
            None,
            "nothing to copy: block"
        );
        assert_eq!(bars_wait(true, Duration::ZERO), Some(BARS_POLL));
        let ms = Duration::from_millis;
        assert_eq!(bars_wait(true, ms(200)), Some(BARS_POLL - ms(200)));
        assert_eq!(bars_wait(true, BARS_POLL + ms(5)), Some(Duration::ZERO));
    }
}
