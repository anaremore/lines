//! External level sources (decision 0022): the public half.
//!
//! A [`LevelSource`] publishes [`LevelMap`]s (lines, zones, labels: `lines_core::levels`) for
//! the products it serves. Sources are registered process-wide ([`register`]), like market
//! data extensions (decision 0020); official builds register theirs at start-up. Nothing in
//! this module names a vendor.
//!
//! # Shape (decision 0011, I/O-free decoding)
//!
//! - [`LevelSource::connect`] opens the source's own [`LevelTransport`]: the adapter does its
//!   HTTP / SSE (and keeps any credentials) itself.
//! - The runtime ([`runtime`]) asks the transport for [`LevelFrame`]s: one refresh's
//!   responses for poll and one-shot sources ([`LevelTransport::fetch`]), events for SSE
//!   ([`LevelTransport::next_event`]). Each batch is stamped, optionally **recorded**, then
//!   handed to [`LevelSource::decode`], which is pure: frames (+ the previous map + the
//!   chart's prices) in, map out.
//! - Replay feeds the recorded batches to the same `decode` at their recorded times, so a
//!   replay shows exactly the levels the live run showed.
//!
//! Fetching and decoding run on the levels runtime thread, never the UI thread.

pub mod runtime;
pub mod setting;

#[cfg(any(test, feature = "demo-levels"))]
pub mod demo;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail};
use bytes::Bytes;
pub use futures_util::future::BoxFuture;
use lines_core::{ContractKey, Price, Product, UtcMillis};
use parking_lot::RwLock;

pub use lines_core::levels::{
    LabelMode, LabelPriority, LevelError, LevelMap, LevelMapBuilder, LevelMeta, LevelShape,
    LevelStyle, LevelType, LevelValue, Marker, NamedValue, Series, TypeStyle,
};

/// How a source refreshes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    /// [`LevelTransport::fetch`] every interval.
    Poll(Duration),
    /// A stream: [`LevelTransport::next_event`] until it ends, then reconnect.
    Sse,
    /// One fetch per connection (static maps).
    OneShot,
}

/// One setting a source needs (decision 0021: settings are data). Lines draws every one
/// itself, in the Studies menu's level editor and Settings › Plugins, from this description
/// alone: the kinds are a closed set and a plugin never draws its own UI. Values are text
/// ([`SettingValues`]); [`setting`] parses, checks, steps and formats them.
///
/// Build with [`SettingSpec::new`] and the `const` setters, so a field added later never breaks
/// a source:
///
/// ```
/// use lines_feed::levels::{SettingKind, SettingSpec};
/// const REFRESH: SettingSpec = SettingSpec::new(
///     "refresh", "Refresh", SettingKind::Seconds { min: 1, max: 60 }, "5", "How often to poll.",
/// )
/// .group("Connection")
/// .connection();
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SettingSpec {
    /// Stable key, e.g. `service_url`.
    pub key: &'static str,
    pub label: &'static str,
    pub kind: SettingKind,
    /// Default, as text (`""`: none).
    pub default: &'static str,
    pub help: &'static str,
    /// The heading it is listed under ("Connection"); consecutive settings of one group share
    /// it. `""`: none.
    pub group: &'static str,
    /// Shown, dimmed, while an optional value is empty ("from the service"). `""`: none.
    pub placeholder: &'static str,
    /// Lines' own controls do this (a source's labels, zone fills or legend stats): never
    /// shown; the source keeps its default.
    pub owned_by_lines: bool,
    /// A switch that asks before it turns on: the question. `""`: never asks. Turning it off
    /// never asks.
    pub confirm: &'static str,
    /// Part of the connection (its address, the symbol fetched, how often): a change makes a
    /// live source connect again. Any other change decodes the newest batch again, without a
    /// fetch.
    pub connection: bool,
}

impl SettingSpec {
    pub const fn new(
        key: &'static str,
        label: &'static str,
        kind: SettingKind,
        default: &'static str,
        help: &'static str,
    ) -> SettingSpec {
        SettingSpec {
            key,
            label,
            kind,
            default,
            help,
            group: "",
            placeholder: "",
            owned_by_lines: false,
            confirm: "",
            connection: false,
        }
    }

    pub const fn group(mut self, group: &'static str) -> SettingSpec {
        self.group = group;
        self
    }

