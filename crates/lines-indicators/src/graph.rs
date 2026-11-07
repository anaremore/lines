//! The series graph (decision 0017).
//!
//! Every series is a node keyed by [`SeriesKey`] = (feed, [`Spec`]). The feed fixes the
//! contract and the data source; the spec names what is computed and its parameters.
//! - **Shared**: acquiring a key that exists returns the same node (reference counted), so a
//!   VWAP with the same feed and parameters is computed once however many charts, custom
//!   indicators or plugins consume it.
//! - **A DAG**: a node's inputs are acquired (recursively) before it, so node order is a
//!   topological order. A spec that reaches itself through its inputs is rejected
//!   ([`GraphError::Cycle`]); input types are checked ([`GraphError::Type`]).
//! - **Incremental**: [`Graph::push`] applies one feed batch to the feed's base series, then
//!   updates only dependent nodes, each from the earliest time its inputs changed.
//! - **Deterministic**: evaluation order is node order; nothing reads clocks or I/O.

use std::collections::HashMap;
use std::fmt;

use lines_core::{Bar, UtcMillis};

use crate::average::{Average, AverageSpec};
use crate::bars::{BarsOut, ChartBars, ExternalBars, TimeBars};
use crate::delta::{CumDelta, DeltaPoint, DeltaSpec};
use crate::series::{Input, MarketSeries};
use crate::vwap::{Point, Vwap, VwapSpec};

/// Identifies a running feed (one contract from one source).
pub type FeedId = u64;

/// What a series is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Spec {
    /// The feed's base series at a bar period ([`MarketSeries`]).
    Market { period_ms: i64 },
    /// VWAP over the base series at a bar period.
    Vwap { period_ms: i64, vwap: VwapSpec },
    /// Cumulative aggressor delta over the base series at a bar period.
    CumDelta { period_ms: i64, delta: DeltaSpec },
    /// The chart's bars on a timeframe other than the base's ([`crate::bars`]): derived in
    /// the engine for time bars, set by the runtime for trade-built bars.
    Bars { period_ms: i64, bars: ChartBars },
    /// A moving average or Bollinger Bands on the chart's bars.
    Average {
        period_ms: i64,
        bars: ChartBars,
        average: AverageSpec,
    },
    /// A user or plugin indicator, by name; the engine's factory resolves it (its inputs may
    /// name other indicators, which is how cycles could arise).
    Named(String),
}

impl Spec {
    pub fn output_type(&self) -> SeriesType {
        match self {
            Spec::Market { .. } => SeriesType::Market,
            Spec::Vwap { .. } | Spec::Average { .. } | Spec::Named(_) => SeriesType::Line,
            Spec::CumDelta { .. } => SeriesType::Candles,
            Spec::Bars { .. } => SeriesType::Bars,
        }
    }

