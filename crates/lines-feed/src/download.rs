//! Download past days (decision 0030 §8): a source that can fetch whole past trade dates
//! says so through the extension point (decision 0020), and each day it delivers becomes a
//! normal stored day in the local store (decision 0019, [`crate::disk::day`]).
//!
//! **The seam** ([`PastDays`], versioned by [`API_VERSION`], decision 0030 §10). A source
//! offers content choices ([`Contents`], e.g. "Trades + top of book", "Full depth"), a cost
//! estimate per day ([`DayEstimate`]: included in the plan or an estimated price, plus a size;
//! or the provider's own refusal, [`Refusal`]), and a download that streams Lines events for
//! one contract and trade date into a [`DaySink`] the app gives it. The calls block: they run
//! only on a download thread ([`Job`]), never on the UI thread; a download checks
//! [`DaySink::canceled`] and stops when [`DaySink::frame`] says so. Nothing here names a
//! vendor.
//!
//! **The engine.** [`plan`] says what the store already holds for each date (stored days are
//! never estimated or requested again); [`estimate`] and [`download`] run a [`Job`] on its own
//! thread and report [`Update`]s. A download re-checks each day's price first (free) and never
//! fetches a day whose price is now above the one the person accepted.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lines_core::{CivilDate, ContractKey, Product, UtcMillis};

use crate::disk::ContractDir;
use crate::disk::day::{BeginError, DayState, DayWriter, Written, day_state, session_span};
use crate::native::NativeEvent;

/// The version of the past-days interface this build speaks. An extension built against
/// another version is refused at registration with a clear message, never driven wrong.
pub const API_VERSION: u32 = 1;

/// Most days one request may name.
pub const MAX_DAYS: usize = 93;

/// One content choice a source offers, e.g. "Trades + top of book".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Contents {
    /// Stable id the source understands (`tob`, `depth`).
    pub id: &'static str,
    /// What the dialog shows.
    pub label: &'static str,
}

/// What a day costs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Charge {
    /// Included in the account's plan: nothing to pay.
    Included,
    /// The provider's estimate in millionths of a US dollar, rounded up (never understated).
    Estimated { usd_micros: u64 },
}

impl Charge {
    /// Millionths of a dollar to pay (0 when included).
    pub fn usd_micros(self) -> u64 {
        match self {
            Charge::Included => 0,
            Charge::Estimated { usd_micros } => usd_micros,
        }
    }

    /// From a provider's dollar figure (a float on the wire): rounded *up* to the micro-dollar,
    /// `None` for a negative or non-finite value.
    pub fn from_usd(usd: f64, included: bool) -> Option<Charge> {
        if !usd.is_finite() || usd < 0.0 {
            return None;
        }
        if included {
            return Some(Charge::Included);
        }
        let micros = (usd * 1e6).ceil();
        (micros < u64::MAX as f64).then_some(Charge::Estimated {
            usd_micros: micros as u64,
        })
    }

    /// "Included in your plan" or "Estimated $1.63" (cents rounded up).
    pub fn text(self) -> String {
        match self {
            Charge::Included => "Included in your plan".into(),
            Charge::Estimated { usd_micros } => format!("Estimated {}", usd_text(usd_micros)),
        }
    }
}

/// `$1.63`: millionths of a dollar shown in cents, rounded up so an estimate never reads lower
/// than it is.
pub fn usd_text(usd_micros: u64) -> String {
    let cents = usd_micros.div_ceil(10_000);
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// `58.0 MiB`, `2.07 GiB`, `512 B`.
pub fn size_text(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    match i {
        0 => format!("{bytes} B"),
        _ if v >= 100.0 => format!("{v:.0} {}", U[i]),
        _ if v >= 10.0 => format!("{v:.1} {}", U[i]),
        _ => format!("{v:.2} {}", U[i]),
    }
}

/// One day's estimate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DayEstimate {
    pub charge: Charge,
    /// What the provider says the day weighs (its own measure, e.g. uncompressed).
    pub bytes: u64,
}

