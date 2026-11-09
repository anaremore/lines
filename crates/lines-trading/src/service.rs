//! The trading core's runtime (decision 0012): its own thread, never a lock the UI holds.
//!
//! The UI (and anything else) talks to it through a [`TradingHandle`]:
//! - **Commands in**: [`TradingHandle::submit`] stamps the command with the input time, gives
//!   it an id and returns at once (a channel send, no waiting). The result arrives as an
//!   [`Event::Command`] with that id.
//! - **Events out**: [`TradingHandle::subscribe`] returns a channel of every [`Event`], in
//!   order.
//!
//! The thread wakes for commands, market data and the broker's due times (paper ack latency),
//! and saves the paper trading file (if configured) at most once a second after a change.
//! Market data comes from running feeds through [`FeedBridge`].
//!
//! **Time** ([`Clock`]): live sources run on the wall clock. A replay runs on market time
//! (the newest trade time seen, advancing with the wall clock between trades, so acks still
//! arrive while the market is quiet). [`Clock::Manual`] moves only with market data and
//! [`TradingHandle::advance_to`]: with [`TradingHandle::sync`] a session is deterministic
//! (tests, screenshots).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use lines_core::{ContractKey, ProductId, Side as BookSide, UtcMillis};
use lines_feed::FeedConfig;
use lines_feed::catalog::Target;
use lines_feed::provider::MarketEvent;
use lines_feed::registry::{FeedRegistry, Subscription};
use lines_feed::store::ChartStore;

use crate::broker::PaneContext;
use crate::command::{Command, Envelope, Source};
use crate::core::TradingCore;
use crate::event::Event;
use crate::market::{MarketInput, from_events, option_quotes, seed_option_quotes};
use crate::paper::PaperConfig;
use crate::persist::{SavedAccount, TradingFile, VERSION};
use crate::types::CommandId;

/// Where the core's `now` comes from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Clock {
    /// Live sources.
    Wall,
    /// Replays: the newest market time seen, plus the wall time since it arrived.
    Market,
    /// Only market data and [`TradingHandle::advance_to`] move time (deterministic).
    Manual,
}

/// Something that follows the core on its thread, in its order: every market batch the core
/// sees (after the core) and every event it emits (after each drain). Must be quick: it runs
/// on the trading thread (the journal, `crate::journal::JournalObserver`).
pub trait Observer: Send {
    fn market(&mut self, contract: ContractKey, inputs: &[MarketInput]);
    fn events(&mut self, events: &[Event]);
}

enum Msg {
    /// Boxed: a command (with its contracts) is far larger than every other message.
    Command(Box<Envelope>),
    Market(ContractKey, Vec<MarketInput>),
    Subscribe(Sender<Event>),
    Rebuild,
    AddAccounts(Vec<crate::core::AccountConfig>),
    Latency(Sender<(Option<u64>, Option<u64>, usize)>),
    AdvanceTo(UtcMillis),
    /// The strategy host is alive (decision 0025).
    StrategyHeartbeat,
    Sync(Sender<()>),
    Shutdown,
}

/// The way in (cheap to clone; usable from any thread).
#[derive(Clone)]
pub struct TradingHandle {
    tx: Sender<Msg>,
    next_id: Arc<AtomicU64>,
    order_latency: Arc<std::sync::Mutex<crate::core::OrderLatency>>,
}

impl TradingHandle {
    /// Submit a command now (its input time is now).
    pub fn submit(&self, source: Source, command: Command) -> CommandId {
        self.submit_at(source, Instant::now(), command)
    }

    /// Submit a command whose input event happened at `issued` (the click), so the latency
    /// metric covers the whole path.
    pub fn submit_at(&self, source: Source, issued: Instant, command: Command) -> CommandId {
        self.submit_from_pane(source, issued, None, command)
    }

    /// [`TradingHandle::submit_at`] for a command issued from a pane (DOM, chart): its
    /// contract, how it was chosen and the front month go to the broker with each request.
    pub fn submit_from_pane(
        &self,
        source: Source,
        issued: Instant,
        pane: Option<PaneContext>,
        command: Command,
    ) -> CommandId {
        let id = CommandId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let _ = self.tx.send(Msg::Command(Box::new(Envelope {
            id,
            source,
            issued,
            command,
            pane,
        })));
        id
    }

    /// The strategy host is alive (decision 0025). Once a host has sent one, armed strategies
    /// pause when none arrives for `strategy::HEARTBEAT_TIMEOUT` of wall-clock time. A
    /// message, not a command (no result event).
    pub fn strategy_heartbeat(&self) {
        let _ = self.tx.send(Msg::StrategyHeartbeat);
    }

