//! The strategy host (decision 0025): runs strategy instances in the app, on their own thread,
//! off the UI thread and off the trading thread (decision 0012). It never takes a lock the UI
//! or the core holds.
//!
//! - **In:** each instance's market batches (`lines_feed::listen` on its feed), its required
//!   level sources (polled by revision), the trading core's events about it, and a timer.
//! - **Out:** its decisions' commands, sent with `Source::Strategy(id)` through the same
//!   `TradingHandle` as everything else: the core's strategy gate, prop rules, risk and
//!   guardrails apply, and a person's order on the same account and contract pauses it.
//! - **Heartbeat:** every second, so the core pauses armed instances if this thread stops.
//! - **Pauses it sends itself** (its own instance, `PauseStrategy`): the feed is stale, a
//!   required level source is missing or stale, the contract changed (a roll), or the strategy
//!   panicked (the instance is then never called again).
//! - It never arms anything: only a person does (the core refuses anyone else).
//! - [`StrategyHost::snapshot`] for the strategy panel: each instance's state, status lines and
//!   latest decisions with their results.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use lines_core::{Product, UtcMillis};
use lines_feed::levels::runtime::{LevelHandle, LevelStatus};
use lines_feed::provider::MarketEvent;
use lines_trading::{
    Clock, Command, Event, Source, StrategyConfig, StrategyId, StrategyState, TradingHandle,
};

use crate::backtest::DecisionRecord;
use crate::{Ctx, Decision, Input, LevelView, Strategy, StrategyExtension};

/// Decisions one input may cause (a strategy answering its own events in a loop pauses).
const MAX_DECISIONS_PER_INPUT: usize = 64;
/// Decisions kept per instance for the panel.
pub const KEEP_DECISIONS: usize = 200;
/// The host's tick: level polls, staleness checks, the timer input, the heartbeat.
const TICK: Duration = Duration::from_millis(250);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);

/// A level source as the host reads it (a `LevelHandle` in the app; fakes in tests).
pub trait LevelFeed: Send {
    fn source(&self) -> &'static str;
    /// Bumped on every change of the map or its status.
    fn revision(&self) -> u64;
    fn view(&self, now: UtcMillis) -> LevelView;
}

impl LevelFeed for LevelHandle {
    fn source(&self) -> &'static str {
        self.cell().read().source
    }
    fn revision(&self) -> u64 {
        self.cell().revision()
    }
    fn view(&self, now: UtcMillis) -> LevelView {
        let s = self.cell().read();
        LevelView {
            source: s.source,
            status: s.status(now),
            map: s.map.clone(),
            received: s.received,
        }
    }
}

/// One instance to run.
pub struct Instance {
    pub ext: Arc<dyn StrategyExtension>,
    pub params: serde_json::Value,
    pub config: StrategyConfig,
    pub product: Product,
    /// Its feed's batches (`lines_feed::listen(store.feed_id())`).
    pub market: Receiver<Arc<[MarketEvent]>>,
    /// One per source in `ext.level_sources()`.
    pub levels: Vec<Box<dyn LevelFeed>>,
    /// The feed counts as stale after this long without a batch (wall clock).
    pub stale_after: Duration,
}

/// An instance as the panel shows it.
#[derive(Clone, Debug)]
pub struct InstanceView {
    pub id: StrategyId,
    pub label: String,
    pub strategy: String,
    pub version: String,
    pub state: StrategyState,
    pub reason: Option<String>,
    /// The strategy failed (panicked or was refused); it is never called again.
    pub fault: Option<String>,
    pub status: Vec<String>,
    /// Newest last (the newest [`KEEP_DECISIONS`] only).
    pub decisions: Vec<DecisionRecord>,
    /// Decisions made so far, all of them (to tell new ones from the ring's).
    pub decided: u64,
}

enum Msg {
    Add(Box<Running>),
    Remove(StrategyId),
    Market(StrategyId, Arc<[MarketEvent]>),
    Core(Box<Event>),
    Sync(Sender<()>),
    Shutdown,
}