    pub const fn placeholder(mut self, placeholder: &'static str) -> SettingSpec {
        self.placeholder = placeholder;
        self
    }

    pub const fn owned_by_lines(mut self) -> SettingSpec {
        self.owned_by_lines = true;
        self
    }

    pub const fn confirm(mut self, question: &'static str) -> SettingSpec {
        self.confirm = question;
        self
    }

    pub const fn connection(mut self) -> SettingSpec {
        self.connection = true;
        self
    }

    /// Whether Lines shows it (not a secret, not one Lines' own controls do).
    pub fn shown(&self) -> bool {
        !self.owned_by_lines && self.kind != SettingKind::Secret
    }
}

/// What a setting is, and so its control and its checks (a closed set).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// A text field.
    Text,
    /// A text field holding an `http`/`https` URL (checked as one).
    Url,
    /// Never stored in workspace files or recordings, never displayed; the adapter reads it
    /// from the OS keychain or its environment variable itself.
    Secret,
    /// A switch: `"true"` or `"false"`.
    Bool,
    /// Whole seconds in `[min, max]`, shown in the unit that reads best ("90 s", "23 h") and
    /// stepped in it.
    Seconds { min: u32, max: u32 },
    /// One of these values (the static list; [`LevelSource::choices`] may replace it with what
    /// the source knows now). A value's label is its text, an initial capital.
    Choice(&'static [&'static str]),
    /// A decimal in `[min, max]`, counted in steps of `10^-decimals` (exact: `min`, `max` and
    /// `step` are in those units, so 0.05 with 2 decimals is `5`), shown with `decimals`
    /// places and `unit` after it ("pt"), stepped by `step`. `optional`: empty is allowed
    /// (shown as the spec's placeholder).
    Number {
        min: i64,
        max: i64,
        step: i64,
        decimals: u8,
        unit: &'static str,
        optional: bool,
    },
}

/// Setting values by key (secrets excluded).
pub type SettingValues = BTreeMap<String, String>;

/// What a source is asked to serve.
#[derive(Clone, Debug)]
pub struct LevelContext {
    pub product: Product,
    /// The chart's contract (`None`: unknown, e.g. an old recording).
    pub contract: Option<ContractKey>,
    pub settings: SettingValues,
}

impl LevelContext {
    /// A setting's value, else its declared default.
    pub fn setting<'a>(&'a self, spec: &'a SettingSpec) -> &'a str {
        self.settings
            .get(spec.key)
            .map_or(spec.default, String::as_str)
    }
}

/// The chart's market data, for decoding (anchor translation: decision 0022). Also exported
/// as [`PriceAtTime`].
pub trait ChartPrices: Send + Sync {
    /// The chart's price at `t` (`lines_core::levels::price_at`): `None` when the chart's
    /// data doesn't cover `t`. Translation must then be withheld.
    fn price_at(&self, t: UtcMillis, tolerance_ms: i64) -> Option<Price>;
    /// The chart's market clock (the replay clock in a replay), if it has data.
    fn now(&self) -> Option<UtcMillis>;
}

/// The price-at-time interface adapters translate with (decision 0022): the same trait as
/// [`ChartPrices`], under the name adapters use. The helper itself is public too:
/// [`price_at`] (`lines_core::levels::price_at`).
pub use self::ChartPrices as PriceAtTime;
pub use lines_core::levels::price_at;

/// [`ChartPrices`] over a chart's store: its bars (history + live, stitched) and its market
/// clock. Locks the store briefly; used on the levels thread only.
pub struct StorePrices(pub Arc<crate::store::ChartStore>);

impl ChartPrices for StorePrices {
    fn price_at(&self, t: UtcMillis, tolerance_ms: i64) -> Option<Price> {
        let d = self.0.read();
        lines_core::levels::price_at(d.series.bars(), t, tolerance_ms)
    }
    fn now(&self) -> Option<UtcMillis> {
        self.0.read().market_time
    }
}

/// One response or event, as recorded: a topic (the request path or event name; never a
/// secret, never a query string carrying one) and the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelFrame {
    pub topic: String,
    pub body: Bytes,
}

impl LevelFrame {
    pub fn new(topic: impl Into<String>, body: impl Into<Bytes>) -> LevelFrame {
        LevelFrame {
            topic: topic.into(),
            body: body.into(),
        }
    }
}