    /// Every event from now on.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Msg::Subscribe(tx));
        rx
    }

    pub fn market(&self, contract: ContractKey, inputs: Vec<MarketInput>) {
        if !inputs.is_empty() {
            let _ = self.tx.send(Msg::Market(contract, inputs));
        }
    }

    /// Rebuild the cache from the broker (as after a reconnect). Also emits the full state
    /// (orders, positions, accounts): a new subscriber's starting point.
    pub fn rebuild(&self) {
        let _ = self.tx.send(Msg::Rebuild);
    }

    /// Add accounts the broker discovered after connecting (known ones are ignored); the core
    /// rebuilds so they show the broker's state.
    pub fn add_accounts(&self, accounts: Vec<crate::core::AccountConfig>) {
        let _ = self.tx.send(Msg::AddAccounts(accounts));
    }

    /// [`Clock::Manual`]: move time forward (never back).
    pub fn advance_to(&self, t: UtcMillis) {
        let _ = self.tx.send(Msg::AdvanceTo(t));
    }

    /// Wait until everything sent before this call is handled and its events delivered to
    /// subscribers. Blocks (tests, screenshots; never the UI's trading path).
    pub fn sync(&self) {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Msg::Sync(tx));
        let _ = rx.recv_timeout(Duration::from_secs(10));
    }

    /// The latency HUD's order windows (roadmap 6b). The UI reads them with `try_lock` only.
    pub fn order_latency(&self) -> &Arc<std::sync::Mutex<crate::core::OrderLatency>> {
        &self.order_latency
    }

    /// Click → order-sent latency: (p50 µs, p99 µs, samples). Blocks briefly (tests, stats).
    pub fn latency(&self) -> (Option<u64>, Option<u64>, usize) {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Msg::Latency(tx));
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or((None, None, 0))
    }
}

pub struct TradingService {
    handle: TradingHandle,
    thread: Option<thread::JoinHandle<()>>,
}

impl TradingService {
    /// Run `core` on its own thread on the wall clock. `file`: where to save paper state
    /// (with the paper broker's config, needed to write it).
    pub fn start(
        core: TradingCore,
        file: Option<(PathBuf, PaperConfig)>,
    ) -> Result<TradingService> {
        TradingService::start_with(core, file, Clock::Wall)
    }

    pub fn start_with(
        core: TradingCore,
        file: Option<(PathBuf, PaperConfig)>,
        clock: Clock,
    ) -> Result<TradingService> {
        TradingService::start_observed(core, file, clock, Vec::new())
    }

    /// [`TradingService::start_with`], with observers that follow the core on its thread.
    pub fn start_observed(
        core: TradingCore,
        file: Option<(PathBuf, PaperConfig)>,
        clock: Clock,
        observers: Vec<Box<dyn Observer>>,
    ) -> Result<TradingService> {
        let (tx, rx) = mpsc::channel();
        let handle = TradingHandle {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            order_latency: core.order_latency(),
        };
        let thread = thread::Builder::new()
            .name("lines-trading".into())
            .spawn(move || run(core, rx, file, None, clock, observers))?;
        Ok(TradingService {
            handle,
            thread: Some(thread),
        })
    }

