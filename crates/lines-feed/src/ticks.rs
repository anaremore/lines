//! Tick bars per store (roadmap 9b): one incremental [`TickBars`] builder per (store, count,
//! reset mode), shared by every chart that shows it.
//!
//! **Data: the same set as the order flow.** Builders take exactly the trades the tape, volume
//! at price and footprints take, in the same order: the stored trades the feed's warm start
//! replays from the local store (`ChartData::apply_stored`, `disk::orderflow`), then the live
//! trades the stitch counts (`ChartData::apply`, never one at or before `stored_through`).
//! They keep bars, not trades. A chart open at startup therefore gets the stored sessions'
//! bars as they replay.
//!
//! **A chart asking later** ([`ChartStore::acquire_ticks`]) gets bars for every trade counted
//! so far (`ChartData::counted_n`):
//! - every counted trade still in the tape: built at once from it;
//! - while the warm start still replays: deferred (shown as building), and rebuilt when it
//!   ends ([`ChartStore::end_orderflow_loading`]);
//! - otherwise rebuilt on a helper thread. The counted trades before the tape are read again
//!   from where they came from: first the warm start's stored trades, by the record it kept
//!   (`StoredReplay`: the same segments, the same covered ranges, at or before
//!   `stored_through`, so exactly the same trades in the same order; their total must match,
//!   else the store changed, an evicted day), then the live ones from the [`TickSource`]: a
//!   replay's own file through the same stitch and floor, or the local store's segments, which
//!   hold exactly the trades live counted, each once (the writer stores a frame's counted
//!   trades, `frame_counted`): those after `stored_through`. Either way the source is cut
//!   where the tape's first prints (up to [`MATCH_PRINTS`]) appear in order, and accepted only
//!   if exactly the right number came before (a replayed provider session whose start depends
//!   on the replay's clock, such as a minute cutoff on REST trades, can count a different
//!   set). Then every tape print follows, then the trades
//!   counted while the rebuild ran (held), so a trade is never counted twice. The split is by
//!   position, never by time (feeds deliver trades out of time order);
//! - when the counted trades can't be read again (no source, a count that doesn't match,
//!   stored trades from before this process that the warm start didn't replay, an unreadable
//!   segment): from the tape's oldest print, labelled;
//! - if the helper thread can't be started, or the rebuild panics: the same (never stuck on
//!   "building").
//!
//! A rebuild stops early when no chart holds its builder any more or the builder is replaced.
//! A builder no chart holds stops taking trades at once (dropped on the next trade). The trades
//! held meanwhile are applied on the rebuild's thread in chunks, taking the store's lock only
//! to hand each chunk over and for the last few.
//!
//! **Labels.** Bars whose first session isn't covered from its open say from when they begin
//! ([`TickStatus::From`]: "tick bars from 11-05 09:32 ET"): after trading the 1-minute bars
//! show in that session (continuous bars: after any 1-minute bar), or, when the 1-minute bars
//! don't reach back before that session (nothing to compare), more than [`OPEN_SLACK_MS`] after
//! the open.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lines_core::sessions::SessionKind;
use lines_core::{
    Aggressor, BarKind, BarScale, BarSeries, BarsView, CivilDate, Print, Product, TickBars,
    TickReset, UtcMillis,
};

use crate::disk::segment::SegmentReader;
use crate::disk::{ContractDir, Ranges, partition};
use crate::native::{self, NativeEvent};
use crate::recorded::RecordedEvents;
use crate::store::{ChartData, ChartStore};

/// One trade-built bar series a chart asks for (tick, volume, range or seconds bars).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TickKey {
    pub kind: BarKind,
    pub reset: TickReset,
}

/// Where a store's past counted *live* trades can be read again (the warm start's stored
/// trades are read by their own record, `StoredReplay`).
#[derive(Clone, Debug)]
pub enum TickSource {
    /// A replay: its recording (or stored contract or day), from the first frame.
    Replay(PathBuf),
    /// The local store's directory for the live contract: its writer stores exactly the trades
    /// live counted.
    Stored(ContractDir),
}

/// Counted trades held while a rebuild runs, at most (beyond: the rebuild is abandoned).
const MAX_PENDING: usize = 1_000_000;

/// A stored segment is cut where this many of the tape's first prints appear in it, in order.
pub const MATCH_PRINTS: usize = 64;

/// A session's tick bars that begin this long after its open are labelled as starting late.
pub const OPEN_SLACK_MS: i64 = 60_000;

