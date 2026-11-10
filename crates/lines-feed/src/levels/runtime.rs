//! The levels runtime: one task per (source, product, contract) on a dedicated Tokio thread,
//! shared by every chart that shows it ([`acquire`] returns a refcounted [`LevelHandle`];
//! the last one dropped stops the task). The UI reads a [`LevelCell`] with `try_lock` and an
//! atomic revision, like the chart store.
//!
//! Modes ([`set_mode`]):
//! - **Live**: connect, fetch (poll / SSE / one-shot), decode, publish; reconnect with
//!   backoff. With a record directory, every batch is written to
//!   `<dir>/<source>-<PRODUCT>.lfr` (provider `levels`, protocol `lines-levels-1/<source>`).
//! - **Replay**: read that file and decode each batch when the chart's market clock reaches
//!   its recorded time, with the same `decode`. [`replay_settled`] tells screenshot mode when
//!   every due batch has been applied, so screenshots are deterministic.
//! - **Not recorded**: a market replay without level recordings. Nothing is fetched; every
//!   source is missing, [`NOT_RECORDED`].
//!
//! A market replay never runs a source live ([`mode_for`]): today's live map shown against a
//! replay's clock would claim to be the levels of that day. A source with no recorded
//! batches for the replay is missing ("not recorded for this replay") and draws nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use lines_core::{ContractKey, ProductId, UtcMillis};
use parking_lot::{Mutex, MutexGuard, RwLock};

use super::{
    ChartPrices, LevelContext, LevelFrame, LevelMap, LevelSource, RefreshMode, SettingValues,
    decode_batch, encode_batch,
};
use crate::provider::Frame;
use crate::recording::{Direction, FORMAT_VERSION, Header, Reader, Writer};

/// Recording provider id and protocol prefix for level batches.
pub const PROVIDER: &str = "levels";
pub const PROTOCOL: &str = "lines-levels-1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LevelMode {
    #[default]
    Live,
    /// Live, recording every batch into this directory.
    Record(PathBuf),
    /// Replay batches recorded in this directory.
    Replay(PathBuf),
    /// A market replay with no level recordings: nothing is fetched, every source is
    /// missing ([`NOT_RECORDED`]).
    NotRecorded,
}

impl LevelMode {
    /// Live data (recorded or not): sources may ask their services what they serve. In a
    /// replay they may not: what a replay shows must not depend on a service running now.
    pub fn is_live(&self) -> bool {
        matches!(self, LevelMode::Live | LevelMode::Record(_))
    }

    /// Replay with a batch file for `source` and `product` ([`replay_recorded`]).
    pub fn recorded(&self, source: &str, product: &str) -> bool {
        match self {
            LevelMode::Replay(dir) => recording_path(dir, source, product).is_file(),
            _ => false,
        }
    }
}

/// Whether this run's levels are live ([`LevelMode::is_live`]). A source whose
/// `LevelSource::serves` depends on what its service lists answers from its baseline
/// otherwise, and never asks the service.
pub fn is_live() -> bool {
    mode().read().is_live()
}

/// Why a source shows as missing in a replay that has no batches recorded for it.
pub const NOT_RECORDED: &str = "not recorded for this replay";

/// The levels mode for a run. `market_replay`: the chart data is a replay (a recording,
/// a stored session, screenshots). A replay only ever replays recorded levels (or none);
/// live fetching and recording are for live market data only.
pub fn mode_for(
    market_replay: bool,
    record: Option<PathBuf>,
    replay_levels: Option<PathBuf>,
) -> LevelMode {
    match (market_replay, record, replay_levels) {
        (true, _, Some(dir)) => LevelMode::Replay(dir),
        (true, _, None) => LevelMode::NotRecorded,
        (false, Some(dir), _) => LevelMode::Record(dir),
        // Recorded levels without a market replay have no clock to follow: live.
        (false, None, _) => LevelMode::Live,
    }
}

fn mode() -> &'static RwLock<LevelMode> {
    static M: OnceLock<RwLock<LevelMode>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Set before charts acquire sources (start-up).
pub fn set_mode(m: LevelMode) {
    *mode().write() = m;
}

fn settings() -> &'static RwLock<HashMap<String, SettingValues>> {
    static S: OnceLock<RwLock<HashMap<String, SettingValues>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Setting values for a source (until the settings window, decision 0021). Never secrets.
pub fn set_settings(source: &str, values: SettingValues) {
    settings().write().insert(source.to_owned(), values);
}

pub fn settings_for(source: &str) -> SettingValues {
    settings().read().get(source).cloned().unwrap_or_default()
}