/// A source's connection, opened by [`LevelSource::connect`]. Methods must be cancel-safe.
pub trait LevelTransport: Send {
    /// Poll and one-shot sources: perform one refresh and return its responses.
    fn fetch(&mut self) -> BoxFuture<'_, Result<Vec<LevelFrame>>>;
    /// SSE sources: the next event; `Ok(None)` ends the stream (the runtime reconnects).
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<LevelFrame>>> {
        Box::pin(std::future::pending())
    }
}

/// A lower-pane study a level source offers: its lines (each a [`Series`] key of the source's
/// maps) drawn in a pane under the chart, with a shared time axis and crosshair.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeriesStudyDef {
    /// Stable id within the source (lowercase letters, digits, `-`): saved with the chart.
    pub id: &'static str,
    /// Its name in menus and its pane's title.
    pub name: &'static str,
    pub lines: &'static [SeriesLineDef],
    /// Draw a dashed zero line (a value that swings around zero).
    pub zero_line: bool,
    /// Fresh data stopped: when the newest report is older than this (on the source's clock),
    /// the pane says STALE (the lines already end where the reports did).
    pub stale_after_secs: u32,
}

/// One line of a [`SeriesStudyDef`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeriesLineDef {
    /// The [`Series`] key in the source's maps.
    pub key: &'static str,
    /// Its short name in the pane's title and the style editor.
    pub label: &'static str,
    /// Its default color: a theme role, so it reads in every theme and palette.
    pub color: SeriesColor,
    /// Its default line type: each line of a study differs by pattern, never by color alone.
    pub line: lines_core::style::LineType,
}

/// A series line's default color, as a theme role (the app's `Theme::series`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SeriesColor {
    /// The theme's strongest foreground: white on dark, near-black on light.
    Foreground,
    Orange,
    Blue,
    Amber,
}