    /// What a study on the chart's `bars` reads: the base series itself on its own period
    /// (1 minute), else a bars node.
    pub fn bars_input(period_ms: i64, bars: ChartBars) -> Spec {
        if bars.tf.is_one_minute() {
            Spec::Market { period_ms }
        } else {
            Spec::Bars { period_ms, bars }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SeriesKey {
    pub feed: FeedId,
    pub spec: Spec,
}

/// The shape of a series.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SeriesType {
    /// Buckets of bars and exact sums ([`MarketSeries`]).
    Market,
    /// One value (with σ and flags) per bucket ([`Point`]).
    Line,
    /// One candle per bucket ([`DeltaPoint`]).
    Candles,
    /// The chart's bars ([`BarsOut`]).
    Bars,
}

/// Read access to a node's output.
#[derive(Copy, Clone)]
pub enum Output<'a> {
    Market(&'a MarketSeries),
    Line(&'a [Point]),
    Candles(&'a [DeltaPoint]),
    Bars(BarsOut<'a>),
}

impl<'a> Output<'a> {
    pub fn series_type(&self) -> SeriesType {
        match self {
            Output::Market(_) => SeriesType::Market,
            Output::Line(_) => SeriesType::Line,
            Output::Candles(_) => SeriesType::Candles,
            Output::Bars(_) => SeriesType::Bars,
        }
    }

    /// Bars, from a bars node or the base series' own (1-minute) bars.
    pub fn bars(&self) -> Option<BarsOut<'a>> {
        match *self {
            Output::Bars(b) => Some(b),
            Output::Market(m) => Some(BarsOut {
                bars: m.bar_series().bars(),
                trimmed: m.bar_series().trimmed(),
            }),
            _ => None,
        }
    }
}

/// A computed series. Built-ins (VWAP) and, at roadmap 10, plugins implement the same trait.
///
/// # Mapping to the plugin world (decision 0008, roadmap 10b)
///
/// The WIT world will mirror this trait one to one:
/// ```text
/// world indicator {
///   import host: interface {
///     // Ask the host for a series by spec; returns a handle to the *shared* node.
///     request: func(spec: string) -> result<series-handle, error>;   // e.g. "vwap(reset=rth,bands=1|2)"
///     read-line: func(h: series-handle, from: u32) -> list<point>;
///   }
///   export inputs: func() -> list<string>;              // = Indicator::inputs
///   export update: func(changed-from: s64) -> u32;      // = Indicator::update
///   export output: func(from: u32) -> list<point>;      // = Indicator::output
/// }
/// ```
/// A plugin that wants VWAP declares the input `vwap(reset=rth,bands=1|2)`. The host parses it
/// into `Spec::Vwap`, acquires the node through this graph (sharing it with every chart that
/// shows the same VWAP), and on each update hands the plugin the points that changed since
/// `from`. The plugin never sees trades it didn't ask for and never recomputes a built-in.
pub trait Indicator: Send {
    /// The series it consumes, in order, on its own feed.
    fn inputs(&self) -> Vec<Spec>;
    /// Bring the output up to date; `inputs` changed at or after `from`. Returns the index of
    /// the first output point that may have changed.
    fn update(&mut self, inputs: &[Output<'_>], from: UtcMillis) -> usize;
    fn output(&self) -> Output<'_>;
    /// Evaluations so far (tests, stats).
    fn evaluations(&self) -> u64 {
        0
    }
}

impl Indicator for Vwap {
    fn inputs(&self) -> Vec<Spec> {
        vec![Spec::Market {
            period_ms: self.period_ms(),
        }]
    }

    fn update(&mut self, inputs: &[Output<'_>], from: UtcMillis) -> usize {
        match inputs.first() {
            Some(Output::Market(m)) => Vwap::update(self, m, from),
            _ => 0,
        }
    }

    fn output(&self) -> Output<'_> {
        Output::Line(self.points())
    }

    fn evaluations(&self) -> u64 {
        self.evaluations
    }
}

impl Indicator for CumDelta {
    fn inputs(&self) -> Vec<Spec> {
        vec![Spec::Market {
            period_ms: self.period_ms(),
        }]
    }

    fn update(&mut self, inputs: &[Output<'_>], from: UtcMillis) -> usize {
        match inputs.first() {
            Some(Output::Market(m)) => CumDelta::update(self, m, from),
            _ => 0,
        }
    }

    fn output(&self) -> Output<'_> {
        Output::Candles(self.points())
    }

    fn evaluations(&self) -> u64 {
        self.evaluations
    }
}

impl Indicator for TimeBars {
    fn inputs(&self) -> Vec<Spec> {
        vec![Spec::Market {
            period_ms: self.period_ms(),
        }]
    }

    fn update(&mut self, inputs: &[Output<'_>], _from: UtcMillis) -> usize {
        if let Some(Output::Market(m)) = inputs.first() {
            TimeBars::update(self, m);
        }
        0
    }

    fn output(&self) -> Output<'_> {
        Output::Bars(TimeBars::output(self))
    }

    fn evaluations(&self) -> u64 {
        self.evaluations
    }
}

/// A study on the chart's bars and the series it reads them from.
struct OnBars<T> {
    input: Spec,
    imp: T,
}

impl Indicator for OnBars<Average> {
    fn inputs(&self) -> Vec<Spec> {
        vec![self.input.clone()]
    }

    fn update(&mut self, inputs: &[Output<'_>], from: UtcMillis) -> usize {
        match inputs.first().and_then(Output::bars) {
            Some(b) => self.imp.update(b, from),
            None => 0,
        }
    }

    fn output(&self) -> Output<'_> {
        Output::Line(self.imp.points())
    }

    fn evaluations(&self) -> u64 {
        self.imp.evaluations
    }
}

/// A node's computation.
pub enum NodeKind {
    Market(Box<MarketSeries>),
    /// Bars set from outside the graph ([`Graph::sync_bars`]): the store's trade-built bars.
    Bars(Box<ExternalBars>),
    Indicator {
        /// `imp.inputs()`, captured when the node is made.
        inputs: Vec<Spec>,
        imp: Box<dyn Indicator>,
    },
}

impl NodeKind {
    pub fn indicator(imp: Box<dyn Indicator>) -> NodeKind {
        NodeKind::Indicator {
            inputs: imp.inputs(),
            imp,
        }
    }

    fn input_specs(&self) -> &[Spec] {
        match self {
            NodeKind::Market(_) | NodeKind::Bars(_) => &[],
            NodeKind::Indicator { inputs, .. } => inputs,
        }
    }

    fn output(&self) -> Output<'_> {
        match self {
            NodeKind::Market(m) => Output::Market(m),
            NodeKind::Bars(b) => Output::Bars(b.output()),
            NodeKind::Indicator { imp, .. } => imp.output(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// The spec reaches itself through its inputs.
    Cycle(Vec<Spec>),
    /// The factory doesn't know the spec.
    Unknown(Spec),
    /// An input has the wrong shape for its consumer.
    Type {
        input: Spec,
        expected: SeriesType,
        got: SeriesType,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Cycle(path) => write!(f, "indicator cycle: {path:?}"),
            GraphError::Unknown(s) => write!(f, "unknown series {s:?}"),
            GraphError::Type {
                input,
                expected,
                got,
            } => write!(f, "{input:?} is {got:?}, expected {expected:?}"),
        }
    }
}

impl std::error::Error for GraphError {}

/// Makes the node for a key (built-ins and named indicators). `None`: unknown.
pub type Factory<'a> = dyn FnMut(&SeriesKey) -> Option<NodeKind> + 'a;

/// The node for a built-in spec. `calendar`: the feed product's sessions (VWAP resets).
pub fn make_builtin(
    key: &SeriesKey,
    calendar: Option<&'static lines_core::sessions::SessionCalendar>,
    max_buckets: usize,
) -> Option<NodeKind> {
    match &key.spec {
        Spec::Market { period_ms } => Some(NodeKind::Market(Box::new(MarketSeries::new(
            *period_ms,
            max_buckets,
        )))),
        Spec::Vwap { period_ms, vwap } => Some(NodeKind::indicator(Box::new(Vwap::new(
            *vwap, *period_ms, calendar,
        )))),
        Spec::CumDelta { period_ms, delta } => Some(NodeKind::indicator(Box::new(CumDelta::new(
            *delta, *period_ms, calendar,
        )))),
        // The base series is the 1-minute bars (`Spec::bars_input`).
        Spec::Bars { bars, .. } if bars.tf.is_one_minute() => None,
        Spec::Bars { bars, .. } if bars.from_trades() => {
            Some(NodeKind::Bars(Box::new(ExternalBars::new(max_buckets))))
        }
        Spec::Bars { period_ms, bars } => Some(NodeKind::indicator(Box::new(TimeBars::new(
            *period_ms,
            bars.tf,
            calendar,
            max_buckets,
        )))),
        Spec::Average {
            period_ms,
            bars,
            average,
        } => Some(NodeKind::indicator(Box::new(OnBars {
            input: Spec::bars_input(*period_ms, *bars),
            imp: Average::new(*average),
        }))),
        Spec::Named(_) => None,
    }
}

struct Node {
    key: SeriesKey,
    kind: NodeKind,
    inputs: Vec<usize>,
    refs: u32,
}

/// A node changed in the last push: output points from `first` may differ.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub node: usize,
    pub first: usize,
}

#[derive(Default)]
pub struct Graph {
    /// Append-only (slots are not reused), so index order stays topological.
    nodes: Vec<Option<Node>>,
    index: HashMap<SeriesKey, usize>,
}

impl Graph {
    pub fn new() -> Graph {
        Graph::default()
    }

    /// Live nodes.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn key(&self, id: usize) -> Option<&SeriesKey> {
        self.nodes.get(id)?.as_ref().map(|n| &n.key)
    }

    pub fn output(&self, id: usize) -> Option<Output<'_>> {
        self.nodes.get(id)?.as_ref().map(|n| n.kind.output())
    }

    pub fn evaluations(&self, id: usize) -> u64 {
        match self.nodes.get(id).and_then(|n| n.as_ref()).map(|n| &n.kind) {
            Some(NodeKind::Indicator { imp, .. }) => imp.evaluations(),
            Some(NodeKind::Market(m)) => m.batches,
            Some(NodeKind::Bars(b)) => b.syncs,
            None => 0,
        }
    }

    /// Get or create the node for `key` (and, recursively, its inputs). Each call takes one
    /// reference; [`Graph::release`] gives it back.
    pub fn acquire(&mut self, key: SeriesKey, make: &mut Factory<'_>) -> Result<usize, GraphError> {
        let mut path = Vec::new();
        self.acquire_in(key, make, &mut path)
    }

    fn acquire_in(
        &mut self,
        key: SeriesKey,
        make: &mut Factory<'_>,
        path: &mut Vec<Spec>,
    ) -> Result<usize, GraphError> {
        if path.contains(&key.spec) {
            let mut cycle = path.clone();
            cycle.push(key.spec);
            return Err(GraphError::Cycle(cycle));
        }
        if let Some(&id) = self.index.get(&key) {
            if let Some(n) = self.nodes[id].as_mut() {
                n.refs += 1;
            }
            return Ok(id);
        }
        let kind = make(&key).ok_or_else(|| GraphError::Unknown(key.spec.clone()))?;
        path.push(key.spec.clone());
        let mut inputs = Vec::new();
        for spec in kind.input_specs().to_vec() {
            let input_key = SeriesKey {
                feed: key.feed,
                spec: spec.clone(),
            };
            match self.acquire_in(input_key, make, path) {
                Ok(id) => {
                    // Built-ins say which shape they need through the spec; named inputs are
                    // lines.
                    let got = self.output(id).map(|o| o.series_type());
                    let want = spec.output_type();
                    if got != Some(want) {
                        for &i in &inputs {
                            self.release(i);
                        }
                        self.release(id);
                        path.pop();
                        return Err(GraphError::Type {
                            input: spec,
                            expected: want,
                            got: got.unwrap_or(SeriesType::Line),
                        });
                    }
                    inputs.push(id);
                }
                Err(e) => {
                    for &i in &inputs {
                        self.release(i);
                    }
                    path.pop();
                    return Err(e);
                }
            }
        }
        path.pop();
        let id = self.nodes.len();
        self.nodes.push(Some(Node {
            key: key.clone(),
            kind,
            inputs,
            refs: 1,
        }));
        self.index.insert(key, id);
        // A new node starts from everything its inputs already have.
        self.evaluate(id, UtcMillis(i64::MIN));
        Ok(id)
    }

    /// Drop one reference; a node with none left is removed, releasing its inputs.
    pub fn release(&mut self, id: usize) {
        let Some(n) = self.nodes.get_mut(id).and_then(|n| n.as_mut()) else {
            return;
        };
        n.refs -= 1;
        if n.refs > 0 {
            return;
        }
        let n = self.nodes[id].take().expect("checked above");
        self.index.remove(&n.key);
        for i in n.inputs {
            self.release(i);
        }
    }

    fn evaluate(&mut self, id: usize, from: UtcMillis) -> Option<usize> {
        // Inputs have smaller indices: split the slice to read them while updating `id`.
        let (before, rest) = self.nodes.split_at_mut(id);
        let node = rest[0].as_mut()?;
        let NodeKind::Indicator { imp, .. } = &mut node.kind else {
            return None;
        };
        let outs: Vec<Output<'_>> = node
            .inputs
            .iter()
            .filter_map(|&i| before[i].as_ref().map(|n| n.kind.output()))
            .collect();
        Some(imp.update(&outs, from))
    }

    /// Apply one batch of a feed's events to its base series and update every node that
    /// depends on them, in order. Returns the nodes whose output changed.
    pub fn push(&mut self, feed: FeedId, inputs: &[Input]) -> Vec<Changed> {
        let mut dirty: HashMap<usize, UtcMillis> = HashMap::new();
        let mut changed = Vec::new();
        self.propagate(feed, 0, Some(inputs), &mut dirty, &mut changed);
        changed
    }

    /// The node for `key`, if it exists.
    pub fn id(&self, key: &SeriesKey) -> Option<usize> {
        self.index.get(key).copied()
    }

    /// Bring bars node `id` (trade-built bars, [`NodeKind::Bars`]) up to `bars` (`trimmed`
    /// dropped from their front so far). Cheap: compares and copies only what changed (the
    /// runtime calls it under the store's lock). Returns the first changed bar's time, for
    /// [`Graph::bars_changed`].
    pub fn sync_bars(&mut self, id: usize, bars: &[Bar], trimmed: u64) -> Option<UtcMillis> {
        match &mut self.nodes.get_mut(id)?.as_mut()?.kind {
            NodeKind::Bars(b) => b.sync(bars, trimmed),
            _ => None,
        }
    }

    /// Update everything that depends on bars node `id`, changed from `from`
    /// ([`Graph::sync_bars`]). Returns the nodes whose output changed (`id` first).
    pub fn bars_changed(&mut self, id: usize, from: UtcMillis) -> Vec<Changed> {
        let Some(feed) = self.key(id).map(|k| k.feed) else {
            return Vec::new();
        };
        let mut dirty = HashMap::from([(id, from)]);
        let mut changed = vec![Changed { node: id, first: 0 }];
        self.propagate(feed, id + 1, None, &mut dirty, &mut changed);
        changed
    }

    /// Nodes of `feed` from `start` on, in order: base series take `inputs` (if any), the
    /// others update from the earliest change of their inputs.
    fn propagate(
        &mut self,
        feed: FeedId,
        start: usize,
        inputs: Option<&[Input]>,
        dirty: &mut HashMap<usize, UtcMillis>,
        changed: &mut Vec<Changed>,
    ) {
        for id in start..self.nodes.len() {
            let Some(node) = self.nodes[id].as_mut() else {
                continue;
            };
            if node.key.feed != feed {
                continue;
            }
            match &mut node.kind {
                NodeKind::Market(m) => {
                    if let Some(from) = inputs.and_then(|i| m.apply(i)) {
                        dirty.insert(id, from);
                        changed.push(Changed { node: id, first: 0 });
                    }
                }
                NodeKind::Bars(_) => {}
                NodeKind::Indicator { .. } => {
                    let from = node
                        .inputs
                        .iter()
                        .filter_map(|i| dirty.get(i))
                        .min()
                        .copied();
                    if let Some(from) = from
                        && let Some(first) = self.evaluate(id, from)
                    {
                        dirty.insert(id, from);
                        changed.push(Changed { node: id, first });
                    }
                }
            }
        }
    }
}