/// The provider's own words for why it won't (an entitlement, a bad symbol, a failed login).
/// Shown to the person as they are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One contract and trade date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayRequest {
    pub product: Product,
    pub contract: ContractKey,
    pub date: CivilDate,
    /// The trade date's session `[open, close)`: what to fetch.
    pub open: UtcMillis,
    pub close: UtcMillis,
    /// A [`Contents::id`] the source offered.
    pub contents: &'static str,
}

/// Why a download didn't deliver its day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DayError {
    /// The provider refused (an entitlement, a bad symbol, a failed login): its own words.
    Refused(Refusal),
    /// Something on this side failed (a local file, the conversion, a definition that
    /// disagrees with the catalog, records with no definition): said plainly, not as the
    /// provider's.
    Failed(String),
}

impl From<Refusal> for DayError {
    fn from(r: Refusal) -> Self {
        DayError::Refused(r)
    }
}

/// The sink has stopped taking events (canceled, or the store failed): return at once.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Stop;

/// Where a download delivers one day (given by the app).
pub trait DaySink {
    /// One atomic batch, stamped `time_us` (the provider's receive time, never going back).
    /// `Err(Stop)`: stop downloading and return.
    fn frame(&mut self, time_us: i64, events: &[NativeEvent]) -> Result<(), Stop>;
    /// Progress: bytes received from the provider so far for this day.
    fn received(&mut self, bytes: u64);
    /// The person canceled: stop as soon as possible.
    fn canceled(&self) -> bool;
    /// A private directory for the provider's temporary files (inside the local store, deleted
    /// when the day ends, whatever the outcome).
    fn scratch_dir(&self) -> &Path;
    /// Call right before the first request that may be billed (after any free checks): from
    /// here the day counts as charged, whatever follows.
    fn requesting(&mut self);
    /// Call once everything paid has arrived: the money is spent, so from here a Stop now no
    /// longer stops the day ([`DaySink::frame`] keeps taking events and the day is stored).
    fn fetched(&mut self);
}

/// A source that can download past days.
pub trait PastDays: Send + Sync + 'static {
    /// The [`API_VERSION`] this implementation was built against (return the constant).
    fn api_version(&self) -> u32;
    /// The content choices for a product, the recommended one first; empty = this source can't
    /// download that product. Called on the UI thread: must answer at once (no network, no
    /// disk, no lock that a download holds).
    fn contents(&self, product: &Product) -> Vec<Contents>;
    /// The free estimate for one day. Blocks (network); runs on a download thread.
    /// A refusal is the provider's answer; anything else that went wrong (no key, the
    /// network, an unreadable answer) is `DayError::Failed`.
    fn estimate(&self, req: &DayRequest) -> Result<DayEstimate, DayError>;
    /// Fetch one day and stream it into `sink`. Blocks; runs on a download thread. After
    /// `sink.frame` returns `Err(Stop)` or `sink.canceled()`, return promptly (any error).
    fn download(&self, req: &DayRequest, sink: &mut dyn DaySink) -> Result<(), DayError>;
}

/// Which store and contract a job writes.
#[derive(Clone, Debug)]
pub struct Target {
    /// The store root (contains `store-v1/`).
    pub root: PathBuf,
    /// The source's store directory name (the extension's `--source` name).
    pub source: String,
    pub product: Product,
    pub contract: ContractKey,
}

impl Target {
    pub fn dir(&self) -> ContractDir {
        ContractDir::new(&self.root, &self.source, &self.product, Some(self.contract))
    }

    fn request(&self, date: CivilDate, contents: &'static str) -> Option<DayRequest> {
        let (open, close) = session_span(&self.product, date)?;
        Some(DayRequest {
            product: self.product.clone(),
            contract: self.contract,
            date,
            open,
            close,
            contents,
        })
    }
}