/// Batches a rebuild reads between checks that a chart still wants it.
const CANCEL_CHECK_BATCHES: u32 = 256;

/// Held trades a landing rebuild applies under the store's lock, at most (more are taken out
/// and applied outside it first).
const LAND_UNDER_LOCK: usize = 4096;

#[derive(Debug)]
struct Slot {
    key: TickKey,
    alive: Weak<()>,
    /// Changes when the builder is replaced, so a stale rebuild never lands.
    generation: u64,
    bars: TickBars,
    /// While a rebuild runs: the trades counted meanwhile.
    pending: Option<Vec<Print>>,
    /// Asked while the warm start replayed stored trades: rebuilt when it ends, taking no
    /// trades until then (`pending` is `Some` and stays empty).
    deferred: bool,
    /// The feed's market time when the chart asked (shown while there are no tick bars).
    asked_at: Option<UtcMillis>,
    /// The last status and what it was computed from: charts ask every paint.
    status: Cell<Option<(StatusInputs, TickStatus)>>,
}

/// What [`TickSet::status`] depends on: the builder's revision and the 1-minute bars' extent.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct StatusInputs {
    revision: u64,
    pending: bool,
    series_len: usize,
    series_trimmed: u64,
    series_first: Option<UtcMillis>,
    series_last: Option<UtcMillis>,
}

impl Slot {
    fn new(
        key: TickKey,
        alive: Weak<()>,
        generation: u64,
        bars: TickBars,
        pending: Option<Vec<Print>>,
        asked_at: Option<UtcMillis>,
    ) -> Slot {
        Slot {
            key,
            alive,
            generation,
            bars,
            pending,
            deferred: false,
            asked_at,
            status: Cell::new(None),
        }
    }
}

/// Where a rebuild's source stops and the tape takes over.
#[derive(Clone, Debug)]
enum Cut {
    /// After this many counted trades (an empty tape: nothing to match).
    Count(u64),
    /// Just before these prints, in this order (the tape's first ones); the caller then checks
    /// how many came before.
    Match(Vec<Print>),
}

/// A rebuild's scratch builder: takes the source's counted trades up to the cut.
#[derive(Debug)]
struct Scratch {
    bars: TickBars,
    cut: Cut,
    taken: u64,
    /// The newest source prints not yet in `bars`.
    ring: VecDeque<Print>,
    done: bool,
}

impl Scratch {
    fn new(bars: TickBars, cut: Cut) -> Scratch {
        let (done, ring) = match &cut {
            Cut::Count(k) => (*k == 0, 0),
            Cut::Match(want) => (want.is_empty(), want.len() + 1),
        };
        Scratch {
            bars,
            cut,
            taken: 0,
            ring: VecDeque::with_capacity(ring),
            done,
        }
    }

    fn push(&mut self, p: Print) {
        if self.done {
            return;
        }
        match &self.cut {
            Cut::Count(k) => {
                self.bars.push(p.time, p.price, p.qty, p.aggressor);
                self.taken += 1;
                self.done = self.taken >= *k;
            }
            Cut::Match(want) => {
                self.ring.push_back(p);
                if self.ring.len() > want.len()
                    && let Some(q) = self.ring.pop_front()
                {
                    self.bars.push(q.time, q.price, q.qty, q.aggressor);
                    self.taken += 1;
                }
                self.done = self.ring.len() == want.len() && self.ring.iter().eq(want.iter());
            }
        }
    }
}

/// A store's tick-bar builders.
#[derive(Debug, Default)]
pub struct TickSet {
    slots: Vec<Slot>,
    generation: u64,
    /// A rebuild's scratch copy: its one builder, taking trades only up to the cut.
    scratch: Option<Box<Scratch>>,
}

/// What a chart says about its tick bars.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TickStatus {
    /// Rebuilding from the source.
    Building,
    /// Bars begin here, after their session's open (or after trading the 1-minute bars
    /// show): earlier trading has no tick bars.
    From(UtcMillis),
    /// Nothing to say.
    Complete,
}

impl TickStatus {
    /// The text: "building tick bars", "tick bars from 11-05 09:32 ET" ([`write_from`]).
    pub fn label(&self, zone: &lines_core::display_tz::DisplayTz) -> Option<String> {
        match *self {
            TickStatus::Building => Some("building tick bars".into()),
            TickStatus::From(t) => {
                let mut s = String::new();
                let _ = write_from(t, zone, &mut s);
                Some(s)
            }
            TickStatus::Complete => None,
        }
    }
}