struct Running {
    cfg: StrategyConfig,
    ext: Arc<dyn StrategyExtension>,
    product: Product,
    strategy: Box<dyn Strategy>,
    levels: Vec<Box<dyn LevelFeed>>,
    /// Revision and status each level source was last given to the strategy at.
    told: Vec<Option<(u64, LevelStatus)>>,
    stale_after: Duration,
    state: StrategyState,
    reason: Option<String>,
    fault: Option<String>,
    /// When the last batch arrived (wall clock), and the newest trade time in it.
    last_batch: Instant,
    market_time: Option<(UtcMillis, Instant)>,
    decisions: VecDeque<DecisionRecord>,
    decided: u64,
    /// The pause the host asked for, until its answer (not sent twice).
    pausing: Option<u64>,
    alive: Arc<AtomicBool>,
}

pub struct StrategyHost {
    tx: Sender<Msg>,
    thread: Option<thread::JoinHandle<()>>,
    view: Arc<Mutex<Vec<InstanceView>>>,
    revision: Arc<AtomicU64>,
}

impl StrategyHost {
    /// Start the host for one trading core. `clock`: how the core reads time (a replay's
    /// market clock, the wall clock live), so the strategies see the same `now`.
    pub fn start(handle: TradingHandle, clock: Clock) -> StrategyHost {
        let (tx, rx) = mpsc::channel();
        let events = handle.subscribe();
        let fwd = tx.clone();
        thread::Builder::new()
            .name("lines-strategy-events".into())
            .spawn(move || {
                for e in events {
                    if fwd.send(Msg::Core(Box::new(e))).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn the strategy event forwarder");
        let view = Arc::new(Mutex::new(Vec::new()));
        let revision = Arc::new(AtomicU64::new(0));
        let (v, r) = (view.clone(), revision.clone());
        let thread = thread::Builder::new()
            .name("lines-strategies".into())
            .spawn(move || run(handle, clock, rx, v, r))
            .expect("spawn the strategy host");
        StrategyHost {
            tx,
            thread: Some(thread),
            view,
            revision,
        }
    }

    /// Add an instance: the strategy is created now (bad parameters refuse it here, with the
    /// reason), then the host adds it to the core (Disarmed). A person arms it.
    pub fn add(&self, inst: Instance) -> Result<(), String> {
        if inst.levels.len() != inst.ext.level_sources().len()
            || inst
                .levels
                .iter()
                .zip(inst.ext.level_sources())
                .any(|(l, s)| l.source() != *s)
        {
            return Err(format!(
                "{} needs the level sources {:?}",
                inst.ext.label(),
                inst.ext.level_sources()
            ));
        }
        if !inst.ext.serves(&inst.product) {
            return Err(format!(
                "{} doesn't trade {}",
                inst.ext.label(),
                inst.product.symbol
            ));
        }
        if self.snapshot().iter().any(|v| v.id == inst.config.id) {
            return Err(format!("{} is already running", inst.config.id));
        }
        let strategy = inst.ext.create(&inst.params)?;
        let alive = Arc::new(AtomicBool::new(true));
        let id = inst.config.id;
        let (fwd, a) = (self.tx.clone(), alive.clone());
        let market = inst.market;
        thread::Builder::new()
            .name(format!("lines-strategy-md-{}", id.0))
            .spawn(move || {
                for b in market {
                    if !a.load(Ordering::Acquire) || fwd.send(Msg::Market(id, b)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let n = inst.levels.len();
        let running = Running {
            cfg: inst.config,
            ext: inst.ext,
            product: inst.product,
            strategy,
            levels: inst.levels,
            told: vec![None; n],
            stale_after: inst.stale_after,
            state: StrategyState::Disarmed,
            reason: None,
            fault: None,
            last_batch: Instant::now(),
            market_time: None,
            decisions: VecDeque::new(),
            decided: 0,
            pausing: None,
            alive,
        };
        self.tx
            .send(Msg::Add(Box::new(running)))
            .map_err(|_| "the strategy host has stopped".to_string())
    }

    /// Stop running an instance and remove it from the core (its entries cancelled).
    pub fn remove(&self, id: StrategyId) {
        let _ = self.tx.send(Msg::Remove(id));
    }

    /// The instances, for the panel (a short lock; never held while strategies run).
    pub fn snapshot(&self) -> Vec<InstanceView> {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Wait until everything sent to the host before this call is handled, the level sources
    /// polled and the snapshot published. Blocks (tests, screenshots; never the UI).
    pub fn sync(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Msg::Sync(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }

    /// Bumped whenever the snapshot changed.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

impl Drop for StrategyHost {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Host {
    handle: TradingHandle,
    clock: Clock,
    instances: BTreeMap<StrategyId, Running>,
    /// Command id → (instance, its decision's sequence number).
    pending: BTreeMap<u64, (StrategyId, u64)>,
    /// Decisions recorded per instance so far (to find one in the ring).
    seq: BTreeMap<StrategyId, u64>,
    /// The host's own commands: adding an instance (refused: the instance faults) and pauses.
    adds: BTreeMap<u64, StrategyId>,
    changed: bool,
}

fn run(
    handle: TradingHandle,
    clock: Clock,
    rx: Receiver<Msg>,
    view: Arc<Mutex<Vec<InstanceView>>>,
    revision: Arc<AtomicU64>,
) {
    let mut h = Host {
        handle,
        clock,
        instances: BTreeMap::new(),
        pending: BTreeMap::new(),
        seq: BTreeMap::new(),
        adds: BTreeMap::new(),
        changed: false,
    };
    let mut next_tick = Instant::now();
    let mut next_beat = Instant::now();
    let mut syncs: Vec<Sender<()>> = Vec::new();
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Add(r)) => h.add(*r),
            Ok(Msg::Remove(id)) => h.remove(id),
            Ok(Msg::Market(id, b)) => h.market(id, &b),
            Ok(Msg::Core(e)) => h.core(&e),
            Ok(Msg::Sync(tx)) => {
                // Answered after the tick below (levels polled, snapshot published).
                next_tick = Instant::now();
                syncs.push(tx);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + TICK;
            if Instant::now() >= next_beat {
                next_beat = Instant::now() + HEARTBEAT_EVERY;
                h.handle.strategy_heartbeat();
                h.timer();
            }
            h.poll_levels();
        }
        if std::mem::take(&mut h.changed) {
            let v: Vec<InstanceView> = h.instances.values().map(Running::view).collect();
            *view.lock().unwrap_or_else(|e| e.into_inner()) = v;
            revision.fetch_add(1, Ordering::Release);
        }
        for s in syncs.drain(..) {
            let _ = s.send(());
        }
    }
    for r in h.instances.values() {
        r.alive.store(false, Ordering::Release);
    }
}

impl Running {
    fn view(&self) -> InstanceView {
        InstanceView {
            id: self.cfg.id,
            label: self.cfg.label.clone(),
            strategy: self.ext.name().to_owned(),
            version: self.ext.version().to_owned(),
            state: self.state,
            reason: self.reason.clone(),
            fault: self.fault.clone(),
            status: catch_unwind(AssertUnwindSafe(|| self.strategy.status())).unwrap_or_default(),
            decisions: self.decisions.iter().cloned().collect(),
            decided: self.decided,
        }
    }

    /// The instance's `now`: the wall clock live; on a market or manual clock the newest
    /// trade time seen (`None` before the first one: not yet known).
    fn now(&self, clock: Clock) -> Option<UtcMillis> {
        match (clock, self.market_time) {
            (Clock::Wall, _) => Some(lines_feed::now_utc()),
            (Clock::Market, Some((t, at))) => Some(UtcMillis(
                t.0 + at.elapsed().as_millis().min(i64::MAX as u128) as i64,
            )),
            (Clock::Manual, Some((t, _))) => Some(t),
            (_, None) => None,
        }
    }
}

impl Host {
    fn add(&mut self, r: Running) {
        let id = r.cfg.id;
        if let Some(old) = self.instances.get(&id) {
            old.alive.store(false, Ordering::Release);
        }
        // The core holds the instance (Disarmed); its answer comes back as an event.
        let cid = self
            .handle
            .submit(Source::Api, Command::AddStrategy(r.cfg.clone()));
        self.adds.insert(cid.0, id);
        self.instances.insert(id, r);
        self.changed = true;
    }

    fn remove(&mut self, id: StrategyId) {
        if let Some(r) = self.instances.remove(&id) {
            r.alive.store(false, Ordering::Release);
            self.seq.remove(&id);
            self.pending.retain(|_, (s, _)| *s != id);
            self.adds.retain(|_, s| *s != id);
            self.handle
                .submit(Source::Api, Command::RemoveStrategy { id });
            self.changed = true;
        }
    }

    /// Run one input through an instance and send its decisions.
    fn input(&mut self, id: StrategyId, input: Input<'_>) {
        let clock = self.clock;
        let Some(r) = self.instances.get_mut(&id) else {
            return;
        };
        if r.fault.is_some() {
            return;
        }
        // Before a replay's first trade, the wall clock (nothing is armed on it yet).
        let now = r.now(clock).unwrap_or_else(lines_feed::now_utc);
        let ctx = Ctx {
            now,
            id,
            account: &r.cfg.account,
            contract: r.cfg.contract,
            product: &r.product,
            state: r.state,
        };
        let mut out = Vec::new();
        let strategy = &mut r.strategy;
        let ran = catch_unwind(AssertUnwindSafe(|| {
            strategy.on_input(&ctx, input, &mut out)
        }));
        if let Err(p) = ran {
            let why = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".into());
            r.fault = Some(format!("the strategy failed: {why}"));
            tracing::error!(target: "lines::strategy", strategy = %id, "failed: {why}");
            let reason = r.fault.clone().unwrap_or_default();
            self.pause(id, reason);
            self.changed = true;
            return;
        }
        self.decide(id, now, out);
    }

    fn decide(&mut self, id: StrategyId, now: UtcMillis, out: Vec<Decision>) {
        let mut budget = MAX_DECISIONS_PER_INPUT;
        let mut paused = false;
        for d in out {
            let seq = {
                let s = self.seq.entry(id).or_default();
                *s += 1;
                *s
            };
            let over = budget == 0;
            budget = budget.saturating_sub(1);
            let command = (!over).then(|| d.command.clone()).flatten();
            let record = DecisionRecord {
                at: now,
                not_sent: (over && d.command.is_some()).then(|| {
                    format!("over the budget of {MAX_DECISIONS_PER_INPUT} decisions for one input")
                }),
                decision: d,
                result: None,
            };
            tracing::info!(
                target: "lines::strategy",
                strategy = %id,
                rule = %record.decision.rule,
                "{}",
                record.decision.reason
            );
            if let Some(r) = self.instances.get_mut(&id) {
                r.decided += 1;
                r.decisions.push_back(record);
                while r.decisions.len() > KEEP_DECISIONS {
                    r.decisions.pop_front();
                }
            }
            if let Some(c) = command {
                let cid = self.handle.submit(Source::Strategy(id), c);
                self.pending.insert(cid.0, (id, seq));
            }
            if over && !paused {
                paused = true;
                self.pause(
                    id,
                    format!("more than {MAX_DECISIONS_PER_INPUT} decisions for one input"),
                );
            }
            self.changed = true;
        }
    }

    /// Ask the core to pause an instance (its own command; never twice in a row).
    fn pause(&mut self, id: StrategyId, reason: String) {
        let Some(r) = self.instances.get_mut(&id) else {
            return;
        };
        if r.state != StrategyState::Armed || r.pausing.is_some() {
            return;
        }
        tracing::warn!(target: "lines::strategy", strategy = %id, "pausing: {reason}");
        let cid = self
            .handle
            .submit(Source::Strategy(id), Command::PauseStrategy { id, reason });
        r.pausing = Some(cid.0);
    }

    fn market(&mut self, id: StrategyId, batch: &[MarketEvent]) {
        let Some(r) = self.instances.get_mut(&id) else {
            return;
        };
        r.last_batch = Instant::now();
        let newest = batch
            .iter()
            .filter_map(|e| match e {
                MarketEvent::Trade { time, .. } => Some(time.0),
                _ => None,
            })
            .max();
        if let Some(t) = newest
            && r.market_time.is_none_or(|(m, _)| t > m.0)
        {
            r.market_time = Some((UtcMillis(t), Instant::now()));
        }
        let rolled = batch.iter().find_map(|e| match e {
            MarketEvent::ContractChanged(k) if *k != r.cfg.contract => Some(*k),
            _ => None,
        });
        if let Some(k) = rolled {
            let label = k.label(&r.product).as_str().to_owned();
            self.pause(
                id,
                format!("the feed moved to {label}: add the strategy for the new contract"),
            );
            return;
        }
        self.input(id, Input::Market(batch));
    }

    fn core(&mut self, e: &Event) {
        if let Event::Command { id, result, .. } = e {
            if let Some(sid) = self.adds.remove(&id.0)
                && let Err(r) = result
                && let Some(i) = self.instances.get_mut(&sid)
            {
                i.fault = Some(format!("the trading core refused it: {}", r.message));
                i.alive.store(false, Ordering::Release);
                self.changed = true;
            }
            for r in self.instances.values_mut() {
                if r.pausing == Some(id.0) {
                    r.pausing = None;
                }
            }
        }
        if let Event::Command { id, result, .. } = e
            && let Some((sid, seq)) = self.pending.remove(&id.0)
            && let Some(r) = self.instances.get_mut(&sid)
        {
            let newest = *self.seq.get(&sid).unwrap_or(&0);
            let back = (newest - seq) as usize;
            let len = r.decisions.len();
            if back < len {
                r.decisions[len - 1 - back].result = Some(result.clone());
            }
            self.changed = true;
        }
        if let Event::Strategy {
            id, state, reason, ..
        } = e
            && let Some(r) = self.instances.get_mut(id)
        {
            r.state = *state;
            r.reason = reason.clone();
            self.changed = true;
            // A strategy that failed is never called again: armed (by a person, or the arm
            // and the failure crossed), it is paused at once.
            if *state == StrategyState::Armed
                && let Some(f) = r.fault.clone()
            {
                let sid = *id;
                self.pause(sid, f);
            }
        }
        let ids: Vec<StrategyId> = self
            .instances
            .values()
            .filter(|r| crate::concerns(&r.cfg, e))
            .map(|r| r.cfg.id)
            .collect();
        for id in ids {
            self.input(id, Input::Trading(e));
        }
    }

    /// Level sources: a new revision or a freshness change goes to the strategy; missing or
    /// stale levels pause an armed instance.
    fn poll_levels(&mut self) {
        let clock = self.clock;
        let ids: Vec<StrategyId> = self.instances.keys().copied().collect();
        for id in ids {
            let mut views = Vec::new();
            let mut bad: Option<String> = None;
            if let Some(r) = self.instances.get_mut(&id) {
                // Freshness needs the instance's clock: before a replay's first trade it is
                // unknown, so nothing pauses for it yet.
                let known = r.now(clock);
                let now = known.unwrap_or_else(lines_feed::now_utc);
                for (i, l) in r.levels.iter().enumerate() {
                    let rev = l.revision();
                    let view = l.view(now);
                    if known.is_some()
                        && !matches!(view.status, LevelStatus::Current)
                        && bad.is_none()
                    {
                        bad = Some(format!(
                            "levels {} are {}",
                            view.source,
                            if matches!(view.status, LevelStatus::Missing) {
                                "missing"
                            } else {
                                "stale"
                            }
                        ));
                    }
                    if r.told[i] != Some((rev, view.status)) {
                        r.told[i] = Some((rev, view.status));
                        views.push(view);
                    }
                }
            }
            for v in &views {
                self.input(id, Input::Levels(v));
            }
            if let Some(why) = bad {
                self.pause(id, why);
            }
        }
    }

    /// The timer input, and the feed's staleness.
    fn timer(&mut self) {
        let ids: Vec<StrategyId> = self.instances.keys().copied().collect();
        for id in ids {
            let stale = self
                .instances
                .get(&id)
                .is_some_and(|r| r.last_batch.elapsed() > r.stale_after);
            if stale {
                self.pause(id, "the market data is stale".into());
            }
            self.input(id, Input::Timer);
        }
    }
}