/// Every date from `from` to `to`, inclusive (at most [`MAX_DAYS`]).
pub fn dates(from: CivilDate, to: CivilDate) -> Result<Vec<CivilDate>, String> {
    if to < from {
        return Err("The end date is before the start date.".into());
    }
    let n = (to.days() - from.days() + 1) as usize;
    if n > MAX_DAYS {
        return Err(format!("At most {MAX_DAYS} days at a time."));
    }
    Ok((0..n as i64).map(|i| from.add_days(i)).collect())
}

/// What the store holds for each date, at `now` (no network, no lock).
pub fn plan(target: &Target, dates: &[CivilDate], now: UtcMillis) -> Vec<(CivilDate, DayState)> {
    let dir = target.dir();
    let busy = locked(&dir);
    dates
        .iter()
        .map(|&d| match day_state(&dir, d, now) {
            DayState::Missing if busy => (d, DayState::Busy),
            s => (d, s),
        })
        .collect()
}

/// Another writer (a live feed, here or in another Lines) holds the contract's lock now.
fn locked(dir: &ContractDir) -> bool {
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.lock_path())
    else {
        return false;
    };
    f.try_lock().is_err()
}

/// How one day of a download ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Stored(Written),
    /// Not fetched: stored, no session, or not over.
    Skipped(DayState),
    /// A live feed is storing this contract.
    Busy,
    /// The provider's own words.
    Refused(String),
    /// The price is now above what was accepted: nothing was fetched.
    PriceChanged(Charge),
    Canceled,
    /// The local store failed (disk full, permissions).
    Failed(String),
}

/// What a job reports, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// An estimate job, first: what the store holds for each date, and the bytes it holds
    /// in all (for the budget note).
    Plan {
        days: Vec<(CivilDate, DayState)>,
        store_bytes: u64,
    },
    /// An estimate job: one day's answer (only days the store lacks are asked).
    Estimate {
        date: CivilDate,
        result: Result<DayEstimate, DayError>,
    },
    /// A download job: a day started.
    Started { date: CivilDate },
    /// A download job: the day's data was asked for. A paid day may be billed from here,
    /// whatever happens next (stopped, failed).
    Requested { date: CivilDate },
    /// A download job: bytes received so far for the day.
    Progress { date: CivilDate, received: u64 },
    /// A download job: how a day ended.
    Day { date: CivilDate, outcome: Outcome },
    /// The job is over (every day reported).
    Finished,
}

/// Cancels a job (shared with the thread running it), in one of two ways:
/// [`Cancel::cancel`] stops at once (the day in flight is discarded and leaves nothing; a
/// retry of a paid day is billed again); [`Cancel::cancel_remaining`] lets the day in flight
/// finish and be stored, and starts no other.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<(AtomicBool, AtomicBool)>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }
    /// Stop now.
    pub fn cancel(&self) {
        self.0.0.store(true, Ordering::Relaxed);
        self.0.1.store(true, Ordering::Relaxed);
    }
    /// Start no further day (the one in flight finishes).
    pub fn cancel_remaining(&self) {
        self.0.1.store(true, Ordering::Relaxed);
    }
    /// Stop now was asked.
    pub fn is_canceled(&self) -> bool {
        self.0.0.load(Ordering::Relaxed)
    }
    /// No further day may start.
    pub fn is_ending(&self) -> bool {
        self.0.1.load(Ordering::Relaxed)
    }
}

/// A running estimate or download on its own thread.
pub struct Job {
    rx: mpsc::Receiver<Update>,
    cancel: Cancel,
    thread: Option<std::thread::JoinHandle<()>>,
    /// `Finished` was delivered (sent, or made up for a thread that died).
    ended: std::cell::Cell<bool>,
}