/// A map's freshness, for display (docs/design.md §7).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LevelStatus {
    /// Nothing received yet (or only errors): show as missing.
    Missing,
    Current,
    /// The map is older than the source's stale threshold, or the last refresh failed.
    Stale {
        age_ms: i64,
    },
}

#[derive(Clone, Debug)]
pub struct LevelState {
    pub source: &'static str,
    pub label: &'static str,
    pub map: Option<Arc<LevelMap>>,
    /// When the map was last received or confirmed: the wall clock live, the recorded time
    /// in a replay.
    pub received: Option<UtcMillis>,
    /// The last refresh's error, if it failed.
    pub error: Option<String>,
    pub stale_after: Duration,
    pub replay: bool,
}

impl LevelState {
    /// Freshness at `now` (wall clock live; the chart's market clock in a replay).
    pub fn status(&self, now: UtcMillis) -> LevelStatus {
        let Some(received) = self.received.filter(|_| self.map.is_some()) else {
            return LevelStatus::Missing;
        };
        let age_ms = (now.0 - received.0).max(0);
        if self.error.is_some() || age_ms > self.stale_after.as_millis() as i64 {
            LevelStatus::Stale { age_ms }
        } else {
            LevelStatus::Current
        }
    }
}

pub struct LevelCell {
    state: Mutex<LevelState>,
    revision: AtomicU64,
}

impl LevelCell {
    #[cfg(test)]
    pub(crate) fn for_test(source: &dyn LevelSource, replay: bool) -> LevelCell {
        LevelCell::new(source, replay)
    }

    fn new(source: &dyn LevelSource, replay: bool) -> LevelCell {
        LevelCell {
            state: Mutex::new(LevelState {
                source: source.id(),
                label: source.label(),
                map: None,
                received: None,
                error: None,
                stale_after: source.stale_after(),
                replay,
            }),
            revision: AtomicU64::new(0),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// UI side: never blocks.
    pub fn try_read(&self) -> Option<MutexGuard<'_, LevelState>> {
        self.state.try_lock()
    }

    pub fn read(&self) -> MutexGuard<'_, LevelState> {
        self.state.lock()
    }

    fn update(&self, f: impl FnOnce(&mut LevelState)) {
        f(&mut self.state.lock());
        self.revision.fetch_add(1, Ordering::Release);
    }
}

type Key = (String, ProductId, Option<ContractKey>);

struct Entry {
    key: Key,
    cell: Arc<LevelCell>,
    task: tokio::task::JoinHandle<()>,
    /// Replay: time of the next batch not yet applied (`i64::MAX`: none); live: `i64::MAX`.
    next_due: Arc<AtomicI64>,
    loaded: Arc<AtomicBool>,
    /// The chart's clock (replays deliver batches by it).
    prices: Arc<dyn ChartPrices>,
    /// Live: where its batches are recorded now (set and cleared by [`record_into`]).
    rec: Shared<Recorder>,
    /// Live: its newest batch and when it came (a recording started later begins with it).
    last: Shared<(UtcMillis, Vec<LevelFrame>)>,
    source_id: String,
    header: Header,
}

pub(crate) type Shared<T> = Arc<parking_lot::Mutex<Option<T>>>;

/// A live source's recorder slot, recording into `dir` from its next batch (`None`: empty).
/// `pinned`: `--record-levels` from launch, which the app's Start and Stop leave alone.
pub(crate) fn recorder_slot(
    source: &dyn LevelSource,
    ctx: &LevelContext,
    dir: Option<PathBuf>,
    pinned: bool,
) -> Shared<Recorder> {
    Arc::new(parking_lot::Mutex::new(dir.map(|d| Recorder {
        path: recording_path(&d, source.id(), ctx.product.symbol),
        header: header(source, ctx),
        writer: None,
        pinned,
    })))
}

/// Start recording a running live source into `dir` (`None`: stop): its newest batch first,
/// at the time it came, then every later batch (`live` writes into `rec`).
pub(crate) fn begin(
    rec: &Shared<Recorder>,
    last: &Shared<(UtcMillis, Vec<LevelFrame>)>,
    dir: Option<&std::path::Path>,
    source_id: &str,
    header: &Header,
) {
    let mut slot = rec.lock();
    // `--record-levels` records from launch to its own folder: the app's recording never
    // takes it over or ends it.
    if slot.as_ref().is_some_and(|r| r.pinned) {
        return;
    }
    *slot = dir.map(|d| {
        let mut r = Recorder {
            path: recording_path(d, source_id, &header.product),
            header: header.clone(),
            writer: None,
            pinned: false,
        };
        if let Some((t, frames)) = last.lock().as_ref() {
            r.write(*t, frames);
        }
        r
    });
}

