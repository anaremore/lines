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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use lines_core::ContractKey;
use lines_feed::FeedConfig;
use lines_feed::catalog::Target;
use lines_feed::registry::{FeedRegistry, Subscription};

use crate::command::{Command, Envelope, Source};
use crate::core::TradingCore;
use crate::event::Event;
use crate::market::{MarketInput, from_events};
use crate::paper::PaperConfig;
use crate::persist::{SavedAccount, TradingFile, VERSION};
use crate::types::CommandId;

enum Msg {
    Command(Envelope),
    Market(ContractKey, Vec<MarketInput>),
    Subscribe(Sender<Event>),
    Rebuild,
    Latency(Sender<(Option<u64>, Option<u64>, usize)>),
    Shutdown,
}

/// The way in (cheap to clone; usable from any thread).
#[derive(Clone)]
pub struct TradingHandle {
    tx: Sender<Msg>,
    next_id: Arc<AtomicU64>,
}

impl TradingHandle {
    /// Submit a command now (its input time is now).
    pub fn submit(&self, source: Source, command: Command) -> CommandId {
        self.submit_at(source, Instant::now(), command)
    }

    /// Submit a command whose input event happened at `issued` (the click), so the latency
    /// metric covers the whole path.
    pub fn submit_at(&self, source: Source, issued: Instant, command: Command) -> CommandId {
        let id = CommandId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let _ = self.tx.send(Msg::Command(Envelope {
            id,
            source,
            issued,
            command,
        }));
        id
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

    /// Rebuild the cache from the broker (as after a reconnect).
    pub fn rebuild(&self) {
        let _ = self.tx.send(Msg::Rebuild);
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
    /// Run `core` on its own thread. `file`: where to save paper state (with the paper
    /// broker's config, needed to write it).
    pub fn start(
        core: TradingCore,
        file: Option<(PathBuf, PaperConfig)>,
    ) -> Result<TradingService> {
        let (tx, rx) = mpsc::channel();
        let handle = TradingHandle {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        let thread = thread::Builder::new()
            .name("lines-trading".into())
            .spawn(move || run(core, rx, file))?;
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
            })
            .collect(),
        next_order_id: core.next_order_id(),
    })
}

fn run(mut core: TradingCore, rx: Receiver<Msg>, file: Option<(PathBuf, PaperConfig)>) {
    let mut subscribers: Vec<Sender<Event>> = Vec::new();
    let mut dirty = false;
    let mut last_save = Instant::now();
    loop {
        let now = lines_feed::now_utc();
        let wait = core
            .next_wake()
            .map(|t| Duration::from_millis((t.0 - now.0).clamp(0, 100) as u64))
            .unwrap_or(Duration::from_millis(100));
        // Handle the first message, then everything already queued, before the broker tick
        // and the broadcast (a burst of clicks doesn't wait behind per-message overhead).
        let mut msg = rx.recv_timeout(wait);
        let mut stop = false;
        loop {
            match msg {
                Ok(Msg::Command(env)) => {
                    core.handle(lines_feed::now_utc(), env);
                    dirty = true;
                }
                Ok(Msg::Market(k, inputs)) => core.market(lines_feed::now_utc(), k, &inputs),
                Ok(Msg::Subscribe(tx)) => subscribers.push(tx),
                Ok(Msg::Rebuild) => core.rebuild(lines_feed::now_utc()),
                Ok(Msg::Latency(tx)) => {
                    let l = core.latency();
                    let _ = tx.send((l.percentile(50.0), l.percentile(99.0), l.count()));
                }
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
        core.tick(lines_feed::now_utc());
        let events = core.drain();
        if !events.is_empty() {
            dirty = true;
            subscribers.retain(|s| events.iter().all(|e| s.send(e.clone()).is_ok()));
        }
        if dirty && last_save.elapsed() >= Duration::from_secs(1) {
            save(&core, &file);
            dirty = false;
            last_save = Instant::now();
        }
    }
    save(&core, &file);
}

fn save(core: &TradingCore, file: &Option<(PathBuf, PaperConfig)>) {
    if let Some((path, paper)) = file
        && let Some(f) = trading_file(core, paper)
        && let Err(e) = crate::persist::save(path, &f)
    {
        tracing::error!("can't save {}: {e:#}", path.display());
    }
}

/// Market data for the trading core from running feeds: subscribes to a contract through the
/// shared feed registry (decision 0013: always a contract) and forwards its batches.
pub struct FeedBridge {
    registry: FeedRegistry,
    base: FeedConfig,
    subs: HashMap<ContractKey, Subscription>,
}

impl FeedBridge {
    /// `base`: the data source (its target is replaced per contract).
    pub fn new(registry: FeedRegistry, base: FeedConfig) -> FeedBridge {
        FeedBridge {
            registry,
            base,
            subs: HashMap::new(),
        }
    }

    /// Start forwarding a contract's market data to the trading core (once per contract).
    pub fn follow(&mut self, contract: ContractKey, handle: &TradingHandle) -> Result<()> {
        if self.subs.contains_key(&contract) {
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
                }
            })?;
        self.subs.insert(contract, sub);
        Ok(())
    }
}