/// "tick bars from 11-05 09:32 ET": date and zone on the display zone's clock, as the
/// crosshair's time tag gives them (the bars may begin on an earlier day than the ones in
/// view). Allocation-free.
pub fn write_from(
    t: UtcMillis,
    zone: &lines_core::display_tz::DisplayTz,
    out: &mut impl std::fmt::Write,
) -> std::fmt::Result {
    let (_, mo, d, h, m, _) = zone.civil(t);
    write!(out, "tick bars from {mo:02}-{d:02} {h:02}:{m:02} ")?;
    zone.write_label(t, out)
}

impl TickSet {
    /// One counted trade, into every builder.
    pub(crate) fn push(&mut self, p: Print) {
        if let Some(sc) = &mut self.scratch {
            sc.push(p);
            return;
        }
        // A builder no chart holds any more stops now, not at the next chart's request.
        if self.slots.iter().any(|s| s.alive.strong_count() == 0) {
            self.prune();
        }
        for s in &mut self.slots {
            match &mut s.pending {
                // Rebuilt from every counted trade once the warm start ends.
                Some(_) if s.deferred => {}
                Some(v) if v.len() < MAX_PENDING => v.push(p),
                Some(_) => {
                    // Too long a rebuild: give it up and build from here (labelled: the
                    // bars start late).
                    s.pending = None;
                    s.generation = 0;
                    s.bars.clear();
                    s.bars.push(p.time, p.price, p.qty, p.aggressor);
                }
                None => s.bars.push(p.time, p.price, p.qty, p.aggressor),
            }
        }
    }

    /// A contract switch: every builder starts over (a running rebuild is dropped).
    pub(crate) fn clear(&mut self) {
        for s in &mut self.slots {
            s.bars.clear();
            s.pending = None;
            s.deferred = false;
            s.generation = 0;
        }
    }

    fn prune(&mut self) {
        self.slots.retain(|s| s.alive.strong_count() > 0);
    }

    fn slot(&self, key: TickKey) -> Option<&Slot> {
        self.slots.iter().find(|s| s.key == key)
    }

    /// Whether a rebuild of `key` at `generation` is still wanted (held, not replaced).
    fn wanted(&self, key: TickKey, generation: u64) -> bool {
        self.slots.iter().any(|s| {
            s.key == key
                && s.generation == generation
                && s.pending.is_some()
                && s.alive.strong_count() > 0
        })
    }

    /// The bars for `key` (empty while it rebuilds or if no chart asked).
    pub fn view(&self, key: TickKey) -> BarsView<'_> {
        match self.slot(key) {
            Some(s) if s.pending.is_none() => s.bars.view(),
            _ => BarsView::fixed(&[], 60_000),
        }
    }

    /// The builder for `key`, unless it is rebuilding.
    pub fn bars(&self, key: TickKey) -> Option<&TickBars> {
        self.slot(key)
            .filter(|s| s.pending.is_none())
            .map(|s| &s.bars)
    }

    /// Bars dropped from the front of `key`'s series so far.
    pub fn trimmed(&self, key: TickKey) -> u64 {
        self.slot(key).map_or(0, |s| s.bars.trimmed())
    }

    /// Whether `key`'s bars begin after trading that `series` (the 1-minute bars) shows in
    /// their first session (continuous bars: anywhere before), or, when the 1-minute bars don't
    /// reach back before that session (nothing to compare), more than [`OPEN_SLACK_MS`] after
    /// its open: then the chart says from when it has tick bars (at the first bar). Cached per builder revision and 1-minute extent
    /// (charts ask every paint; the calendar lookup runs only on a change).
    pub fn status(&self, key: TickKey, series: &BarSeries, product: &Product) -> TickStatus {
        let Some(s) = self.slot(key) else {
            return TickStatus::Complete;
        };
        let inputs = StatusInputs {
            revision: s.bars.revision(),
            pending: s.pending.is_some(),
            series_len: series.len(),
            series_trimmed: series.trimmed(),
            series_first: series.bars().first().map(|b| b.time),
            series_last: series.bars().last().map(|b| b.time),
        };
        if let Some((was, st)) = s.status.get()
            && was == inputs
        {
            return st;
        }
        let st = slot_status(s, key, series, product);
        s.status.set(Some((inputs, st)));
        st
    }
}

