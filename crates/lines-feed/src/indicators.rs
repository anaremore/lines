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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};

use lines_core::{ProductId, UtcMillis};
use lines_indicators::graph::{SeriesKey, Spec, make_builtin};
use lines_indicators::{FeedId, Graph, Input, Output, Point};
use parking_lot::{Mutex, MutexGuard};

use crate::connection::EventSink;
use crate::provider::MarketEvent;

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
                Output::Market(_) => return,
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
    Acquire(FeedInfo, Spec, Arc<OutputCell>),
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
        let cell = Arc::new(OutputCell::default());
        self.send(Msg::Acquire(info, spec.clone(), cell.clone()));
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

    /// The local store's bar coverage (inclusive ms ranges): a history gap the provider
    /// reports inside it isn't missing for the chart (`Input::Covered`).
    pub fn covered(&mut self, ranges: Vec<(i64, i64)>) {
        if !ranges.is_empty() {
            self.engine
                .send(Msg::Events(self.info.feed, vec![Input::Covered(ranges)]));
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
                MarketEvent::HistoryGap { from, to } => Input::HistoryGap {
                    from: *from,
                    to: *to,
                },
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

    fn find(&self, feed: FeedId, spec: &Spec) -> Option<usize> {
        self.cells.keys().copied().find(|&id| {
            self.graph
                .key(id)
                .is_some_and(|k| k.feed == feed && &k.spec == spec)
        })
    }
}

fn run(rx: Receiver<Msg>) {
    let mut st = State {
        graph: Graph::new(),
        feeds: HashMap::new(),
        cells: HashMap::new(),
    };
    while let Ok(msg) = rx.recv() {
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
                for c in st.graph.push(feed, &inputs) {
                    let (Some(cells), Some(out)) = (st.cells.get(&c.node), st.graph.output(c.node))
                    else {
                        continue;
                    };
                    for cell in cells {
                        cell.sync_from(out, c.first);
                    }
                }
            }
            Msg::Acquire(info, spec, cell) => match st.acquire(info, spec) {
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
            },
            Msg::Release(feed, spec, cell) => {
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
                let _ = reply.send(());
            }
        }
    }
}