impl Job {
    /// Updates that arrived since the last call (never blocks). A job whose thread ended
    /// without saying so (a panic, or it never started) ends with `Finished`.
    pub fn poll(&self) -> Vec<Update> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(u) => {
                    self.ended.set(self.ended.get() || u == Update::Finished);
                    out.push(u);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.ended.replace(true) {
                        out.push(Update::Finished);
                    }
                    break;
                }
            }
        }
        out
    }

    /// The next update, waiting up to `timeout` (tests, the command line); `Finished` for a
    /// thread that died.
    pub fn next(&self, timeout: Duration) -> Option<Update> {
        match self.rx.recv_timeout(timeout) {
            Ok(u) => {
                self.ended.set(self.ended.get() || u == Update::Finished);
                Some(u)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) if !self.ended.replace(true) => {
                Some(Update::Finished)
            }
            Err(_) => None,
        }
    }

    /// Stop now (the day in flight leaves nothing).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Let the day in flight finish; start no other.
    pub fn cancel_remaining(&self) {
        self.cancel.cancel_remaining();
    }

    /// Wait for the thread to end (after [`Update::Finished`]).
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Dropping a job (its window closed) is "Cancel remaining": a day in flight, maybe already
/// billed, finishes and is stored on the job's thread; no other day starts. Stop now
/// ([`Job::cancel`]) is the one way to discard it.
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.cancel_remaining();
    }
}

fn spawn(cancel: Cancel, f: impl FnOnce(&mpsc::Sender<Update>, &Cancel) + Send + 'static) -> Job {
    let (tx, rx) = mpsc::channel();
    let c = cancel.clone();
    let thread = std::thread::Builder::new()
        .name("lines-download".into())
        .spawn(move || {
            f(&tx, &c);
            let _ = tx.send(Update::Finished);
        })
        .ok();
    if thread.is_none() {
        tracing::error!("could not start the download thread");
    }
    Job {
        rx,
        cancel,
        thread,
        ended: std::cell::Cell::new(false),
    }
}

/// What the store holds for each date, then an estimate for every date it lacks (stored,
/// sessionless and unfinished days are never asked about), one by one on a download thread.
pub fn estimate(
    source: Arc<dyn PastDays>,
    target: Target,
    contents: &'static str,
    dates: Vec<CivilDate>,
    now: UtcMillis,
    cancel: Cancel,
) -> Job {
    spawn(cancel, move |tx, cancel| {
        let days = plan(&target, &dates, now);
        let store_bytes = crate::disk::retention::segments(&target.root)
            .iter()
            .map(|s| s.2)
            .sum();
        let _ = tx.send(Update::Plan {
            days: days.clone(),
            store_bytes,
        });
        for (date, state) in days {
            if cancel.is_ending() {
                break;
            }
            if state != DayState::Missing {
                continue;
            }
            let Some(req) = target.request(date, contents) else {
                continue;
            };
            let result = source.estimate(&req);
            let _ = tx.send(Update::Estimate { date, result });
        }
    })
}