/// [`TickSet::status`], uncached.
fn slot_status(s: &Slot, key: TickKey, series: &BarSeries, product: &Product) -> TickStatus {
    if s.pending.is_some() {
        return TickStatus::Building;
    }
    // The first bar; with none yet, from when the chart asked, else the newest 1-minute bar's
    // end.
    let start = s
        .bars
        .bars()
        .first()
        .map(|b| b.time)
        .or(s.asked_at)
        .or_else(|| {
            let b = series.bars().last()?;
            Some(UtcMillis(b.time.0 + series.period_ms()))
        });
    let Some(start) = start else {
        return TickStatus::Complete;
    };
    let session_open = crate::catalog::sessions(product.id)
        .and_then(|c| c.next_session(start, SessionKind::Eth))
        .map_or_else(|| start.floor_to(86_400_000), |s| s.open.min(start));
    let since = match key.reset {
        TickReset::Session => session_open,
        TickReset::Continuous => UtcMillis(i64::MIN),
    };
    let minute = start.floor_to(UtcMillis::MINUTE);
    let bars = series.bars();
    let k = bars.partition_point(|b| b.time < since);
    // The 1-minute bars reach back before this session: they show whether the session traded
    // before the tick bars begin. (Bars that begin with the tick bars show nothing.)
    let comparable = bars.first().is_some_and(|b| b.time < session_open);
    match bars.get(k) {
        // A whole earlier minute has bars: trading the tick bars don't have.
        Some(b) if b.time < minute => TickStatus::From(start),
        // Nothing earlier: a thin opening is not a late start.
        Some(_) if comparable => TickStatus::Complete,
        // Nothing to compare against: late only past the slack.
        _ if start.0 - session_open.0 > OPEN_SLACK_MS => TickStatus::From(start),
        _ => TickStatus::Complete,
    }
}

/// A chart's hold on a tick-bar series: the builder lives while any lease does.
#[derive(Debug)]
pub struct TickLease {
    key: TickKey,
    _alive: Arc<()>,
    rebuild: Option<JoinHandle<()>>,
}

impl TickLease {
    pub fn key(&self) -> TickKey {
        self.key
    }

    /// Wait for this lease's rebuild, if it started one (tests and tools).
    pub fn join_rebuild(&mut self) {
        if let Some(h) = self.rebuild.take() {
            let _ = h.join();
        }
    }
}

static REBUILDS: AtomicUsize = AtomicUsize::new(0);