    /// [`TradingService::start_observed`] for a broker venue's core: no trading file (the
    /// broker holds accounts and positions), but its accounts' guardrail progress is saved to
    /// `guards` (lockouts at once, other progress at most once a second) to be restored with
    /// `TradingCore::set_join_guardrails` at the next start.
    pub fn start_venue(
        core: TradingCore,
        clock: Clock,
        observers: Vec<Box<dyn Observer>>,
        guards: Option<PathBuf>,
    ) -> Result<TradingService> {
        let (tx, rx) = mpsc::channel();
        let handle = TradingHandle {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            order_latency: core.order_latency(),
        };
        let thread = thread::Builder::new()
            .name("lines-trading-venue".into())
            .spawn(move || run(core, rx, None, guards, clock, observers))?;
        Ok(TradingService {
            handle,
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> TradingHandle {
        self.handle.clone()
    }
}

impl Drop for TradingService {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The saved form of a core whose broker is the paper broker.
pub fn trading_file(core: &TradingCore, paper: &PaperConfig) -> Option<TradingFile> {
    let broker = serde_json::from_value(core.broker().saved_state()?).ok()?;
    Some(TradingFile {
        version: VERSION,
        paper: paper.clone(),
        broker,
        accounts: core
            .accounts()
            .map(|(c, t)| SavedAccount {
                config: c.clone(),
                tracker: t.clone(),
                guardrails: core.guardrails(&c.id).cloned().unwrap_or_default(),
                pending_rules: core.pending_rules(&c.id).cloned(),
            })
            .collect(),
        next_order_id: core.next_order_id(),
        in_flight: core.in_flight().cloned().collect(),
        triggers: core.saved_triggers(),
    })
}

/// The core's notion of now.
struct Now {
    clock: Clock,
    /// Market/Manual: the newest time seen, and (Market) when it arrived.
    market: Option<(UtcMillis, Instant)>,
    /// Never goes back.
    last: UtcMillis,
}

impl Now {
    fn now(&mut self) -> UtcMillis {
        let t = match (self.clock, self.market) {
            (Clock::Wall, _) => lines_feed::now_utc(),
            (Clock::Market, Some((t, at))) => {
                UtcMillis(t.0 + at.elapsed().as_millis().min(i64::MAX as u128) as i64)
            }
            (Clock::Manual, Some((t, _))) => t,
            (_, None) => self.last,
        };
        self.last = UtcMillis(self.last.0.max(t.0));
        self.last
    }

    /// Market data arrived: its newest trade time moves the market clocks.
    fn saw(&mut self, inputs: &[MarketInput]) {
        let newest = inputs
            .iter()
            .filter_map(|i| match i {
                MarketInput::Trade { time, .. } => Some(time.0),
                _ => None,
            })
            .max();
        if let Some(t) = newest {
            self.advance(UtcMillis(t));
        }
    }

    fn advance(&mut self, t: UtcMillis) {
        if self.clock != Clock::Wall && self.market.is_none_or(|(m, _)| t.0 > m.0) {
            self.market = Some((t, Instant::now()));
        }
    }

    /// When the core next needs a tick, as a wait from now.
    fn wait(&mut self, wake: Option<UtcMillis>) -> Duration {
        let idle = Duration::from_millis(100);
        if self.clock == Clock::Manual {
            // Time only moves with messages: nothing to wake for.
            return idle;
        }
        let now = self.now();
        wake.map(|t| Duration::from_millis((t.0 - now.0).clamp(0, 100) as u64))
            .unwrap_or(idle)
    }
}

fn run(
    mut core: TradingCore,
    rx: Receiver<Msg>,
    file: Option<(PathBuf, PaperConfig)>,
    guards: Option<PathBuf>,
    clock: Clock,
    mut observers: Vec<Box<dyn Observer>>,
) {
    let mut guard_saves = guards.map(|path| GuardSaves {
        path,
        written: None,
        dirty: false,
        last: Instant::now(),
    });
    let mut subscribers: Vec<Sender<Event>> = Vec::new();
    let mut dirty = false;
    let mut last_save = Instant::now();
    let mut clock = Now {
        clock,
        market: None,
        last: UtcMillis(0),
    };
    let mut syncs: Vec<Sender<()>> = Vec::new();
    // The strategy host's last heartbeat (wall clock); `None` until one arrives.
    let mut last_beat: Option<Instant> = None;
    // Events of this batch so far: observers get them before each market batch (so a trade
    // the batch opened sees the prices that follow), subscribers all of them at the end.
    let mut events: Vec<Event> = Vec::new();
    loop {
        let wait = clock.wait(core.next_wake());
        // Handle the first message, then everything already queued, before the broker tick
        // and the broadcast (a burst of clicks doesn't wait behind per-message overhead).
        let mut msg = rx.recv_timeout(wait);
        let mut stop = false;
        loop {
            match msg {
                Ok(Msg::Command(env)) => {
                    let now = clock.now();
                    core.handle(now, *env);
                    dirty = true;
                }
                Ok(Msg::Market(k, inputs)) => {
                    clock.saw(&inputs);
                    let now = clock.now();
                    // Acks due before this data happen first.
                    core.tick(now);
                    if !observers.is_empty() {
                        let before = core.drain();
                        for o in &mut observers {
                            o.events(&before);
                        }
                        events.extend(before);
                    }
                    core.market(now, k, &inputs);
                    for o in &mut observers {
                        o.market(k, &inputs);
                    }
                }
                Ok(Msg::Subscribe(tx)) => subscribers.push(tx),
                Ok(Msg::Rebuild) => core.rebuild(clock.now()),
                Ok(Msg::AddAccounts(accounts)) => core.add_accounts(clock.now(), accounts),
                Ok(Msg::Latency(tx)) => {
                    let l = core.latency();
                    let _ = tx.send((l.percentile(50.0), l.percentile(99.0), l.count()));
                }
                Ok(Msg::AdvanceTo(t)) => {
                    clock.advance(t);
                    core.tick(clock.now());
                }
                Ok(Msg::Sync(tx)) => syncs.push(tx),
                Ok(Msg::StrategyHeartbeat) => last_beat = Some(Instant::now()),
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    stop = true;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
            msg = match rx.try_recv() {
                Ok(m) => Ok(m),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
            };
        }
        if stop {
            break;
        }
        // A silent strategy host: armed strategies pause (once; the next heartbeat re-arms
        // this watch, never the strategies).
        if last_beat.is_some_and(|t| t.elapsed() > crate::strategy::HEARTBEAT_TIMEOUT) {
            last_beat = None;
            core.strategy_host_silent(clock.now());
        }
        core.tick(clock.now());
        core.note_time(clock.now());
        let rest = core.drain();
        for o in &mut observers {
            o.events(&rest);
        }
        events.extend(rest);
        let events = std::mem::take(&mut events);
        if let Some(g) = &mut guard_saves {
            g.after(&core, &events);
        }
        if !events.is_empty() {
            dirty = true;
            subscribers.retain(|s| events.iter().all(|e| s.send(e.clone()).is_ok()));
        }
        for s in syncs.drain(..) {
            let _ = s.send(());
        }
        if dirty && last_save.elapsed() >= Duration::from_secs(1) {
            save(&core, &file);
            dirty = false;
            last_save = Instant::now();
        }
    }
    save(&core, &file);
    if let Some(g) = &mut guard_saves {
        g.dirty = true;
        g.write(&core);
    }
}

/// A venue's guardrails file: written when what it would hold changed, at once for a lockout
/// (or its end), else at most once a second.
struct GuardSaves {
    path: PathBuf,
    written: Option<crate::persist::GuardFile>,
    dirty: bool,
    last: Instant,
}

impl GuardSaves {
    fn after(&mut self, core: &TradingCore, events: &[Event]) {
        self.dirty |= !events.is_empty();
        let lock = events.iter().any(|e| matches!(e, Event::Guardrail { .. }));
        if lock || (self.dirty && self.last.elapsed() >= Duration::from_secs(1)) {
            self.write(core);
        }
    }

    fn write(&mut self, core: &TradingCore) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.last = Instant::now();
        let f = core.guard_file();
        if self.written.as_ref() == Some(&f) {
            return;
        }
        match crate::persist::save_guards(&self.path, &f) {
            Ok(()) => self.written = Some(f),
            Err(e) => tracing::error!("can't save {}: {e:#}", self.path.display()),
        }
    }
}

fn save(core: &TradingCore, file: &Option<(PathBuf, PaperConfig)>) {
    if let Some((path, paper)) = file
        && let Some(f) = trading_file(core, paper)
        && let Err(e) = crate::persist::save(path, &f)
    {
        tracing::error!("can't save {}: {e:#}", path.display());
    }
}

/// Market data for the trading core from running feeds.
///
/// - [`FeedBridge::follow`] subscribes to a contract through the shared registry (decision
///   0013: always a contract).
/// - [`FeedBridge::follow_store`] listens to a feed that is already running (a pane's), so the
///   paper broker sees exactly the data the pane shows, without a second connection or a
///   second replay. Its contract comes from the store and follows `ContractChanged`.
#[derive(Clone)]
pub struct FeedBridge {
    registry: FeedRegistry,
    base: FeedConfig,
    subs: Arc<Mutex<HashMap<ContractKey, Subscription>>>,
    /// Feed ids being forwarded by `follow_store`.
    listening: Arc<Mutex<HashSet<u64>>>,
    /// The underlyings of live triggers: their feeds, kept running with or without a pane.
    held: Arc<Mutex<HashMap<ProductId, Subscription>>>,
}

impl FeedBridge {
    /// `base`: the data source (its target is replaced per contract).
    pub fn new(registry: FeedRegistry, base: FeedConfig) -> FeedBridge {
        FeedBridge {
            registry,
            base,
            subs: Arc::default(),
            listening: Arc::default(),
            held: Arc::default(),
        }
    }

    /// Keep a feed running for exactly these products, so a trigger on one can fire with no pane
    /// open (decision 0029). It's the same subscription a pane on that product has, so the core
    /// never gets a feed's data twice. Returns the products with no feed to be had: their
    /// triggers can't fire, which the app says.
    pub fn hold_underlyings(
        &self,
        wanted: &BTreeSet<ProductId>,
        handle: &TradingHandle,
    ) -> BTreeSet<ProductId> {
        // Out from under the lock: the last subscriber gone joins its feed's thread.
        let gone: Vec<Subscription> = {
            let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
            let drop_these: Vec<ProductId> = held
                .keys()
                .filter(|p| !wanted.contains(p))
                .copied()
                .collect();
            drop_these.iter().filter_map(|p| held.remove(p)).collect()
        };
        drop(gone);
        let mut missing = BTreeSet::new();
        for product in wanted {
            if self.holding(*product) {
                continue;
            }
            let mut cfg = self.base.clone();
            cfg.target = Some(Target::Product(*product));
            cfg.record = None;
            match self.registry.subscribe(cfg) {
                Ok(sub) => {
                    self.follow_store(sub.store(), handle);
                    self.held
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(*product, sub);
                }
                Err(e) => {
                    tracing::warn!("no feed for a trigger's underlying {product:?}: {e:#}");
                    missing.insert(*product);
                }
            }
        }
        missing
    }

    /// A feed is being kept running for this product.
    pub fn holding(&self, product: ProductId) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&product)
    }

    /// Start forwarding a contract's market data to the trading core (once per contract).
    pub fn follow(&self, contract: ContractKey, handle: &TradingHandle) -> Result<()> {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        if subs.contains_key(&contract) {
            return Ok(());
        }
        let mut cfg = self.base.clone();
        cfg.target = Some(Target::Contract(contract));
        cfg.record = None;
        let sub = self.registry.subscribe(cfg)?;
        let rx = lines_feed::listen::listen(sub.store().feed_id());
        let h = handle.clone();
        thread::Builder::new()
            .name("lines-trading-md".into())
            .spawn(move || {
                for batch in rx {
                    h.market(contract, from_events(&batch));
                    for (k, q) in option_quotes(contract.product, &batch) {
                        h.market(k, vec![q]);
                    }
                }
            })?;
        subs.insert(contract, sub);
        Ok(())
    }

    /// Forward a running feed's data (once per feed). The core is first given the store's
    /// current book and last price (book levels are totals, so overlap with the first
    /// forwarded batches is harmless; no trade is replayed). Ends when the store is gone.
    pub fn follow_store(&self, store: &Arc<ChartStore>, handle: &TradingHandle) {
        let feed = store.feed_id();
        if !self
            .listening
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(feed)
        {
            return;
        }
        // Listen first, then snapshot: nothing between the two is lost.
        let rx = lines_feed::listen::listen(feed);
        let (contract, seed, options, market_time) = {
            let d = store.read();
            (
                d.contract,
                seed_inputs(&d),
                seed_option_quotes(d.product.id, &d.chains),
                d.market_time,
            )
        };
        // Market clocks start at the feed's time (a finished replay has no trades to come).
        if let Some(t) = market_time {
            handle.advance_to(t);
        }
        if let Some(k) = contract {
            handle.market(k, seed);
        }
        for (k, q) in options {
            handle.market(k, vec![q]);
        }
        let weak: Weak<ChartStore> = Arc::downgrade(store);
        let h = handle.clone();
        let listening = self.listening.clone();
        let spawned = thread::Builder::new()
            .name("lines-trading-md".into())
            .spawn(move || {
                let mut contract = contract;
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(batch) => {
                            for e in batch.iter() {
                                if let MarketEvent::ContractChanged(k) = e {
                                    contract = Some(*k);
                                }
                            }
                            if let Some(k) = contract {
                                h.market(k, from_events(&batch));
                                for (o, q) in option_quotes(k.product, &batch) {
                                    h.market(o, vec![q]);
                                }
                            }
                        }
                        Err(RecvTimeoutError::Timeout) if weak.strong_count() > 0 => {}
                        Err(_) => break,
                    }
                }
                listening
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&feed);
            });
        if let Err(e) = spawned {
            tracing::error!("can't forward market data to trading: {e}");
            self.listening
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&feed);
        }
    }
}

/// A store's current market state as inputs: the book, best bid/ask, the last price.
fn seed_inputs(d: &lines_feed::ChartData) -> Vec<MarketInput> {
    let mut v = vec![MarketInput::BookClear];
    for side in [BookSide::Bid, BookSide::Ask] {
        v.extend(d.book.levels(side).map(|(price, l)| MarketInput::Book {
            side,
            price,
            qty: l.qty,
        }));
    }
    v.extend(d.bid.map(MarketInput::Bid));
    v.extend(d.ask.map(MarketInput::Ask));
    v.extend(d.last_trade.map(MarketInput::Last));
    v
}