/// Download each `(date, accepted charge)` in order into the store, on a download thread.
/// Each day is checked again first: a day now stored, busy, or priced above what was accepted
/// is not fetched.
pub fn download(
    source: Arc<dyn PastDays>,
    target: Target,
    contents: &'static str,
    days: Vec<(CivilDate, Charge)>,
    now: UtcMillis,
    cancel: Cancel,
) -> Job {
    spawn(cancel, move |tx, cancel| {
        for (date, accepted) in days {
            let outcome = if cancel.is_ending() {
                Outcome::Canceled
            } else {
                let _ = tx.send(Update::Started { date });
                one_day(&*source, &target, contents, date, accepted, now, cancel, tx)
            };
            let _ = tx.send(Update::Day { date, outcome });
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn one_day(
    source: &dyn PastDays,
    target: &Target,
    contents: &'static str,
    date: CivilDate,
    accepted: Charge,
    now: UtcMillis,
    cancel: &Cancel,
    tx: &mpsc::Sender<Update>,
) -> Outcome {
    let Some(req) = target.request(date, contents) else {
        return Outcome::Skipped(DayState::NoSession);
    };
    // Take the lock and the part file before spending anything.
    let mut writer = match DayWriter::begin(
        &target.root,
        &target.source,
        &target.product,
        Some(target.contract),
        date,
        now,
    ) {
        Ok(w) => w,
        Err(BeginError::Not(state)) => return Outcome::Skipped(state),
        Err(BeginError::Busy) => return Outcome::Busy,
        Err(BeginError::Io(e)) => return Outcome::Failed(e.to_string()),
    };
    // The price again (free): never more than what the person accepted.
    match source.estimate(&req) {
        Ok(e) if e.charge.usd_micros() <= accepted.usd_micros() => {}
        Ok(e) => return Outcome::PriceChanged(e.charge),
        Err(DayError::Refused(r)) => return Outcome::Refused(r.0),
        Err(DayError::Failed(m)) => return Outcome::Failed(m),
    }
    // Under the lock: no other download of this contract runs, so any scratch folder left
    // by a crashed one goes.
    sweep_scratch(&writer.dir().path);
    let scratch = writer.dir().path.join(format!("{date}.download"));
    if let Err(e) = std::fs::create_dir_all(&scratch) {
        return Outcome::Failed(e.to_string());
    }
    let mut sink = Sink {
        writer: &mut writer,
        cancel,
        tx,
        date,
        scratch: scratch.clone(),
        last_progress: None,
        error: None,
        stopped: false,
        requested: false,
        fetched: false,
        delivered: false,
    };
    let result = source.download(&req, &mut sink);
    let error = sink.error.take();
    let (stopped, requested, delivered, fetched) =
        (sink.stopped, sink.requested, sink.delivered, sink.fetched);
    let _ = std::fs::remove_dir_all(&scratch);
    if !requested && (result.is_ok() || delivered) {
        // A source that never said when it asked: anything arrived, so count it as charged
        // (never reads low).
        let _ = tx.send(Update::Requested { date });
    }
    if let Some(e) = error {
        return Outcome::Failed(e);
    }
    // Stored only if the source finished (Ok) and the sink never stopped it: a day stopped
    // mid-way leaves nothing; one whose paid data had all arrived (`fetched`) is never stopped.
    // After `fetched` a Stop now stops nothing: an error then is the conversion's, a failure.
    if stopped || (cancel.is_canceled() && !fetched) {
        return Outcome::Canceled; // the writer drops: no part file, no coverage
    }
    match result {
        Err(DayError::Refused(r)) => return Outcome::Refused(r.0),
        Err(DayError::Failed(m)) => return Outcome::Failed(m),
        Ok(()) => {}
    }
    match writer.commit() {
        Ok(w) => Outcome::Stored(w),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// Remove every `<date>.download` scratch folder in a contract's directory.
fn sweep_scratch(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() && p.extension().is_some_and(|x| x == "download") {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

struct Sink<'a> {
    writer: &'a mut DayWriter,
    cancel: &'a Cancel,
    tx: &'a mpsc::Sender<Update>,
    date: CivilDate,
    scratch: PathBuf,
    last_progress: Option<Instant>,
    error: Option<String>,
    /// `frame` answered `Stop` (Stop now before everything paid had arrived).
    stopped: bool,
    requested: bool,
    /// Everything paid has arrived: Stop now no longer stops the day.
    fetched: bool,
    /// Bytes or events arrived.
    delivered: bool,
}

impl DaySink for Sink<'_> {
    fn frame(&mut self, time_us: i64, events: &[NativeEvent]) -> Result<(), Stop> {
        if self.error.is_some() {
            return Err(Stop);
        }
        if self.cancel.is_canceled() && !self.fetched {
            self.stopped = true;
            return Err(Stop);
        }
        self.delivered = true;
        self.writer.block(time_us, events).map_err(|e| {
            self.error = Some(e.to_string());
            Stop
        })
    }

    fn received(&mut self, bytes: u64) {
        self.delivered |= bytes > 0;
        let due = self
            .last_progress
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
        if due {
            self.last_progress = Some(Instant::now());
            let _ = self.tx.send(Update::Progress {
                date: self.date,
                received: bytes,
            });
        }
    }

    /// Never once everything paid has arrived (`fetched`): a source that returns promptly
    /// on a cancel must not discard a paid day.
    fn canceled(&self) -> bool {
        self.cancel.is_canceled() && !self.fetched
    }

    fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    fn requesting(&mut self) {
        if !self.requested {
            self.requested = true;
            let _ = self.tx.send(Update::Requested { date: self.date });
        }
    }

    fn fetched(&mut self) {
        self.fetched = true;
    }
}

/// A synthetic source for tests and screenshots: deterministic trades and quotes for any
/// session, priced and refused by rule. No network, no vendor.
pub mod demo {
    use super::*;
    use crate::native::Aggressor;
    use lines_core::{Price, Qty, Side};

    pub const TOB: Contents = Contents {
        id: "tob",
        label: "Trades + top of book",
    };
    pub const DEPTH: Contents = Contents {
        id: "depth",
        label: "Full depth",
    };

    /// Seconds between synthetic trades.
    const EVERY_S: i64 = 30;

    #[derive(Clone, Debug, Default)]
    pub struct Demo {
        /// Days before this date are charged ([`Demo::price_usd_micros`] each); later days are
        /// included.
        pub included_from: Option<CivilDate>,
        pub price_usd_micros: u64,
        /// Dates the provider refuses, with its message.
        pub refuse: Vec<(CivilDate, String)>,
        /// Cancel this job after this many frames (tests).
        pub cancel_after: Option<(usize, Cancel)>,
        /// At this frame, meet the barrier twice (tests: hold a day mid-download).
        pub hold_at: Option<(usize, Arc<std::sync::Barrier>)>,
        /// Deliver nothing (tests: an empty day).
        pub empty: bool,
        /// Say everything paid has arrived before streaming (tests: Stop now after that).
        pub fetched_first: bool,
        /// Never call `requesting`, deliver a few frames, then fail (tests: a silent source is
        /// still counted as charged).
        pub silent_fail: bool,
        /// Estimate calls made (tests).
        pub asked: Arc<std::sync::Mutex<Vec<CivilDate>>>,
    }

    impl Demo {
        /// Every frame the demo delivers for a day: `(time_us, events)`.
        pub fn frames(req: &DayRequest) -> Vec<(i64, Vec<NativeEvent>)> {
            let tick = req.product.price.tick;
            let mut seed = req.date.days() as u64 ^ 0x9E37_79B9_7F4A_7C15;
            let mut px = 600_000 / tick * tick;
            let mut out = Vec::new();
            let mut t = req.open.0 / 1000 + 1;
            while t * 1000 < req.close.0 {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let step = ((seed >> 33) % 3) as i64 - 1;
                px += step * tick;
                let buy = (seed >> 40) & 1 == 1;
                let us = t * 1_000_000;
                let mut ev = vec![NativeEvent::Trade {
                    time_us: us,
                    price: Price(px),
                    qty: Qty(1 + ((seed >> 20) % 5) as i64),
                    aggressor: if buy { Aggressor::Buy } else { Aggressor::Sell },
                }];
                if req.contents == DEPTH.id {
                    ev.push(NativeEvent::BookLevel {
                        time_us: us,
                        side: Side::Bid,
                        price: Price(px - tick),
                        qty: Qty(10),
                        orders: 3,
                    });
                } else {
                    ev.push(NativeEvent::Bbo {
                        time_us: us,
                        bid: Some(Price(px - tick)),
                        ask: Some(Price(px)),
                    });
                }
                out.push((us + 50, ev));
                t += EVERY_S;
            }
            out
        }

        fn refusal(&self, date: CivilDate) -> Option<Refusal> {
            self.refuse
                .iter()
                .find(|(d, _)| *d == date)
                .map(|(_, m)| Refusal(m.clone()))
        }
    }

    impl PastDays for Demo {
        fn api_version(&self) -> u32 {
            API_VERSION
        }

        fn contents(&self, _product: &Product) -> Vec<Contents> {
            vec![TOB, DEPTH]
        }

        fn estimate(&self, req: &DayRequest) -> Result<DayEstimate, DayError> {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(req.date);
            if let Some(r) = self.refusal(req.date) {
                return Err(r.into());
            }
            // Full depth is included for a shorter window and costs three times as much, as
            // vendors price deeper data.
            let depth = req.contents == DEPTH.id;
            let from = self
                .included_from
                .map(|d| if depth { d.add_days(1) } else { d });
            let included = from.is_none_or(|d| req.date >= d);
            let per_frame: u64 = if depth { 900 } else { 120 };
            let frames = ((req.close.0 - req.open.0) / 1000 / EVERY_S) as u64;
            Ok(DayEstimate {
                charge: if included {
                    Charge::Included
                } else {
                    Charge::Estimated {
                        usd_micros: self.price_usd_micros * if depth { 3 } else { 1 },
                    }
                },
                bytes: frames * per_frame * 1000,
            })
        }

        fn download(&self, req: &DayRequest, sink: &mut dyn DaySink) -> Result<(), DayError> {
            if let Some(r) = self.refusal(req.date) {
                return Err(r.into());
            }
            if self.silent_fail {
                for (t, ev) in Self::frames(req).iter().take(3) {
                    sink.frame(*t, ev)
                        .map_err(|_| DayError::Failed("stopped".into()))?;
                }
                return Err(DayError::Failed("the connection dropped".into()));
            }
            sink.requesting();
            if self.fetched_first {
                sink.fetched();
            }
            if self.empty {
                return Ok(());
            }
            for (n, (t, ev)) in Self::frames(req).iter().enumerate() {
                if let Some((at, b)) = &self.hold_at
                    && n == *at
                {
                    b.wait();
                    b.wait();
                }
                if let Some((after, c)) = &self.cancel_after
                    && n == *after
                {
                    c.cancel();
                }
                if sink.canceled() {
                    return Err(DayError::Failed("canceled".into()));
                }
                sink.frame(*t, ev)
                    .map_err(|_| DayError::Failed("stopped".into()))?;
                sink.received(n as u64 * 120);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_and_sizes_read_honestly() {
        assert_eq!(usd_text(1_630_000), "$1.63");
        assert_eq!(usd_text(1_620_001), "$1.63", "rounded up");
        assert_eq!(usd_text(0), "$0.00");
        assert_eq!(
            Charge::from_usd(0.52, false).unwrap().text(),
            "Estimated $0.52"
        );
        assert_eq!(Charge::from_usd(0.0, true), Some(Charge::Included));
        assert_eq!(Charge::from_usd(-1.0, false), None);
        assert_eq!(Charge::from_usd(f64::NAN, false), None);
        assert_eq!(Charge::Included.text(), "Included in your plan");
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(58 * 1024 * 1024), "58.0 MiB");
        assert_eq!(size_text(2_222_000_000), "2.07 GiB");
        assert_eq!(size_text(11_400 * 1024 * 1024), "11.1 GiB");
    }

    #[test]
    fn date_ranges() {
        let d = |y, m, dd| CivilDate::new(y, m, dd).unwrap();
        assert_eq!(dates(d(2026, 11, 3), d(2026, 11, 5)).unwrap().len(), 3);
        assert!(dates(d(2026, 11, 5), d(2026, 11, 3)).is_err());
        assert!(dates(d(2026, 1, 1), d(2026, 12, 31)).is_err());
    }
}