/// The directory live levels are recorded into now ("Start recording" in the app; `None`:
/// not recording). Charts that acquire a source while it is set record too.
fn record_dir() -> &'static RwLock<Option<PathBuf>> {
    static D: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// Start (`Some(dir)`) or stop (`None`) recording every live level source into `dir`, one
/// file per source and product as `--record-levels` writes ([`recording_path`]). Each starts
/// with the source's newest batch (so a replay has the map from its first moment, even a
/// one-shot source's), then every later batch. Never blocks: the files are written on the
/// levels runtime. Replayed sources are never recorded.
pub fn record_into(dir: Option<PathBuf>) {
    *record_dir().write() = dir.clone();
    let held: Vec<Arc<Entry>> = entries()
        .lock()
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    rt().spawn(async move {
        for e in held {
            if e.cell.read().replay {
                continue;
            }
            begin(&e.rec, &e.last, dir.as_deref(), &e.source_id, &e.header);
        }
    });
}

/// Whether live levels are being recorded ([`record_into`]).
pub fn recording() -> bool {
    record_dir().read().is_some()
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.task.abort();
        // Only our own (now dead) registration: a newer entry may hold the key already.
        let mut map = entries().lock();
        if map.get(&self.key).is_some_and(|w| w.strong_count() == 0) {
            map.remove(&self.key);
        }
    }
}

fn entries() -> &'static Mutex<HashMap<Key, Weak<Entry>>> {
    static E: OnceLock<Mutex<HashMap<Key, Weak<Entry>>>> = OnceLock::new();
    E.get_or_init(Default::default)
}

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("lines-levels")
            .enable_all()
            .build()
            .expect("levels runtime")
    })
}

/// A chart's hold on one source's maps for its product. Clone to share; the task stops when
/// the last handle is dropped.
#[derive(Clone)]
pub struct LevelHandle {
    entry: Arc<Entry>,
}

impl LevelHandle {
    pub fn cell(&self) -> &LevelCell {
        &self.entry.cell
    }

    pub fn source(&self) -> &str {
        &self.entry.key.0
    }

    /// The product and contract this handle's task serves.
    pub fn market(&self) -> (ProductId, Option<ContractKey>) {
        (self.entry.key.1, self.entry.key.2)
    }
}

/// Whether this run replays recorded levels (`LevelMode::Replay`) and has a batch file for
/// `source` and the chart's `product` (the [`recording_path`] the runtime would read, named
/// by the chart's own symbol: an MES chart's is `-MES.lfr`). Sources whose `serves` depends
/// on what a live service lists answer this in a replay, so a replay shows exactly what was
/// recorded, with no service. A file check: fine from `serves` (when a chart attaches, and
/// on the rare render after `serving_changed`), never per frame.
pub fn replay_recorded(source: &str, product: &lines_core::Product) -> bool {
    mode().read().recorded(source, product.symbol)
}

/// The `.lfr` path of a source's batches for a product in `dir`.
pub fn recording_path(dir: &std::path::Path, source: &str, product: &str) -> PathBuf {
    let product: String = product
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    dir.join(format!("{source}-{product}.lfr"))
}

/// Start (or join) the task serving `source` for the chart's product and contract.
/// `prices` is the chart's market data (anchor translation, replay clock).
pub fn acquire(
    source: Arc<dyn LevelSource>,
    ctx: LevelContext,
    prices: Arc<dyn ChartPrices>,
) -> LevelHandle {
    let key = (source.id().to_owned(), ctx.product.id, ctx.contract);
    let mut map = entries().lock();
    if let Some(e) = map.get(&key).and_then(Weak::upgrade) {
        return LevelHandle { entry: e };
    }
    let m = match mode().read().clone() {
        // A synthetic source has no day to be wrong about: it runs as live in a replay.
        LevelMode::NotRecorded if source.synthetic() => LevelMode::Live,
        m => m,
    };
    let replaying = matches!(m, LevelMode::Replay(_) | LevelMode::NotRecorded);
    let cell = Arc::new(LevelCell::new(&*source, replaying));
    // Recording live levels: `--record-levels` from the start, or the app's recording now.
    let hdr = header(&*source, &ctx);
    let dir = match &m {
        LevelMode::Record(d) => Some(d.clone()),
        LevelMode::Live => record_dir().read().clone(),
        _ => None,
    };
    let rec = recorder_slot(&*source, &ctx, dir, matches!(m, LevelMode::Record(_)));
    let last: Shared<(UtcMillis, Vec<LevelFrame>)> = Default::default();
    let source_id = source.id().to_owned();
    let next_due = Arc::new(AtomicI64::new(i64::MAX));
    let loaded = Arc::new(AtomicBool::new(!replaying));
    let clock = prices.clone();
    let task = {
        let (cell, next_due, loaded) = (cell.clone(), next_due.clone(), loaded.clone());
        let (rec, last) = (rec.clone(), last.clone());
        rt().spawn(async move {
            match m {
                LevelMode::Live | LevelMode::Record(_) => {
                    live(source, ctx, prices, cell, rec, last).await
                }
                LevelMode::Replay(dir) => {
                    replay(source, ctx, prices, cell, Some(dir), next_due, loaded).await
                }
                LevelMode::NotRecorded => {
                    replay(source, ctx, prices, cell, None, next_due, loaded).await
                }
            }
        })
    };
    let entry = Arc::new(Entry {
        key: key.clone(),
        cell,
        task,
        next_due,
        loaded,
        prices: clock,
        rec,
        last,
        source_id,
        header: hdr,
    });
    map.insert(key, Arc::downgrade(&entry));
    LevelHandle { entry }
}