/// A source of external levels (decision 0022). Implemented by adapters outside this repo
/// (and by the synthetic [`demo`] source in tests and `demo-levels` builds).
pub trait LevelSource: Send + Sync + 'static {
    /// Stable id: lowercase letters, digits, `-`. Persisted with panes, stored in recordings.
    fn id(&self) -> &'static str;
    /// Short display name (legend, menus).
    fn label(&self) -> &'static str;
    /// Whether this source has levels for `product`. Called on the UI thread when a chart
    /// attaches, and again at render after [`serving_changed`] (never block: answer from what
    /// the source already knows). An answer that changes later is announced with
    /// [`serving_changed`]. Never asked for an option chart: level sources are not attached
    /// to option contracts (the underlying's levels are in another market's prices).
    fn serves(&self, product: &Product) -> bool;
    fn refresh(&self) -> RefreshMode;
    /// A map older than this (since it was last received or confirmed) shows as stale.
    fn stale_after(&self) -> Duration;
    /// A synthetic source whose map doesn't depend on the day (the demo, test fixtures): it
    /// may run in a market replay that has no recorded batches for it. Real sources are not
    /// (the default): in a replay they show only what was recorded, else missing.
    fn synthetic(&self) -> bool {
        false
    }
    fn settings(&self) -> &'static [SettingSpec] {
        &[]
    }
    /// A [`SettingKind::Choice`]'s options now, `(value, label)` (`None`: the spec's own
    /// list), e.g. the symbols a service lists. Asked on the UI thread: answer from what the
    /// source already knows, never by fetching.
    fn choices(&self, key: &str) -> Option<Vec<(String, String)>> {
        let _ = key;
        None
    }
    /// Why these values can't be used together, `(key, reason)` for each refused value (e.g. a
    /// remote address while remote access is off). Each value has already passed its kind's
    /// own checks. A refused value is never applied.
    fn check(&self, values: &SettingValues) -> Vec<(&'static str, String)> {
        let _ = values;
        Vec::new()
    }
    /// A default this source takes from outside Lines (its environment), for a key with no
    /// saved value: Lines shows it as the value, marked "from the environment".
    fn outside_default(&self, key: &str) -> Option<String> {
        let _ = key;
        None
    }
    /// The lower-pane line studies this source offers (its maps carry their [`Series`] by
    /// line key). Each is its own chart study (Studies › Add, on charts this source serves).
    fn series_studies(&self) -> &'static [SeriesStudyDef] {
        &[]
    }
    /// The kinds of level this source emits, each with a label, a group and its recommended
    /// look (decision 0026: the chart's per-type style starts from it). A type's `id` is the
    /// kind slug its levels carry. Kinds not declared (and every kind of a source declaring
    /// none, the default) are drawn as their levels' own style says.
    fn types(&self) -> &'static [LevelType] {
        &[]
    }
    /// What its labels say unless the chart changes it: the name (the default) or the name and
    /// price.
    fn label_mode(&self) -> LabelMode {
        LabelMode::Name
    }
    /// Open a connection (live only; runs on the levels runtime).
    fn connect(&self, ctx: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>>;
    /// Turn one batch of frames into a map. Pure: the same frames, previous map and chart
    /// prices give the same result (replays depend on it). `Ok(None)`: nothing changed (e.g.
    /// a keepalive); the current map is confirmed as current. `Err`: the batch is refused and
    /// shown as an error; the previous map stays, marked stale.
    fn decode(
        &self,
        ctx: &LevelContext,
        prices: &dyn ChartPrices,
        prev: Option<&LevelMap>,
        frames: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String>;
}

fn table() -> &'static RwLock<Vec<Arc<dyn LevelSource>>> {
    static T: OnceLock<RwLock<Vec<Arc<dyn LevelSource>>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Register a level source for this process (official builds: through the extension
/// point's `Extensions`, decision 0020). The same id again replaces it.
pub fn register(source: Arc<dyn LevelSource>) -> Result<()> {
    let id = source.id();
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        bail!("level source id {id:?}: use 1-32 lowercase letters, digits and '-'");
    }
    {
        let mut t = table().write();
        t.retain(|s| s.id() != id);
        t.push(source);
    }
    // Charts opened before it was registered ask again.
    serving_changed();
    Ok(())
}

/// Every registered source, in registration order.
pub fn registered() -> Vec<Arc<dyn LevelSource>> {
    table().read().clone()
}

/// The registered sources that serve `product`.
pub fn serving(product: &Product) -> Vec<Arc<dyn LevelSource>> {
    table()
        .read()
        .iter()
        .filter(|s| s.serves(product))
        .cloned()
        .collect()
}

static SERVING_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Announce that a source's [`LevelSource::serves`] answers changed (e.g. it learned the
/// products it serves from its service after charts were opened): first update what `serves`
/// returns, then call this. Charts ask every source again ([`serving`]) and add or remove its
/// layer; a source that doesn't serve a chart's product shows nothing there, not even a
/// legend row. Only live runs ([`runtime::is_live`]) may change their answers this way.
/// Cheap: call it only when an answer changed.
pub fn serving_changed() {
    SERVING_GENERATION.fetch_add(1, Ordering::Release);
}

/// Bumped by every [`serving_changed`]. A chart that attached at an older generation asks
/// [`serving`] again.
pub fn serving_generation() -> u64 {
    SERVING_GENERATION.load(Ordering::Acquire)
}

pub fn by_id(id: &str) -> Option<Arc<dyn LevelSource>> {
    table().read().iter().find(|s| s.id() == id).cloned()
}

/// Recorded batch encoding: per frame `u32 topic len | topic | u32 body len | body` (LE).
pub fn encode_batch(frames: &[LevelFrame], out: &mut Vec<u8>) {
    for f in frames {
        out.extend_from_slice(&(f.topic.len() as u32).to_le_bytes());
        out.extend_from_slice(f.topic.as_bytes());
        out.extend_from_slice(&(f.body.len() as u32).to_le_bytes());
        out.extend_from_slice(&f.body);
    }
}

/// Inverse of [`encode_batch`]; malformed input is an error, never a panic.
pub fn decode_batch(mut b: &[u8]) -> Result<Vec<LevelFrame>, String> {
    let mut out = Vec::new();
    let take = |b: &mut &[u8]| -> Result<Vec<u8>, String> {
        let n = b
            .get(..4)
            .ok_or("truncated length")?
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| "bad length")? as usize;
        let v = b.get(4..4 + n).ok_or("truncated field")?.to_vec();
        *b = &b[4 + n..];
        Ok(v)
    };
    while !b.is_empty() {
        let topic = String::from_utf8(take(&mut b)?).map_err(|_| "topic is not UTF-8")?;
        let body = take(&mut b)?;
        out.push(LevelFrame::new(topic, body));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