/// Wait until no tick-bar rebuild runs (screenshots: the frame shows finished bars). `false`
/// if some still ran at the timeout.
pub fn wait_rebuilds(timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while REBUILDS.load(Ordering::Acquire) > 0 {
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// The warm start's stored trades, as a rebuild reads them again (`StoredReplay`).
#[derive(Clone, Debug)]
struct StoredPart {
    dir: ContractDir,
    covered: Ranges,
    /// `stored_through`: every replayed trade is at or before it, every live one after.
    through: i64,
    trades: u64,
}

/// What a rebuild needs, taken under the store's lock.
struct Job {
    key: TickKey,
    generation: u64,
    /// The warm start's stored trades: counted first.
    stored: Option<StoredPart>,
    /// Where the live counted trades after them are read again.
    live: Option<TickSource>,
    product: Product,
    contract: Option<lines_core::ContractKey>,
    capabilities: lines_core::Capabilities,
    provider: &'static str,
    period_ms: i64,
    max_bars: usize,
    /// The tape's prints, oldest first.
    tape: Vec<Print>,
    /// Trades counted before the tape's oldest print.
    before_tape: u64,
}

/// The tape's prints, oldest first.
fn tape_prints(d: &ChartData) -> Vec<Print> {
    let mut v: Vec<Print> = d.tape.iter_newest().copied().collect();
    v.reverse();
    v
}

/// A rebuild of `key` for every trade counted so far, if the ones before the tape can be read
/// again; else the tape back (it holds every counted trade, or the bars start late).
fn job_for(
    d: &ChartData,
    key: TickKey,
    generation: u64,
    tape: Vec<Print>,
) -> Result<Job, Vec<Print>> {
    let Some(before_tape) = d
        .counted_n
        .checked_sub(tape.len() as u64)
        .filter(|n| *n > 0)
    else {
        return Err(tape);
    };
    let stored = match (d.stored.trades, &d.stored.dir, d.stored_through) {
        (0, _, _) => None,
        (trades, Some(dir), Some(through)) => Some(StoredPart {
            dir: dir.clone(),
            covered: d.stored.covered.clone(),
            through,
            trades,
        }),
        // Stored trades applied without their record: they can't be read again.
        _ => return Err(tape),
    };
    if before_tape > d.stored.trades && d.tick_source.is_none() {
        return Err(tape);
    }
    Ok(Job {
        key,
        generation,
        stored,
        live: d.tick_source.clone(),
        product: d.product.clone(),
        contract: d.contract,
        capabilities: d.capabilities,
        provider: d.provider,
        period_ms: d.series.period_ms(),
        max_bars: d.series.max_len(),
        before_tape,
        tape,
    })
}

impl ChartStore {
    /// Tick bars of `key` for a chart: shared with other charts asking for the same; built
    /// as the module docs say. Takes the store's lock briefly (no I/O under it).
    pub fn acquire_ticks(self: &Arc<Self>, key: TickKey) -> TickLease {
        let alive = Arc::new(());
        let mut job = None;
        {
            let mut d = self.read();
            d.ticks.prune();
            if let Some(s) = d.ticks.slots.iter().find(|s| s.key == key)
                && let Some(a) = s.alive.upgrade()
            {
                return TickLease {
                    key,
                    _alive: a,
                    rebuild: None,
                };
            }
            let cal = crate::catalog::sessions(d.product.id);
            let mut bars = TickBars::with_kind(
                key.kind,
                key.reset,
                cal,
                BarScale::of(&d.product),
                d.series.max_len(),
            );
            let tape = tape_prints(&d);
            let all_in_tape = d.counted_n <= tape.len() as u64;
            let mut pending = None;
            let mut deferred = false;
            d.ticks.generation += 1;
            let generation = d.ticks.generation;
            if !all_in_tape && d.orderflow_loading {
                // Rebuilt from every counted trade when the warm start ends.
                pending = Some(Vec::new());
                deferred = true;
            } else {
                match job_for(&d, key, generation, tape) {
                    Ok(j) => {
                        pending = Some(Vec::new());
                        job = Some(j);
                    }
                    // Every counted trade, or (the rest can't be read again) from the tape's
                    // oldest print.
                    Err(tape) => {
                        for p in &tape {
                            bars.push(p.time, p.price, p.qty, p.aggressor);
                        }
                    }
                }
            }
            let asked_at = d.market_time;
            let mut slot = Slot::new(
                key,
                Arc::downgrade(&alive),
                generation,
                bars,
                pending,
                asked_at,
            );
            slot.deferred = deferred;
            d.ticks.slots.push(slot);
        }
        let rebuild = job.and_then(|job| {
            let guard = Rebuilding::new(Arc::downgrade(self), job.key, job.generation);
            spawn_rebuild(guard, job)
        });
        TickLease {
            key,
            _alive: alive,
            rebuild,
        }
    }

    /// The warm start's stored trades are all applied (`orderflow_loading` off): tick charts
    /// that asked meanwhile rebuild now, from every counted trade. Returns the rebuilds'
    /// threads (tests join them; the feed lets them run).
    pub fn end_orderflow_loading(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let store = Arc::downgrade(self);
        let jobs = self.update_derived(|d| {
            d.orderflow_loading = false;
            let mut jobs = Vec::new();
            let keys: Vec<TickKey> = d
                .ticks
                .slots
                .iter()
                .filter(|s| s.deferred && s.alive.strong_count() > 0)
                .map(|s| s.key)
                .collect();
            for key in keys {
                d.ticks.generation += 1;
                let generation = d.ticks.generation;
                let made = job_for(d, key, generation, tape_prints(d));
                let Some(s) = d.ticks.slots.iter_mut().find(|s| s.key == key) else {
                    continue;
                };
                s.deferred = false;
                s.generation = generation;
                s.bars.clear();
                match made {
                    Ok(job) => {
                        s.pending = Some(Vec::new());
                        // Counted from here, so screenshots wait for it.
                        jobs.push((Rebuilding::new(store.clone(), key, generation), job));
                    }
                    Err(tape) => {
                        s.pending = None;
                        for p in tape {
                            s.bars.push(p.time, p.price, p.qty, p.aggressor);
                        }
                    }
                }
                d.chart_epoch += 1;
            }
            jobs
        });
        jobs.into_iter()
            .filter_map(|(guard, job)| spawn_rebuild(guard, job))
            .collect()
    }
}

/// Run `job` on a helper thread, counted in `REBUILDS` by `guard` until it ends; unless the
/// rebuild ran to its end, the builder then falls back to the tape (a thread that never
/// started or panicked).
fn spawn_rebuild(guard: Rebuilding, job: Job) -> Option<JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("lines-tick-rebuild".into())
        .spawn(move || {
            let (key, generation) = (job.key, job.generation);
            let store = guard.store.clone();
            let mut checks = 0u32;
            let cancelled = || {
                checks = checks.wrapping_add(1);
                if !checks.is_multiple_of(CANCEL_CHECK_BATCHES) {
                    return false;
                }
                store
                    .upgrade()
                    .is_none_or(|s| !s.read().ticks.wanted(key, generation))
            };
            // Cancelled (`None`): nobody wants it any more.
            if let Some(bars) = rebuild(&job, cancelled)
                && let Some(store) = store.upgrade()
            {
                land(&store, key, generation, bars);
            }
            guard.finish();
        });
    match spawned {
        Ok(h) => Some(h),
        Err(e) => {
            // The closure and its guard are dropped: the tape fallback ran.
            tracing::warn!("tick bars: no rebuild thread ({e}): from the tape");
            None
        }
    }
}

/// A rebuild in flight, counted in `REBUILDS` until dropped. Dropped before `done` (its thread
/// never started, or panicked), the builder falls back to the tape ([`from_tape`]).
struct Rebuilding {
    store: Weak<ChartStore>,
    key: TickKey,
    generation: u64,
    done: bool,
}

impl Rebuilding {
    fn new(store: Weak<ChartStore>, key: TickKey, generation: u64) -> Rebuilding {
        REBUILDS.fetch_add(1, Ordering::AcqRel);
        Rebuilding {
            store,
            key,
            generation,
            done: false,
        }
    }

    /// The rebuild ran to its end (landed, or cancelled: nobody wants it).
    fn finish(mut self) {
        self.done = true;
    }
}

impl Drop for Rebuilding {
    fn drop(&mut self) {
        if !self.done
            && let Some(store) = self.store.upgrade()
        {
            store.update_derived(|d| from_tape(d, self.key, self.generation));
        }
        REBUILDS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A rebuild that can't run: the builder takes the tape as it is now (the newest of the trades
/// held meanwhile), labelled as starting late like a source that doesn't cover the range.
fn from_tape(d: &mut ChartData, key: TickKey, generation: u64) {
    let Some(s) = d
        .ticks
        .slots
        .iter_mut()
        .find(|s| s.key == key && s.generation == generation && s.pending.is_some())
    else {
        return; // released, replaced or reset meanwhile
    };
    s.pending = None;
    s.bars.clear();
    let mut prints: Vec<Print> = d.tape.iter_newest().copied().collect();
    prints.reverse();
    for p in prints {
        s.bars.push(p.time, p.price, p.qty, p.aggressor);
    }
    d.chart_epoch += 1;
}

/// What [`take_held`] did.
#[derive(Debug)]
enum Held {
    /// Released, replaced or reset meanwhile.
    Gone,
    /// Swapped in, with the last held trades.
    Landed,
    /// Too many held trades to apply under the lock: these, to apply outside it first.
    Chunk(Vec<Print>),
}

/// Swap a finished rebuild in and apply what was counted meanwhile. While many trades are
/// held (a long rebuild holds up to [`MAX_PENDING`]), they are taken out and applied outside
/// the store's lock, so it is held only to hand them over and for the last few.
fn land(store: &ChartStore, key: TickKey, generation: u64, mut bars: TickBars) {
    loop {
        let held =
            store.update_derived(|d| take_held(d, key, generation, &mut bars, LAND_UNDER_LOCK));
        match held {
            Held::Gone | Held::Landed => return,
            Held::Chunk(v) => {
                for p in v {
                    bars.push(p.time, p.price, p.qty, p.aggressor);
                }
            }
        }
    }
}

/// Under the lock: with at most `max` held trades, apply them to `bars` and swap it in (the
/// old, empty builder is left in `bars`); else take the held trades out for the caller.
fn take_held(
    d: &mut ChartData,
    key: TickKey,
    generation: u64,
    bars: &mut TickBars,
    max: usize,
) -> Held {
    let Some(s) = d
        .ticks
        .slots
        .iter_mut()
        .find(|s| s.key == key && s.generation == generation && s.pending.is_some())
    else {
        return Held::Gone;
    };
    let Some(held) = s.pending.as_mut() else {
        return Held::Gone;
    };
    if held.len() > max {
        return Held::Chunk(std::mem::take(held));
    }
    for p in held.drain(..) {
        bars.push(p.time, p.price, p.qty, p.aggressor);
    }
    s.pending = None;
    std::mem::swap(&mut s.bars, bars);
    d.chart_epoch += 1;
    Held::Landed
}

/// Build `job`'s bars: the source's counted trades before the tape, then the tape. Falls back
/// to the tape alone when the source doesn't cover the range; `None` if cancelled.
fn rebuild(job: &Job, cancelled: impl FnMut() -> bool) -> Option<TickBars> {
    let cal = crate::catalog::sessions(job.product.id);
    let tape_only = || {
        let mut b = TickBars::with_kind(
            job.key.kind,
            job.key.reset,
            cal,
            BarScale::of(&job.product),
            job.max_bars,
        );
        for p in &job.tape {
            b.push(p.time, p.price, p.qty, p.aggressor);
        }
        b
    };
    let mut bars = match from_source(job, cancelled) {
        Ok(Read::Done(b)) => *b,
        Ok(Read::Uncovered) => return Some(tape_only()),
        Ok(Read::Cancelled) => return None,
        Err(e) => {
            tracing::warn!("tick bars: rebuild from {:?} failed: {e:#}", job.live);
            return Some(tape_only());
        }
    };
    for p in &job.tape {
        bars.push(p.time, p.price, p.qty, p.aggressor);
    }
    Some(bars)
}

/// How reading a rebuild's source ended.
enum Read {
    /// The source's counted trades before the tape.
    Done(Box<TickBars>),
    /// The source doesn't cover the range, or never reaches the tape.
    Uncovered,
    Cancelled,
}

/// The counted trades before the tape, read again: the warm start's stored trades (all of
/// them must come back, else the store changed), then the live ones from the source, up to
/// the cut.
fn from_source(job: &Job, mut cancelled: impl FnMut() -> bool) -> anyhow::Result<Read> {
    let cal = crate::catalog::sessions(job.product.id);
    let mut bars = TickBars::with_kind(
        job.key.kind,
        job.key.reset,
        cal,
        BarScale::of(&job.product),
        job.max_bars,
    );
    let mut taken = 0u64;
    let through = job.stored.as_ref().map(|s| s.through);
    if let Some(s) = &job.stored {
        let want = job.before_tape.min(s.trades);
        let Some(&(first, _)) = s.covered.ranges().first() else {
            return Ok(Read::Uncovered);
        };
        let first = partition::trade_date(&job.product, UtcMillis(first));
        let last = partition::trade_date(&job.product, UtcMillis(s.through));
        let mut total = 0u64;
        // As `disk::orderflow::read` took them: inside the replayed ranges and the segment's
        // own trade date (all at or before `through`).
        let keep = |(open, close): (i64, i64), t: i64| {
            t <= s.through && (open..close).contains(&t) && s.covered.contains(t)
        };
        let each = |p: Print| {
            total += 1;
            if taken < want {
                bars.push(p.time, p.price, p.qty, p.aggressor);
                taken += 1;
            }
            true
        };
        let pass = read_stored(
            &s.dir,
            &job.product,
            (first, Some(last)),
            keep,
            each,
            &mut cancelled,
        )?;
        if pass == Pass::Cancelled {
            return Ok(Read::Cancelled);
        }
        if total != s.trades {
            // A stored day evicted or changed since the warm start.
            return Ok(Read::Uncovered);
        }
    }
    let rest = job.before_tape - taken;
    if rest == 0 {
        return Ok(Read::Done(Box::new(bars)));
    }
    match &job.live {
        None => Ok(Read::Uncovered),
        Some(TickSource::Replay(path)) => {
            // Through a scratch store: the same stitch and stored floor, so the same counted
            // trades in the same order.
            let mut events = RecordedEvents::open(path)?;
            let mut d = ChartData::new(
                job.product.clone(),
                job.contract,
                job.capabilities,
                job.provider,
                job.period_ms,
                job.max_bars,
            );
            d.stored_through = through;
            // Cut where the tape's first prints appear, and only if exactly `rest` trades came
            // before them: a provider session whose start depends on the replay's clock (a
            // minute cutoff on REST trades) can count a different set, which is then refused.
            let cut = if job.tape.is_empty() {
                Cut::Count(rest)
            } else {
                Cut::Match(job.tape[..job.tape.len().min(MATCH_PRINTS)].to_vec())
            };
            d.ticks.scratch = Some(Box::new(Scratch::new(bars, cut)));
            while !d.ticks.scratch.as_ref().is_some_and(|s| s.done) {
                if cancelled() {
                    return Ok(Read::Cancelled);
                }
                let Some((_, batch)) = events.next_batch()? else {
                    break;
                };
                d.apply(&batch);
            }
            match d.ticks.scratch.take() {
                // The source must reach the tape with exactly the trades live counted before
                // it: else some would be missing or counted twice.
                Some(s) if s.done && s.taken == rest => Ok(Read::Done(Box::new(s.bars))),
                _ => Ok(Read::Uncovered),
            }
        }
        Some(TickSource::Stored(dir)) if dir.contract != job.contract => Ok(Read::Uncovered),
        Some(TickSource::Stored(dir)) => {
            // The live trades are after `through` (no floor without a warm start), so they
            // start on its trade date, else (nothing replayed) on the tape's.
            let Some(t0) = through.or(job.tape.first().map(|p| p.time.0)) else {
                return Ok(Read::Uncovered);
            };
            let first = partition::trade_date(&job.product, UtcMillis(t0));
            let want = job.tape[..job.tape.len().min(MATCH_PRINTS)].to_vec();
            let mut sc = Scratch::new(bars, Cut::Match(want));
            let pass = read_stored(
                dir,
                &job.product,
                (first, None),
                |_, t| through.is_none_or(|s| t > s),
                |p| {
                    sc.push(p);
                    !sc.done
                },
                &mut cancelled,
            )?;
            if pass == Pass::Cancelled {
                return Ok(Read::Cancelled);
            }
            // Exactly the live trades before the tape: more are stored trades from before this
            // process (not replayed at startup), fewer never reached the store.
            if sc.done && sc.taken == rest {
                Ok(Read::Done(Box::new(sc.bars)))
            } else {
                Ok(Read::Uncovered)
            }
        }
    }
}

/// How a [`read_stored`] pass ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Pass {
    /// Every segment read to its end (or to a damaged block).
    End,
    /// `each` asked to stop.
    Stopped,
    Cancelled,
}

/// The stored trades of `dir`'s trade dates from `first` (to `last`, if given), oldest date
/// first and in stored order within one, that `keep(the date's span, ms)` accepts, to `each`
/// (false: stop). A segment ends at its first damaged, partial or undecodable block, as the
/// warm start reads it; an evicted one is skipped.
fn read_stored(
    dir: &ContractDir,
    product: &Product,
    (first, last): (CivilDate, Option<CivilDate>),
    mut keep: impl FnMut((i64, i64), i64) -> bool,
    mut each: impl FnMut(Print) -> bool,
    cancelled: &mut impl FnMut() -> bool,
) -> anyhow::Result<Pass> {
    let mut events = Vec::new();
    for date in dir.dates() {
        if date < first || last.is_some_and(|l| date > l) {
            continue;
        }
        let (open, close) = partition::trade_date_span(product, date);
        let span = (open.0, close.0);
        let mut r = match SegmentReader::open(&dir.segment_path(date)) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        loop {
            if cancelled() {
                return Ok(Pass::Cancelled);
            }
            let Ok(block) = r.next_block()? else {
                break;
            };
            events.clear();
            if native::decode(&block.events, &mut events).is_err() {
                break;
            }
            for e in &events {
                let NativeEvent::Trade {
                    time_us,
                    price,
                    qty,
                    aggressor,
                } = *e
                else {
                    continue;
                };
                let t = time_us.div_euclid(1000);
                if !keep(span, t) {
                    continue;
                }
                let p = Print {
                    time: UtcMillis(t),
                    price,
                    qty,
                    aggressor: match aggressor {
                        native::Aggressor::Buy => Some(Aggressor::Buy),
                        native::Aggressor::Sell => Some(Aggressor::Sell),
                        native::Aggressor::Unknown => None,
                    },
                };
                if !each(p) {
                    return Ok(Pass::Stopped);
                }
            }
        }
    }
    Ok(Pass::End)
}

#[cfg(test)]
mod tests;