/// Whether every source has caught up: a replay has applied each batch due by its chart's
/// clock; a live source has received its first answer (a map or an error). Screenshot mode
/// waits for this so its frame is deterministic.
pub fn settled() -> bool {
    // Upgrade under the lock, look (and drop) outside it: dropping the last handle locks.
    let held: Vec<Arc<Entry>> = entries()
        .lock()
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    held.iter().all(|e| {
        let s = e.cell.read();
        if s.replay {
            let now = e.prices.now().map_or(i64::MIN, |t| t.0);
            e.loaded.load(Ordering::Acquire) && e.next_due.load(Ordering::Acquire) > now
        } else {
            s.received.is_some() || s.error.is_some()
        }
    })
}

/// Block until [`settled`] (or `timeout`); `false` on timeout.
pub fn wait_settled(timeout: Duration) -> bool {
    let end = std::time::Instant::now() + timeout;
    while !settled() {
        if std::time::Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Decode one batch received at `time` and publish the result.
fn apply(
    source: &dyn LevelSource,
    ctx: &LevelContext,
    prices: &dyn ChartPrices,
    cell: &LevelCell,
    frames: &[LevelFrame],
    time: UtcMillis,
) {
    let prev = cell.read().map.clone();
    let result = source.decode(ctx, prices, prev.as_deref(), frames);
    cell.update(|s| match result {
        Ok(Some(map)) if map.product != ctx.product.id || map.source != source.id() => {
            s.error = Some(format!(
                "map for {}/{:?}, not this chart",
                map.source, map.product
            ));
        }
        Ok(Some(map)) => {
            s.map = Some(Arc::new(map));
            s.received = Some(time);
            s.error = None;
        }
        Ok(None) => {
            if s.map.is_some() {
                s.received = Some(time);
            }
            s.error = None;
        }
        Err(e) => s.error = Some(e),
    });
}

pub(crate) struct Recorder {
    path: PathBuf,
    header: Header,
    writer: Option<Writer<std::io::BufWriter<std::fs::File>>>,
    /// `--record-levels` (see [`recorder_slot`]).
    pinned: bool,
}

impl Recorder {
    fn write(&mut self, time: UtcMillis, frames: &[LevelFrame]) {
        if self.writer.is_none() {
            self.header.start_wall_ms = time.0;
            match Writer::create(&self.path, &self.header) {
                Ok(w) => self.writer = Some(w),
                Err(e) => {
                    tracing::warn!("levels: can't record to {}: {e}", self.path.display());
                    return;
                }
            }
        }
        let mut buf = Vec::new();
        encode_batch(frames, &mut buf);
        let since = Duration::from_millis((time.0 - self.header.start_wall_ms).max(0) as u64);
        // Batches are rare (seconds apart): flush each, so an aborted task loses nothing.
        if let Some(w) = self.writer.as_mut()
            && let Err(e) = w
                .write(Direction::FromServer, since, &Frame::binary(buf))
                .and_then(|()| w.flush())
        {
            tracing::warn!("levels: recording failed: {e}");
        }
    }
}

#[cfg(test)]
pub(crate) fn header_for(source: &dyn LevelSource, ctx: &LevelContext) -> Header {
    header(source, ctx)
}

fn header(source: &dyn LevelSource, ctx: &LevelContext) -> Header {
    Header {
        start_wall_ms: 0,
        provider: PROVIDER.into(),
        protocol: format!("{PROTOCOL}/{}", source.id()),
        product: ctx.product.symbol.into(),
        contract: ctx.contract,
        version: FORMAT_VERSION,
    }
}

pub(crate) async fn live(
    source: Arc<dyn LevelSource>,
    ctx: LevelContext,
    prices: Arc<dyn ChartPrices>,
    cell: Arc<LevelCell>,
    rec: Shared<Recorder>,
    last: Shared<(UtcMillis, Vec<LevelFrame>)>,
) {
    let batch = |frames: &[LevelFrame]| {
        let now = crate::now_utc();
        // Under the recorder's lock, so a recording starting now (`begin`) writes this batch
        // either as its first or from here, never neither.
        let mut slot = rec.lock();
        if let Some(r) = slot.as_mut() {
            r.write(now, frames);
        }
        *last.lock() = Some((now, frames.to_vec()));
        drop(slot);
        apply(&*source, &ctx, &*prices, &cell, frames, now);
    };
    let mut backoff = Duration::from_secs(1);
    let fail = |cell: &LevelCell, e: anyhow::Error| {
        tracing::warn!("levels {}: {e:#}", cell.read().source);
        cell.update(|s| s.error = Some(format!("{e:#}")));
    };
    loop {
        let mut t = match source.connect(&ctx).await {
            Ok(t) => t,
            Err(e) => {
                fail(&cell, e);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        match source.refresh() {
            RefreshMode::OneShot => match t.fetch().await {
                Ok(f) => {
                    batch(&f);
                    // Static: keep it. A one-shot map is never refreshed, so it never ages
                    // past the source's threshold unless the source says it should.
                    std::future::pending::<()>().await;
                }
                Err(e) => fail(&cell, e),
            },
            RefreshMode::Poll(every) => loop {
                match t.fetch().await {
                    Ok(f) => {
                        backoff = Duration::from_secs(1);
                        batch(&f);
                    }
                    Err(e) => {
                        fail(&cell, e);
                        break;
                    }
                }
                tokio::time::sleep(every).await;
            },
            RefreshMode::Sse => loop {
                match t.next_event().await {
                    Ok(Some(f)) => {
                        backoff = Duration::from_secs(1);
                        batch(std::slice::from_ref(&f));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        fail(&cell, e);
                        break;
                    }
                }
            },
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Read a recorded levels file: `(time, frames)` per batch, checked against the source and
/// product.
pub fn read_recording(
    path: &std::path::Path,
    source: &str,
    product: &str,
) -> Result<Vec<(UtcMillis, Vec<LevelFrame>)>, String> {
    let mut r = Reader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let h = r.header.clone();
    if h.provider != PROVIDER || h.protocol != format!("{PROTOCOL}/{source}") {
        return Err(format!(
            "{}: not a {source} levels recording ({} / {})",
            path.display(),
            h.provider,
            h.protocol
        ));
    }
    if h.product != product {
        return Err(format!(
            "{}: levels for {}, not {product}",
            path.display(),
            h.product
        ));
    }
    let mut out = Vec::new();
    while let Some(rec) = r.next_record().map_err(|e| e.to_string())? {
        let t = UtcMillis(h.start_wall_ms + rec.since_start.as_millis() as i64);
        out.push((t, decode_batch(&rec.frame.data)?));
    }
    Ok(out)
}

pub(crate) async fn replay(
    source: Arc<dyn LevelSource>,
    ctx: LevelContext,
    prices: Arc<dyn ChartPrices>,
    cell: Arc<LevelCell>,
    dir: Option<PathBuf>,
    next_due: Arc<AtomicI64>,
    loaded: Arc<AtomicBool>,
) {
    let path = dir.map(|d| recording_path(&d, source.id(), ctx.product.symbol));
    let batches = match path.filter(|p| p.exists()) {
        Some(p) => read_recording(&p, source.id(), ctx.product.symbol),
        None => {
            // Nothing recorded for this source: missing, and never fetched live.
            cell.update(|s| s.error = Some(NOT_RECORDED.into()));
            loaded.store(true, Ordering::Release);
            return;
        }
    };
    let batches = match batches {
        Ok(b) => b,
        Err(e) => {
            cell.update(|s| s.error = Some(e));
            loaded.store(true, Ordering::Release);
            return;
        }
    };
    let due = |i: usize| batches.get(i).map_or(i64::MAX, |b| b.0.0);
    next_due.store(due(0), Ordering::Release);
    loaded.store(true, Ordering::Release);
    let mut i = 0;
    while i < batches.len() {
        if let Some(now) = prices.now() {
            while i < batches.len() && batches[i].0 <= now {
                apply(&*source, &ctx, &*prices, &cell, &batches[i].1, batches[i].0);
                i += 1;
                next_due.store(due(i), Ordering::Release);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
