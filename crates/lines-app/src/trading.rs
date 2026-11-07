//! The app ↔ trading core bridge (roadmap 7; decisions 0012, 0018).
//!
//! The trading core runs on its own thread (`lines_trading::TradingService`); the app holds a
//! [`Trading`] global with its handle. UI actions **send commands** and return at once: a click
//! is a channel send stamped with the click's `Instant` (the click → order-sent metric), never
//! a wait. Nothing here takes a lock the core holds.
//!
//! **Events in, like market data:** a pump thread reads the core's event stream into a
//! [`TradeState`] cache (behind its own mutex, never shared with the core) and bumps a lock-free
//! revision per account. A pane polls its account's revision with its store's (8 ms, only
//! while visible), redraws when it moved, and `try_lock`s the cache in paint, so a busy core
//! never stalls a frame and a pane only redraws for its own account.
//!
//! **Market data for paper fills:** a pane showing a contract makes the core follow that
//! pane's running feed (`FeedBridge::follow_store`), so the paper broker sees exactly what the
//! DOM shows, from the same connection or replay.
//!
//! **Accounts:** paper only for now (decision 0018: the account decides the broker and the
//! rules). The paper state lives in `<config dir>/paper/<source>.json` per data source (paper
//! fills on one source's prices never mark against another's); replays and screenshots keep
//! theirs in memory only (market time runs in the past). A damaged file is moved aside, never
//! overwritten, and reported.
//!
//! **Journal** (roadmap 8c): the trading thread also feeds `lines_trading::journal` (round
//! trips from fills, MAE/MFE from the same market batches), saved append-only to
//! `<config dir>/journal/<source>.jsonl` beside the paper file, under the same rule: replays
//! and screenshots keep it in memory. A journal this build can't read (newer, damaged
//! header) is left as it is and nothing is written to it this run.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gpui::{App, Global};
use lines_core::{ContractKey, Price};
use lines_feed::FeedConfig;
use lines_feed::registry::FeedRegistry;
use lines_feed::store::ChartStore;
use lines_trading::order::Order;
use lines_trading::persist;
use lines_trading::rules;
use lines_trading::service::Clock;
use lines_trading::{
    AccountConfig, AccountId, AccountView, Command as TradeCommand, CommandId, Event, FeedBridge,
    OrderId, OrderKind, PaperBroker, PaperConfig, PositionView, Reject, RulePreset, Source,
    TradingCore, TradingHandle, TradingService,
};
use lines_trading::{GuardState, Guardrails, Journal, JournalShared};

/// How long a rejection stays on screen where it happened.
pub const FLASH: Duration = Duration::from_secs(4);
/// Command results kept for drag ghosts (newest last).
const KEEP_RESULTS: usize = 256;
/// Finished orders kept (a ghost may need to learn its order filled).
const KEEP_FINISHED: usize = 256;

/// One account as the UI shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountInfo {
    pub id: AccountId,
    pub name: String,
    /// Paper accounts only for now; live ones come with the private connectors (0014).
    pub paper: bool,
    /// The prop preset, for the rule strip's limits.
    pub rules: Option<RulePreset>,
}

/// Whose refusal a flash reports (docs/inspiration.md: denied is not rejected).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlashKind {
    /// Refused by the trading core (validation, risk, prop rules): never sent.
    Denied,
    /// Sent, refused by the broker.
    Rejected,
    /// Something the broker did that shouldn't happen (an overfill).
    Warning,
}

impl FlashKind {
    pub fn label(self) -> &'static str {
        match self {
            FlashKind::Denied => "DENIED",
            FlashKind::Rejected => "REJECTED",
            FlashKind::Warning => "WARNING",
        }
    }
}

/// A refusal shown inline where it happened.
#[derive(Clone, Debug, PartialEq)]
pub struct Flash {
    pub kind: FlashKind,
    pub account: AccountId,
    pub contract: Option<ContractKey>,
    /// The order's price, to show it on its row (none: the header).
    pub price: Option<Price>,
    pub message: String,
    pub at: Instant,
}

/// What the UI remembers about a command it sent, to place the answer.
#[derive(Clone, Debug)]
struct Note {
    account: AccountId,
    contract: Option<ContractKey>,
    price: Option<Price>,
}

/// The UI's copy of the trading state, built from events only.
#[derive(Default)]
pub struct TradeState {
    /// Open orders.
    orders: BTreeMap<OrderId, Order>,
    /// Sent with no known outcome (after a restart or reconnect): listed, never drawn as
    /// working.
    unresolved: BTreeMap<OrderId, Order>,
    finished: VecDeque<Order>,
    positions: HashMap<(AccountId, ContractKey), PositionView>,
    accounts: HashMap<AccountId, AccountView>,
    flashes: VecDeque<Flash>,
    results: VecDeque<(CommandId, Result<Vec<OrderId>, Reject>)>,
    notes: HashMap<CommandId, Note>,
    /// Rebuilds seen (from the broker after a (re)connect).
    pub rebuilds: u64,
    /// Each account's guardrail state (roadmap 8b), from `Event::Guardrail`.
    guards: HashMap<AccountId, GuardView>,
}

/// An account's guardrail state as the UI shows it: a cooldown or lockout, why, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardView {
    pub state: GuardState,
    pub reason: Option<String>,
    pub until: Option<lines_core::UtcMillis>,
}

impl TradeState {
    /// Apply one event; returns the account it concerns.
    fn apply(&mut self, e: Event, now: Instant) -> Option<AccountId> {
        match e {
            Event::Command { id, result, .. } => {
                let note = self.notes.remove(&id);
                if let (Err(r), Some(n)) = (&result, &note) {
                    self.flashes.push_back(Flash {
                        kind: FlashKind::Denied,
                        account: n.account.clone(),
                        contract: n.contract,
                        price: n.price,
                        message: r.message.clone(),
                        at: now,
                    });
                }
                if self.results.len() == KEEP_RESULTS {
                    self.results.pop_front();
                }
                self.results.push_back((id, result));
                note.map(|n| n.account)
            }
            Event::Order(o) => {
                let account = o.account.clone();
                if let lines_trading::OrderState::Rejected { reason } = &o.state {
                    self.flashes.push_back(Flash {
                        kind: FlashKind::Rejected,
                        account: o.account.clone(),
                        contract: Some(o.contract),
                        price: crate::dom_trade::order_price(&o),
                        message: reason.clone(),
                        at: now,
                    });
                }
                if o.state == lines_trading::OrderState::Unresolved {
                    self.orders.remove(&o.id);
                    self.unresolved.insert(o.id, o);
                } else if o.is_open() {
                    self.unresolved.remove(&o.id);
                    self.orders.insert(o.id, o);
                } else {
                    self.unresolved.remove(&o.id);
                    self.orders.remove(&o.id);
                    if self.finished.len() == KEEP_FINISHED {
                        self.finished.pop_front();
                    }
                    self.finished.push_back(o);
                }
                Some(account)
            }
            Event::Fill(f) => Some(f.account),
            Event::Position(p) => {
                let account = p.account.clone();
                self.positions.insert((p.account.clone(), p.contract), p);
                Some(account)
            }
            Event::Account(a) => {
                let account = a.account.clone();
                self.accounts.insert(a.account.clone(), a);
                Some(account)
            }
            Event::Breach { account, .. } => Some(account),
            // Strategies (decision 0025) have no pane yet: the strategy panel comes later.
            Event::Strategy { .. } => None,
            Event::Guardrail {
                account,
                state,
                reason,
                until,
            } => {
                self.guards.insert(
                    account.clone(),
                    GuardView {
                        state,
                        reason,
                        until,
                    },
                );
                Some(account)
            }
            Event::Rebuilt { .. } => {
                self.rebuilds += 1;
                None
            }
            Event::Overfill {
                account,
                order,
                excess,
                recorded,
            } => {
                self.flashes.push_back(Flash {
                    kind: FlashKind::Warning,
                    account: account.clone(),
                    contract: None,
                    price: None,
                    message: format!(
                        "order {} filled {} more than its quantity ({})",
                        order.0,
                        excess.0,
                        if recorded { "recorded" } else { "refused" }
                    ),
                    at: now,
                });
                Some(account)
            }
        }
    }

    /// What the UI sent with command `id`. If the answer is already here (the note lost the
    /// race) and was a refusal, it flashes now. Returns whether it did.
    fn note(&mut self, id: CommandId, n: Note, now: Instant) -> bool {
        match self.result(id) {
            Some(Err(r)) => {
                let message = r.message.clone();
                self.flashes.push_back(Flash {
                    kind: FlashKind::Denied,
                    account: n.account,
                    contract: n.contract,
                    price: n.price,
                    message,
                    at: now,
                });
                true
            }
            Some(Ok(_)) => false,
            None => {
                self.notes.insert(id, n);
                false
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .flashes
            .front()
            .is_some_and(|f| now.duration_since(f.at) > FLASH)
        {
            self.flashes.pop_front();
        }
    }

    pub fn open_orders<'a>(
        &'a self,
        account: &'a AccountId,
        contract: ContractKey,
    ) -> impl Iterator<Item = &'a Order> + 'a {
        self.orders
            .values()
            .filter(move |o| &o.account == account && o.contract == contract)
    }

    /// An order, open or recently finished.
    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders
            .get(&id)
            .or_else(|| self.finished.iter().rev().find(|o| o.id == id))
    }

    pub fn position(&self, account: &AccountId, contract: ContractKey) -> Option<&PositionView> {
        self.positions
            .get(&(account.clone(), contract))
            .filter(|p| p.qty != 0)
    }

    /// How many of the account's orders have no known outcome.
    pub fn unresolved(&self, account: &AccountId) -> usize {
        self.unresolved
            .values()
            .filter(|o| &o.account == account)
            .count()
    }

    pub fn account(&self, account: &AccountId) -> Option<&AccountView> {
        self.accounts.get(account)
    }

    /// The account's guardrail lockout, if one is in force (`None`: active).
    pub fn guard(&self, account: &AccountId) -> Option<&GuardView> {
        self.guards
            .get(account)
            .filter(|g| g.state != GuardState::Active)
    }

    pub fn result(&self, id: CommandId) -> Option<&Result<Vec<OrderId>, Reject>> {
        self.results
            .iter()
            .rev()
            .find(|(c, _)| *c == id)
            .map(|(_, r)| r)
    }

    /// Recent rejections for this account (and contract, or the account's header).
    pub fn flashes<'a>(
        &'a self,
        account: &'a AccountId,
        contract: Option<ContractKey>,
        now: Instant,
    ) -> impl Iterator<Item = &'a Flash> + 'a {
        self.flashes.iter().filter(move |f| {
            &f.account == account
                && (f.contract.is_none() || f.contract == contract)
                && now.duration_since(f.at) <= FLASH
        })
    }
}

/// The cache and its revisions, shared by the pump thread and the UI.
pub struct TradeCache {
    state: Mutex<TradeState>,
    /// One per account (fixed at start, so reads are lock-free).
    revisions: HashMap<AccountId, AtomicU64>,
    /// Bumped by anything account-independent (rebuilds).
    global: AtomicU64,
}

impl TradeCache {
    fn new(accounts: &[AccountInfo]) -> TradeCache {
        TradeCache {
            state: Mutex::default(),
            revisions: accounts
                .iter()
                .map(|a| (a.id.clone(), AtomicU64::new(0)))
                .collect(),
            global: AtomicU64::new(0),
        }
    }

    /// The account's revision (lock-free): changes whenever its orders, positions, P&L or
    /// rule state did.
    pub fn revision(&self, account: &AccountId) -> u64 {
        self.global.load(Ordering::Acquire)
            + self
                .revisions
                .get(account)
                .map_or(0, |r| r.load(Ordering::Acquire))
    }

    /// For paint: never waits.
    pub fn try_read(&self) -> Option<MutexGuard<'_, TradeState>> {
        self.state.try_lock().ok()
    }

    /// Off the paint path (tests, screenshots).
    pub fn read(&self) -> MutexGuard<'_, TradeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self, account: Option<&AccountId>) {
        match account.and_then(|a| self.revisions.get(a)) {
            Some(r) => r.fetch_add(1, Ordering::AcqRel),
            None => self.global.fetch_add(1, Ordering::AcqRel),
        };
    }
}

/// Where the trading state comes from and goes.
pub struct TradingSetup {
    /// Paper state file (`None`: in memory only).
    pub file: Option<PathBuf>,
    pub clock: Clock,
    pub paper: PaperConfig,
    /// Accounts for a fresh state (default: [`default_accounts`]).
    pub accounts: Option<Vec<AccountConfig>>,
    /// Whether the paper broker refuses orders while the product's exchange session is
    /// closed. Applied over a saved paper file's setting too. Off for the mock: it is
    /// synthetic and trades around the clock, so the exchange's hours don't apply to it.
    pub check_sessions: bool,
    /// The trader's guardrails per account, keyed by account name (else id), from
    /// `settings.json` (roadmap 8b). Asked for at every start: tighter ones apply at once,
    /// looser ones with the next trade date; a saved lockout stays.
    pub guardrails: BTreeMap<String, Guardrails>,
    /// The trade journal's file and the source label in its header (`None`: in memory).
    pub journal: Option<(PathBuf, String)>,
}

impl TradingSetup {
    /// Live sources keep paper state per source; replays, the mock with `--record`, and
    /// screenshots don't persist.
    pub fn for_source(config_dir: Option<&Path>, source: &str, replay: bool) -> TradingSetup {
        TradingSetup {
            file: (!replay)
                .then(|| config_dir.map(|d| d.join("paper").join(format!("{source}.json"))))
                .flatten(),
            clock: if replay { Clock::Market } else { Clock::Wall },
            paper: PaperConfig::default(),
            accounts: None,
            check_sessions: true,
            guardrails: BTreeMap::new(),
            journal: (!replay)
                .then(|| {
                    config_dir.map(|d| {
                        (
                            d.join("journal").join(format!("{source}.jsonl")),
                            source.to_owned(),
                        )
                    })
                })
                .flatten(),
        }
    }

    /// The app's setup for its data source.
    pub fn for_live(config_dir: Option<&Path>, source: &lines_feed::Source) -> TradingSetup {
        use lines_feed::Source as S;
        let (label, replay) = match source {
            S::CqgMock(_) => ("mock".to_owned(), false),
            S::Coinbase { .. } => ("coinbase".to_owned(), false),
            S::Kraken { .. } => ("kraken".to_owned(), false),
            S::Replay { .. } => ("replay".to_owned(), true),
            // A provider extension (decision 0020): its own file, named after it.
            S::Extension(e) => (
                e.name()
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() {
                            c.to_ascii_lowercase()
                        } else {
                            '-'
                        }
                    })
                    .collect(),
                false,
            ),
            #[allow(unreachable_patterns)]
            _ => ("cqg".to_owned(), false),
        };
        let mut setup = TradingSetup::for_source(config_dir, &label, replay);
        // The mock is synthetic and never closes: CME's hours don't apply to it. Real sources
        // keep the check (wall clock), replays too (market clock: the recorded session).
        setup.check_sessions = !matches!(source, S::CqgMock(_));
        setup
    }

    /// Screenshots and tests: in memory, time moves only with market data and `advance_to`.
    pub fn manual() -> TradingSetup {
        TradingSetup {
            file: None,
            clock: Clock::Manual,
            paper: PaperConfig::default(),
            accounts: None,
            check_sessions: true,
            guardrails: BTreeMap::new(),
            journal: None,
        }
    }
}

/// The paper accounts a fresh install starts with: a plain one, and one with a prop preset
/// (so the rule strip has something to show; the preset is labelled an approximation).
pub fn default_accounts() -> Vec<(AccountConfig, Option<lines_trading::RuleTracker>)> {
    let mut plain = AccountConfig::paper("paper", 50_000);
    plain.name = "Paper".into();
    let mut eval = AccountConfig::paper("paper-eval", 50_000);
    eval.name = "Paper 50K eval".into();
    eval.rules = rules::presets().into_iter().nth(1);
    vec![(plain, None), (eval, None)]
}

/// The app's trading core.
pub struct Trading {
    handle: TradingHandle,
    /// Dropped with the global at exit: stops the thread after a last save.
    _service: Arc<TradingService>,
    bridge: FeedBridge,
    pub accounts: Vec<AccountInfo>,
    pub cache: Arc<TradeCache>,
    notes: Sender<(CommandId, Note)>,
    #[cfg_attr(not(any(test, feature = "screenshot")), allow(dead_code))]
    barrier: Sender<Sender<()>>,
    /// A problem loading the paper file (shown once in the workspace's status).
    pub report: Option<String>,
    /// Screenshot fixture (`--enable-trading`): DOMs start with trading enabled. Never set in
    /// the app (there, the switch is saved per pane).
    pub start_armed: bool,
    /// Screenshot fixture (`--account-menu`): charts and DOMs start with the account list
    /// open. Never set in the app.
    pub start_account_menu: bool,
    /// Screenshot fixture (`--bracket-editor`): DOMs open their bracket-preset editor.
    pub start_editor: bool,
    /// The paper broker refuses orders while the product's session is closed (the switch's
    /// "market closed" badge follows it; off for the mock).
    pub check_sessions: bool,
    /// Screenshot fixture (a `--paper` script's first `account`): the panes' account.
    pub start_account: Option<AccountId>,
    /// The trade journal (roadmap 8c), fed on the trading thread.
    pub journal: Arc<JournalShared>,
    /// The core's clock kind, so the UI reads time the way the core does (`now`).
    clock: Clock,
    /// Manual clock: the newest time a script advanced the core to (ms; `i64::MIN`: none).
    advanced: std::sync::atomic::AtomicI64,
}

impl Global for Trading {}

impl Trading {
    /// Build the paper core (from the setup's file, else the default accounts), start it and
    /// the event pump.
    pub fn start(mut setup: TradingSetup, registry: FeedRegistry, base: FeedConfig) -> Trading {
        let check_sessions = setup.check_sessions;
        let mut report = None;
        let journal_file = setup.journal.take();
        let (journal, journal_report) = open_journal(journal_file.as_ref());
        let loaded = match &setup.file {
            Some(path) => match persist::load(path) {
                Ok(f) => f,
                Err(e) => {
                    report = Some(set_aside(path, &e));
                    None
                }
            },
            None => None,
        };
        let (mut core, paper) = match loaded {
            Some(f) => {
                let mut paper = f.paper.clone();
                paper.check_sessions = setup.check_sessions;
                let mut core = TradingCore::new(Box::new(PaperBroker::from_state(paper, f.broker)));
                for a in f.accounts {
                    let id = a.config.id.clone();
                    core.add_account(a.config, Some(a.tracker));
                    // A lockout survives the restart.
                    core.restore_guardrails(&id, a.guardrails);
                }
                core.set_next_order_id(f.next_order_id);
                // Sent before the app stopped, no answer: unresolved until the broker says.
                core.restore_in_flight(f.in_flight);
                (core, f.paper)
            }
            None => {
                let mut paper = setup.paper.clone();
                paper.check_sessions = setup.check_sessions;
                let mut broker = PaperBroker::new(paper);
                let accounts = match setup.accounts {
                    Some(a) => a.into_iter().map(|c| (c, None)).collect(),
                    None => default_accounts(),
                };
                for (cfg, _) in &accounts {
                    broker.open_account(
                        cfg.id.clone(),
                        lines_trading::Money::whole(cfg.starting_balance),
                    );
                }
                let mut core = TradingCore::new(Box::new(broker));
                for (cfg, t) in accounts {
                    core.add_account(cfg, t);
                }
                (core, setup.paper)
            }
        };
        // The trader's guardrails from settings, through the command path (tighter now, looser
        // with the next trade date; a saved lockout is never cleared).
        let wanted: Vec<(AccountId, Guardrails)> = core
            .accounts()
            .filter_map(|(c, _)| {
                setup
                    .guardrails
                    .get(&c.name)
                    .or_else(|| setup.guardrails.get(c.id.0.as_str()))
                    .map(|g| (c.id.clone(), g.clone()))
            })
            .collect();
        for (account, guardrails) in wanted {
            core.submit(
                lines_feed::now_utc(),
                Source::Api,
                Instant::now(),
                TradeCommand::SetGuardrails {
                    account,
                    guardrails,
                },
            );
        }
        // Orders the broker still holds (brackets survive restarts) come back through rebuild.
        core.rebuild(lines_feed::now_utc());
        let _ = core.drain();
        let accounts: Vec<AccountInfo> = core
            .accounts()
            .map(|(c, _)| AccountInfo {
                id: c.id.clone(),
                name: c.name.clone(),
                paper: true,
                rules: c.rules.clone(),
            })
            .collect();
        let service = TradingService::start_observed(
            core,
            setup.file.map(|p| (p, paper)),
            setup.clock,
            vec![Box::new(journal.observer())],
        )
        .expect("spawn the trading thread");
        let report = match (report, journal_report) {
            (Some(a), Some(b)) => Some(format!("{a}  ·  {b}")),
            (a, b) => a.or(b),
        };
        let handle = service.handle();
        let cache = Arc::new(TradeCache::new(&accounts));
        let (notes, notes_rx) = mpsc::channel();
        let (barrier, barrier_rx) = mpsc::channel();
        spawn_pump(handle.subscribe(), notes_rx, barrier_rx, cache.clone());
        // The full state as the first events.
        handle.rebuild();
        Trading {
            bridge: FeedBridge::new(registry, base),
            handle,
            _service: Arc::new(service),
            accounts,
            cache,
            notes,
            barrier,
            report,
            start_armed: false,
            start_account_menu: false,
            start_editor: false,
            check_sessions,
            start_account: None,
            journal,
            clock: setup.clock,
            advanced: std::sync::atomic::AtomicI64::new(i64::MIN),
        }
    }

    /// Now, as the core sees it, for time left on screen (a cooldown): the wall clock live;
    /// in replays the market time plus the silence since (`ChartStore::market_clock`, as the
    /// core's market clock runs, also after the replay ends); with a manual clock
    /// (screenshots, tests) the newer of the frozen feed clock and the scripted time.
    pub fn now(&self, store: &ChartStore) -> Option<lines_core::UtcMillis> {
        use std::sync::atomic::Ordering;
        match self.clock {
            Clock::Wall => Some(lines_feed::now_utc()),
            Clock::Market => store.market_clock(),
            Clock::Manual => {
                let a = self.advanced.load(Ordering::Acquire);
                let f = store.feed_clock().map_or(i64::MIN, |t| t.0);
                (a.max(f) != i64::MIN).then(|| lines_core::UtcMillis(a.max(f)))
            }
        }
    }

    /// Advance a manual-clock core (scripts), remembering the time for [`Trading::now`].
    #[cfg(any(test, feature = "screenshot"))]
    fn advance_to(&self, t: lines_core::UtcMillis) {
        self.advanced
            .fetch_max(t.0, std::sync::atomic::Ordering::AcqRel);
        self.handle.advance_to(t);
    }

    pub fn try_global(cx: &App) -> Option<&Trading> {
        cx.try_global::<Trading>()
    }

    /// The latency HUD's order windows (roadmap 6b): read with `try_lock` only.
    pub fn order_latency(&self) -> &Arc<std::sync::Mutex<lines_trading::OrderLatency>> {
        self.handle.order_latency()
    }

    #[cfg(test)]
    pub fn handle(&self) -> &TradingHandle {
        &self.handle
    }

    pub fn account(&self, id: &AccountId) -> Option<&AccountInfo> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    /// The account a new pane trades on.
    pub fn default_account(&self) -> Option<AccountId> {
        self.start_account
            .clone()
            .filter(|a| self.account(a).is_some())
            .or_else(|| self.accounts.first().map(|a| a.id.clone()))
    }

    /// The account after `id` (wrapping), for the selector's hotkey.
    pub fn next_account(&self, id: Option<&AccountId>) -> Option<AccountId> {
        let i = id.and_then(|id| self.accounts.iter().position(|a| &a.id == id));
        let next = i.map_or(0, |i| (i + 1) % self.accounts.len().max(1));
        self.accounts.get(next).map(|a| a.id.clone())
    }

    /// Paper fills see the pane's own feed (once per feed).
    pub fn follow(&self, store: &Arc<ChartStore>) {
        self.bridge.follow_store(store, &self.handle);
    }

    /// Wait until everything sent so far is handled by the core and in the cache. Blocks:
    /// screenshots and tests only, never the UI's trading path.
    #[cfg(any(test, feature = "screenshot"))]
    pub fn settle(&self) {
        self.handle.sync();
        let (tx, rx) = mpsc::channel();
        if self.barrier.send(tx).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
        // The journal applied every event the core sent before the sync above.
        self.journal.sync();
    }

    /// Send a command; returns at once. `issued`: the input event's time (the click).
    pub fn submit(&self, source: Source, issued: Instant, cmd: TradeCommand) -> CommandId {
        let note = note_for(&cmd);
        let id = self.handle.submit_at(source, issued, cmd);
        // The pump pairs the note with the answer whichever arrives first.
        if let Some(note) = note {
            let _ = self.notes.send((id, note));
        }
        id
    }
}

/// The note for a command: its account, contract and price (`None`: strategy controls, which
/// name an instance and show in the strategy panel, decision 0025).
fn note_for(cmd: &TradeCommand) -> Option<Note> {
    let (account, contract, price) = match cmd {
        TradeCommand::PlaceOrder(o) => (o.account.clone(), Some(o.contract), first_price(&o.kind)),
        TradeCommand::PlaceBracket(b) => (
            b.entry.account.clone(),
            Some(b.entry.contract),
            first_price(&b.entry.kind),
        ),
        TradeCommand::ModifyOrder { account, price, .. } => (account.clone(), None, *price),
        TradeCommand::CancelOrder { account, .. } => (account.clone(), None, None),
        TradeCommand::CancelAll { account, contract }
        | TradeCommand::Flatten { account, contract } => (account.clone(), *contract, None),
        TradeCommand::SetGuardrails { account, .. } => (account.clone(), None, None),
        TradeCommand::AddStrategy(_)
        | TradeCommand::RemoveStrategy { .. }
        | TradeCommand::ArmStrategy { .. }
        | TradeCommand::PauseStrategy { .. }
        | TradeCommand::StopStrategies { .. } => return None,
    };
    Some(Note {
        account,
        contract,
        price,
    })
}

fn first_price(k: &OrderKind) -> Option<Price> {
    k.prices().next()
}

/// The journal from its file (if any): its trades and edits, and a writer appending to it.
/// A file this build can't read is left alone and not written this run (reported).
fn open_journal(file: Option<&(PathBuf, String)>) -> (Arc<JournalShared>, Option<String>) {
    use lines_trading::journal::store::{self, JournalWriter};
    let Some((path, source)) = file else {
        return (JournalShared::start(Journal::new(), None), None);
    };
    match store::read(path) {
        Ok(loaded) => {
            let mut report = loaded
                .as_ref()
                .filter(|l| l.skipped > 0)
                .map(|l| format!("journal: skipped {} unreadable lines", l.skipped));
            let writer = match JournalWriter::open(path.clone(), source, loaded.as_ref()) {
                Ok(w) => Some(w),
                Err(e) => {
                    report = Some(format!(
                        "journal {} can't be written ({e:#}); not saved this run",
                        path.display()
                    ));
                    None
                }
            };
            let journal = Journal::from_records(loaded.map(|l| l.records).unwrap_or_default());
            (JournalShared::start(journal, writer), report)
        }
        Err(e) => (
            JournalShared::start(Journal::new(), None),
            Some(format!(
                "journal {} not read ({e:#}); left as it is, not saved this run",
                path.display()
            )),
        ),
    }
}

/// Move a damaged paper file aside (never overwrite it) and say so.
fn set_aside(path: &Path, e: &anyhow::Error) -> String {
    let ms = lines_feed::now_utc().0;
    let aside = path.with_extension(format!("unreadable-{ms}.json"));
    match std::fs::rename(path, &aside) {
        Ok(()) => format!(
            "paper trading file unreadable ({e:#}); moved to {} and started fresh",
            aside.display()
        ),
        Err(re) => format!(
            "paper trading file unreadable ({e:#}) and couldn't be moved aside ({re}); paper state is not saved this run"
        ),
    }
}

/// The pump: core events → cache, then the touched accounts' revisions.
fn spawn_pump(
    events: Receiver<Event>,
    notes: Receiver<(CommandId, Note)>,
    barriers: Receiver<Sender<()>>,
    cache: Arc<TradeCache>,
) {
    let spawned = std::thread::Builder::new()
        .name("lines-trading-ui".into())
        .spawn(move || {
            loop {
                let first = match events.recv_timeout(Duration::from_millis(20)) {
                    Ok(e) => Some(e),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                let now = Instant::now();
                let mut touched: Vec<Option<AccountId>> = Vec::new();
                {
                    let mut s = cache.read();
                    while let Ok((id, n)) = notes.try_recv() {
                        if s.note(id, n, now) {
                            touched.push(None);
                        }
                    }
                    let before = s.flashes.len();
                    s.prune(now);
                    if s.flashes.len() != before {
                        touched.push(None);
                    }
                    let mut next = first;
                    while let Some(e) = next {
                        let a = s.apply(e, now);
                        if !touched.contains(&a) {
                            touched.push(a);
                        }
                        next = events.try_recv().ok();
                    }
                }
                for a in &touched {
                    cache.bump(a.as_ref());
                }
                // A barrier was sent after the events it waits for: drain once more, then
                // answer.
                let waiting: Vec<Sender<()>> = barriers.try_iter().collect();
                if !waiting.is_empty() {
                    let now = Instant::now();
                    let mut touched: Vec<Option<AccountId>> = Vec::new();
                    {
                        let mut s = cache.read();
                        while let Ok((id, n)) = notes.try_recv() {
                            if s.note(id, n, now) {
                                touched.push(None);
                            }
                        }
                        while let Ok(e) = events.try_recv() {
                            let a = s.apply(e, now);
                            if !touched.contains(&a) {
                                touched.push(a);
                            }
                        }
                    }
                    for a in &touched {
                        cache.bump(a.as_ref());
                    }
                    for w in waiting {
                        let _ = w.send(());
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!("can't start the trading event pump: {e}");
    }
}

// ---- scripted sessions (screenshots, tests) -----------------------------------------------

#[cfg(any(test, feature = "screenshot"))]
/// One step of a scripted paper session (`--paper` in screenshot mode).
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Command(TradeCommand),
    /// Let the paper broker's acks happen (time moves on by its latency).
    Ack,
    /// Use this account from here on.
    Account(AccountId),
    /// Market time moves on (ms).
    Wait(i64),
    /// A synthetic market trade the trading core sees (paper fills, the journal's MAE/MFE);
    /// the panes don't (screenshots and tests only).
    Print(ContractKey, Price, lines_core::Qty),
}

#[cfg(any(test, feature = "screenshot"))]
/// Parse a script: steps separated by `;`.
///
/// - `buy 2 mkt`, `sell 1 @ 6000.25`, `buy 1 stop 6010.00`, with an optional
///   `bracket STOP/TARGET` (ticks) after a limit or stop;
/// - `ack` (acks arrive; orders sent after the last `ack` stay pending);
/// - `flatten`, `cancelall` (the contract), `account ID`;
/// - `guard trades N | streak LOSSES/MINUTES | loss MONEY | position N | close MINUTES`: one
///   guardrail for the account (roadmap 8b; the others unchanged);
/// - `wait SECONDS` (market time moves on), `print PRICE [QTY]`: a synthetic market trade
///   only the trading core sees (resting orders fill, the journal's MAE/MFE move), for
///   round trips in a screenshot of a finished replay.
pub fn parse_script(
    s: &str,
    product: &lines_core::Product,
    contract: ContractKey,
    first_account: &AccountId,
) -> anyhow::Result<Vec<Step>> {
    use anyhow::{anyhow, bail};
    use lines_trading::{BracketSpec, OrderSpec, Side};
    let mut account = first_account.clone();
    let mut out = Vec::new();
    let mut guards: HashMap<AccountId, Guardrails> = HashMap::new();
    for raw in s.split(';') {
        let w: Vec<&str> = raw.split_whitespace().collect();
        let price = |t: &str| -> anyhow::Result<Price> {
            let units = product
                .price
                .scale
                .parse_decimal(t)
                .map_err(|e| anyhow!("price {t:?}: {e}"))?;
            if units % product.price.tick != 0 {
                bail!("price {t} is not on the tick grid");
            }
            Ok(Price(units))
        };
        match w.as_slice() {
            [] => {}
            ["ack"] => out.push(Step::Ack),
            ["wait", s] => out.push(Step::Wait(
                (s.parse::<f64>()
                    .map_err(|_| anyhow!("wait {s:?}: seconds"))?
                    * 1000.0) as i64,
            )),
            ["print", p, rest @ ..] => {
                let q = match rest {
                    [] => 1,
                    [q] => q.parse().map_err(|_| anyhow!("print quantity {q:?}"))?,
                    _ => bail!("{raw:?}: print PRICE [QTY]"),
                };
                out.push(Step::Print(contract, price(p)?, lines_core::Qty(q)));
            }
            ["account", id] => {
                account = AccountId::new(id);
                out.push(Step::Account(account.clone()));
            }
            ["flatten"] => out.push(Step::Command(TradeCommand::Flatten {
                account: account.clone(),
                contract: Some(contract),
            })),
            ["cancelall"] => out.push(Step::Command(TradeCommand::CancelAll {
                account: account.clone(),
                contract: Some(contract),
            })),
            ["guard", key, value] => {
                let n = |v: &str| -> anyhow::Result<i64> {
                    v.parse()
                        .map_err(|_| anyhow!("guard {key}: {v:?} is not a number"))
                };
                let g = guards.entry(account.clone()).or_default();
                match *key {
                    "trades" => g.max_trades_per_day = Some(n(value)? as u32),
                    "loss" => g.max_daily_loss = Some(n(value)?),
                    "position" => g.max_position = Some(n(value)?),
                    "close" => g.no_entries_before_close_min = Some(n(value)? as u32),
                    "streak" => {
                        let (l, m) = value
                            .split_once('/')
                            .ok_or_else(|| anyhow!("guard streak LOSSES/MINUTES"))?;
                        g.losing_streak = Some(lines_trading::LosingStreak {
                            losses: n(l)? as u32,
                            cooldown_min: n(m)? as u32,
                        });
                    }
                    _ => bail!("guard {key:?}: trades, streak, loss, position or close"),
                }
                out.push(Step::Command(TradeCommand::SetGuardrails {
                    account: account.clone(),
                    guardrails: g.clone(),
                }));
            }
            [side @ ("buy" | "sell"), qty, rest @ ..] => {
                let side = if *side == "buy" {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let qty = lines_core::Qty(qty.parse().map_err(|_| anyhow!("quantity {qty:?}"))?);
                let (kind, rest) = match rest {
                    ["mkt", rest @ ..] => (OrderKind::Market, rest),
                    ["@", p, rest @ ..] => (OrderKind::Limit { price: price(p)? }, rest),
                    ["stop", p, rest @ ..] => (OrderKind::Stop { trigger: price(p)? }, rest),
                    _ => bail!("{raw:?}: expected `mkt`, `@ PRICE` or `stop PRICE`"),
                };
                let spec = OrderSpec {
                    account: account.clone(),
                    contract,
                    side,
                    qty,
                    kind,
                };
                out.push(Step::Command(match rest {
                    [] => TradeCommand::PlaceOrder(spec),
                    ["bracket", st] => {
                        let (stop, target) = st
                            .split_once('/')
                            .and_then(|(a, b)| {
                                Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?))
                            })
                            .ok_or_else(|| anyhow!("bracket {st:?}: STOP/TARGET ticks"))?;
                        let at = spec
                            .kind
                            .prices()
                            .next()
                            .ok_or_else(|| anyhow!("a market order can't carry a bracket"))?;
                        let s = side.sign();
                        let tick = product.price.tick;
                        TradeCommand::PlaceBracket(BracketSpec {
                            entry: spec,
                            take_profit: Some(Price(at.0 + s * target * tick)),
                            stop_loss: Some(Price(at.0 - s * stop * tick)),
                        })
                    }
                    _ => bail!("{raw:?}: unexpected {rest:?}"),
                }));
            }
            _ => bail!("{raw:?}: not a paper script step"),
        }
    }
    Ok(out)
}

#[cfg(any(test, feature = "screenshot"))]
impl Trading {
    /// Run a script on a manual-clock core starting at market time `start`, then wait until
    /// the cache shows the result.
    pub fn run_script(&self, steps: &[Step], start: lines_core::UtcMillis, latency_ms: i64) {
        let mut t = start;
        self.advance_to(t);
        for s in steps {
            match s {
                Step::Command(c) => {
                    self.submit(Source::Test, Instant::now(), c.clone());
                }
                Step::Ack => {
                    t = lines_core::UtcMillis(t.0 + latency_ms + 1);
                    self.advance_to(t);
                }
                Step::Wait(ms) => {
                    t = lines_core::UtcMillis(t.0 + ms);
                    self.handle.advance_to(t);
                }
                Step::Print(k, price, qty) => {
                    self.handle.market(
                        *k,
                        vec![lines_trading::MarketInput::Trade {
                            time: t,
                            price: *price,
                            qty: *qty,
                        }],
                    );
                }
                Step::Account(_) => {}
            }
        }
        self.settle();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lines_core::{ContractKey, Month, Qty, UtcMillis};
    use lines_feed::native::{self, NativeEvent};
    use lines_feed::provider::Frame;
    use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
    use lines_feed::{Source as FeedSource, catalog, new_store, run_feed};
    use lines_trading::market::{MarketInput, from_events};
    use lines_trading::{Money, OrderSpec, OrderState, RejectCode, RiskLimits, Side};

    use super::*;
    use crate::dom_trade::{self, ArmEvent, ArmPrefs, Arming, DomColumn, EntryCtx};

    fn es() -> ContractKey {
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
    }

    fn px(p: f64) -> Price {
        Price((p * 100.0).round() as i64)
    }

    fn start() -> UtcMillis {
        UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
    }

    /// Ten minutes of ES (a Tuesday afternoon, market open): a static book, a trade every
    /// second around 6000, a dip to 5999.25 at +150 s and a pop to 6001.00 at +300 s.
    fn recording(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lines-app-trading-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("es.lfr");
        let t0 = start();
        let mut w = Writer::create(
            &path,
            &Header {
                start_wall_ms: t0.0,
                provider: native::PROVIDER.into(),
                protocol: native::PROTOCOL.into(),
                product: "ES".into(),
                contract: Some(es()),
                version: FORMAT_VERSION,
            },
        )
        .unwrap();
        let us = |s: i64| (t0.0 + s * 1000) * 1000;
        let mut frame = |s: i64, evs: Vec<NativeEvent>| {
            let mut buf = Vec::new();
            for e in &evs {
                native::encode(e, &mut buf);
            }
            w.write(
                Direction::FromServer,
                Duration::from_secs(s as u64),
                &Frame::binary(buf),
            )
            .unwrap();
        };
        let level = |side, p: f64| NativeEvent::BookLevel {
            time_us: us(0),
            side,
            price: px(p),
            qty: Qty(10),
            orders: 0,
        };
        use lines_core::Side::{Ask, Bid};
        frame(
            0,
            vec![
                NativeEvent::BookClear { time_us: us(0) },
                level(Bid, 5999.75),
                level(Bid, 5999.50),
                level(Bid, 5999.25),
                level(Ask, 6000.00),
                level(Ask, 6000.25),
                level(Ask, 6000.50),
            ],
        );
        for s in 1..600 {
            let (p, q) = match s {
                150 => (5999.25, 3),
                300 => (6001.00, 5),
                s if s % 2 == 0 => (6000.00, 1),
                _ => (5999.75, 1),
            };
            frame(
                s,
                vec![NativeEvent::Trade {
                    time_us: us(s),
                    price: px(p),
                    qty: Qty(q),
                    aggressor: native::Aggressor::Unknown,
                }],
            );
        }
        w.finish().unwrap();
        path
    }

    /// The recording's feed batches, through the real feed driver.
    fn batches(tag: &str) -> Vec<Arc<[lines_feed::provider::MarketEvent]>> {
        let config = FeedConfig::new(FeedSource::Replay {
            path: recording(tag),
            speed: 0.0,
            stop_at: None,
        });
        let store = new_store(&config).unwrap();
        let rx = lines_feed::listen::listen(store.feed_id());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_tx, srx) = tokio::sync::watch::channel(false);
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), run_feed(config, store, srx))
                .await
                .unwrap()
        });
        rx.try_iter().collect()
    }

    fn paper_setup(accounts: Vec<AccountConfig>) -> TradingSetup {
        let mut paper = PaperConfig {
            latency_ms: 25,
            ..Default::default()
        };
        paper.fees.insert(catalog::ES.0, Money::cents(100));
        TradingSetup {
            file: None,
            clock: Clock::Manual,
            paper,
            accounts: Some(accounts),
            check_sessions: true,
            guardrails: BTreeMap::new(),
            journal: None,
        }
    }

    fn trading(accounts: Vec<AccountConfig>) -> Trading {
        trading_with(paper_setup(accounts))
    }

    fn trading_with(setup: TradingSetup) -> Trading {
        let base = FeedConfig::new(FeedSource::Replay {
            path: "unused.lfr".into(),
            speed: 0.0,
            stop_at: None,
        });
        Trading::start(setup, FeedRegistry::new(), base)
    }

    /// The owner's report (live, `--source mock`, Friday 22:18 UTC): every paper order was
    /// refused "ES market is closed". The mock is synthetic and trades around the clock, so
    /// its paper broker doesn't apply the exchange's hours; real sources and replays do.
    #[test]
    fn mock_paper_trades_outside_exchange_hours() {
        let mock = TradingSetup::for_live(
            None,
            &FeedSource::CqgMock(lines_feed::cqg::mock::MockConfig::default()),
        );
        assert!(!mock.check_sessions, "the mock never closes");
        let replay = TradingSetup::for_live(
            None,
            &FeedSource::Replay {
                path: "x.lfr".into(),
                speed: 0.0,
                stop_at: None,
            },
        );
        assert!(
            replay.check_sessions,
            "replays keep the session check (market clock)"
        );

        // Friday 2026-11-06 22:18 UTC: CME is closed.
        let closed = UtcMillis::parse_iso("2026-11-06T22:18:00Z").unwrap();
        let account = AccountId::new("paper");
        for check in [false, true] {
            let mut setup = paper_setup(vec![AccountConfig::paper("paper", 50_000)]);
            setup.check_sessions = check;
            let t = trading_with(setup);
            t.handle().advance_to(closed);
            t.handle().market(
                es(),
                vec![
                    MarketInput::BookClear,
                    MarketInput::Book {
                        side: lines_core::Side::Bid,
                        price: px(5999.75),
                        qty: Qty(10),
                    },
                    MarketInput::Book {
                        side: lines_core::Side::Ask,
                        price: px(6000.00),
                        qty: Qty(10),
                    },
                ],
            );
            let cmd = dom_trade::click(
                DomColumn::Bid,
                px(5999.00),
                false,
                armed(),
                &entry("paper", 0, (5999.75, 6000.00)),
            )
            .unwrap();
            t.submit(Source::Dom, Instant::now(), cmd);
            t.handle().advance_to(UtcMillis(closed.0 + 100)); // past the ack latency
            t.settle();
            let s = t.cache.read();
            if check {
                assert_eq!(s.open_orders(&account, es()).count(), 0);
                let f: Vec<_> = s.flashes(&account, Some(es()), Instant::now()).collect();
                assert!(
                    f.iter().any(|f| f.message.contains("market is closed")),
                    "{f:?}"
                );
            } else {
                assert_eq!(
                    s.open_orders(&account, es())
                        .map(|o| o.state.clone())
                        .collect::<Vec<_>>(),
                    vec![lines_trading::OrderState::Working],
                    "accepted on the mock outside CME hours"
                );
            }
        }
    }

    /// Roadmap 8b: guardrails from settings reach the core at start, a guardrail denial reaches
    /// the DOM header as a short reason, and a lockout reaches the cache.
    #[test]
    fn guardrails_from_settings_deny_and_report() {
        let mut setup = paper_setup(vec![AccountConfig::paper("paper", 50_000)]);
        setup.guardrails.insert(
            "paper".into(),
            Guardrails {
                max_position: Some(1),
                ..Default::default()
            },
        );
        let base = FeedConfig::new(FeedSource::Replay {
            path: "unused.lfr".into(),
            speed: 0.0,
            stop_at: None,
        });
        let t = Trading::start(setup, FeedRegistry::new(), base);
        let account = AccountId::new("paper");
        let id = t.submit(
            Source::Dom,
            Instant::now(),
            TradeCommand::PlaceOrder(OrderSpec {
                account: account.clone(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(2),
                kind: OrderKind::Limit { price: px(5990.00) },
            }),
        );
        t.settle();
        let s = t.cache.read();
        let Some(Err(r)) = s.result(id) else {
            panic!("not denied: {:?}", s.result(id))
        };
        assert_eq!(r.code, RejectCode::Guardrail);
        let flash = s
            .flashes(&account, Some(es()), Instant::now())
            .next()
            .unwrap();
        assert_eq!(flash.kind.label(), "DENIED");
        assert_eq!(flash.message, "Max position: would be 2, your limit is 1");
        assert_eq!(s.guard(&account), None, "a denial, not a lockout");
        drop(s);
        // A lockout as the pump receives it: shown until it ends.
        let mut st = TradeState::default();
        let until = UtcMillis(1_800_000_000_000);
        st.apply(
            Event::Guardrail {
                account: account.clone(),
                state: GuardState::Cooldown,
                reason: Some("3 losses in a row".into()),
                until: Some(until),
            },
            Instant::now(),
        );
        assert_eq!(
            st.guard(&account),
            Some(&GuardView {
                state: GuardState::Cooldown,
                reason: Some("3 losses in a row".into()),
                until: Some(until),
            })
        );
        st.apply(
            Event::Guardrail {
                account: account.clone(),
                state: GuardState::Active,
                reason: None,
                until: None,
            },
            Instant::now(),
        );
        assert_eq!(st.guard(&account), None);
    }

    fn entry(account: &str, position: i64, best: (f64, f64)) -> EntryCtx {
        EntryCtx {
            account: AccountId::new(account),
            contract: es(),
            qty: Qty(2),
            position,
            working: 0,
            bracket: None,
            tick: 25,
            best_bid: Some(px(best.0)),
            best_ask: Some(px(best.1)),
            last: Some(px(best.0)),
        }
    }

    fn armed() -> Arming {
        let mut a = Arming::default();
        a.apply(ArmEvent::Toggle, ArmPrefs::default());
        a
    }

    /// The UI command path end to end: DOM clicks (`dom_trade::click`) → `Trading::submit`
    /// → the core on its thread → the event pump → the cache the panes read. Market data is
    /// a replayed session, so the fills and P&L are exact.
    #[test]
    fn replayed_session_through_the_ui_command_path_fills_exactly() {
        let t = trading(vec![AccountConfig::paper("paper", 50_000)]);
        let account = AccountId::new("paper");
        let t0 = start().0;
        // Clicks at session times: buy 2 at 5999.50 (bid column), later sell 2 at 6000.75
        // (ask column). Grouping 1: the row's price is the click's price.
        let clicks = [
            (60, DomColumn::Bid, 5999.50),
            (200, DomColumn::Ask, 6000.75),
        ];
        let mut next = 0;
        for batch in batches("ui") {
            let inputs = from_events(&batch);
            let time = inputs.iter().find_map(|i| match i {
                MarketInput::Trade { time, .. } => Some(time.0),
                _ => None,
            });
            if let Some(time) = time {
                while next < clicks.len() && t0 + clicks[next].0 * 1000 <= time {
                    let (_, col, p) = clicks[next];
                    let pos = t.cache.read().position(&account, es()).map_or(0, |p| p.qty);
                    let cmd = dom_trade::click(
                        col,
                        px(p),
                        false,
                        armed(),
                        &entry("paper", pos, (5999.75, 6000.00)),
                    )
                    .expect("an armed click places an order");
                    t.submit(Source::Dom, Instant::now(), cmd);
                    t.settle();
                    next += 1;
                }
            }
            t.handle().market(es(), inputs);
        }
        t.settle();
        let s = t.cache.read();
        assert!(s.position(&account, es()).is_none(), "flat again");
        let a = s.account(&account).expect("account state");
        // +1.25 points × 2 × $50 = $125, minus 4 × $1.00 fees: exactly.
        assert_eq!(
            a.balance,
            Money::whole(50_000) + Money::whole(125) - Money::whole(4)
        );
        let filled: Vec<(i64, Option<f64>)> = s
            .finished
            .iter()
            .filter(|o| o.state == OrderState::Filled)
            .map(|o| (o.filled.0, o.avg_fill()))
            .collect();
        assert_eq!(
            filled,
            vec![(2, Some(599_950.0)), (2, Some(600_075.0))],
            "both clicks filled at their limit"
        );
        // The click → order-sent metric saw both.
        let (p50, p99, n) = t.handle().latency();
        assert_eq!(n, 2);
        assert!(p50.is_some() && p99.is_some());
    }

    #[test]
    fn flatten_and_cancel_all_work_while_risk_blocks_new_orders() {
        let mut cfg = AccountConfig::paper("paper", 50_000);
        cfg.limits = RiskLimits {
            max_position: Some(2),
            ..Default::default()
        };
        let t = trading(vec![cfg]);
        let account = AccountId::new("paper");
        let feed: Vec<_> = batches("risk");
        // The book and the first minute of trades.
        for b in feed.iter().take(60) {
            t.handle().market(es(), from_events(b));
        }
        let click = |col, p: f64, pos| {
            dom_trade::click(
                col,
                px(p),
                false,
                armed(),
                &entry("paper", pos, (5999.75, 6000.00)),
            )
            .unwrap()
        };
        // Buy 2 through the market (fills on the ask), and a resting buy that risk refuses
        // (position 2 + 2 open > max 2), and a resting sell above.
        t.submit(
            Source::Dom,
            Instant::now(),
            click(DomColumn::Bid, 6000.00, 0),
        );
        t.settle();
        for b in feed.iter().skip(60).take(2) {
            t.handle().market(es(), from_events(b));
        }
        t.settle();
        assert_eq!(
            t.cache.read().position(&account, es()).map(|p| p.qty),
            Some(2)
        );
        let refused = t.submit(
            Source::Dom,
            Instant::now(),
            click(DomColumn::Bid, 5999.00, 2),
        );
        let resting = t.submit(
            Source::Dom,
            Instant::now(),
            click(DomColumn::Ask, 6002.00, 2),
        );
        t.settle();
        for b in feed.iter().skip(62).take(2) {
            t.handle().market(es(), from_events(b));
        }
        t.settle();
        {
            let s = t.cache.read();
            assert!(
                matches!(s.result(refused), Some(Err(r)) if r.code == RejectCode::Risk),
                "{:?}",
                s.result(refused)
            );
            // The refusal shows inline at its price, in this contract's DOM.
            let f: Vec<_> = s.flashes(&account, Some(es()), Instant::now()).collect();
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].price, Some(px(5999.00)));
            assert!(matches!(s.result(resting), Some(Ok(_))));
            assert_eq!(s.open_orders(&account, es()).count(), 1);
        }
        // Cancel All and Flatten as the header and hotkeys send them: never blocked.
        t.submit(
            Source::Hotkey,
            Instant::now(),
            TradeCommand::CancelAll {
                account: account.clone(),
                contract: Some(es()),
            },
        );
        t.submit(
            Source::Hotkey,
            Instant::now(),
            TradeCommand::Flatten {
                account: account.clone(),
                contract: Some(es()),
            },
        );
        t.settle();
        for b in feed.iter().skip(64).take(2) {
            t.handle().market(es(), from_events(b));
        }
        t.settle();
        let s = t.cache.read();
        assert_eq!(s.open_orders(&account, es()).count(), 0, "cancelled");
        assert!(s.position(&account, es()).is_none(), "flattened");
    }

    /// Click → order sent through the DOM's path: `dom_trade::click` builds the command,
    /// `Trading::submit` stamps it with the click's time and sends it; the core records the
    /// time to `Broker::request`. A click every millisecond, alternating a resting order and
    /// Cancel All. Returns (p50 µs, p99 µs, samples).
    fn dom_click_latency(clicks: usize) -> (Option<u64>, Option<u64>, usize) {
        let t = trading(vec![AccountConfig::paper("paper", 50_000)]);
        let account = AccountId::new("paper");
        for b in batches(&format!("latency-{clicks}")).iter().take(30) {
            t.handle().market(es(), from_events(b));
        }
        t.settle();
        let ctx = entry("paper", 0, (5999.75, 6000.00));
        for i in 0..clicks {
            let issued = Instant::now();
            let cmd = if i % 2 == 0 {
                dom_trade::click(DomColumn::Bid, px(5999.00), false, armed(), &ctx).unwrap()
            } else {
                TradeCommand::CancelAll {
                    account: account.clone(),
                    contract: Some(es()),
                }
            };
            t.submit(Source::Dom, issued, cmd);
            std::thread::sleep(Duration::from_millis(1));
        }
        t.settle();
        t.handle().latency()
    }

    /// The header's order buttons go the same way as ladder clicks: built by
    /// `dom_trade::quick`, submitted with the click's `Instant`, timed by the core. Join bid
    /// rests at the bid; Buy Mkt fills at the ask; Reverse flips long 2 to short 2.
    #[test]
    fn header_buttons_are_timed_and_reach_the_core() {
        use crate::dom_trade::{QuickOrder, quick};
        let t = trading(vec![AccountConfig::paper("paper", 50_000)]);
        let account = AccountId::new("paper");
        let feed = batches("buttons");
        for b in feed.iter().take(30) {
            t.handle().market(es(), from_events(b));
        }
        t.settle();
        // Fresh market data after each press, so time moves past the paper ack latency.
        let next = std::cell::Cell::new(30);
        let press = |q, pos| {
            let mut ctx = entry("paper", pos, (5999.75, 6000.00));
            ctx.qty = Qty(2);
            let cmd = quick(q, armed(), &ctx).expect("armed: an order");
            t.submit(Source::Dom, Instant::now(), cmd);
            t.settle();
            for b in feed.iter().skip(next.get()).take(2) {
                t.handle().market(es(), from_events(b));
            }
            next.set(next.get() + 2);
            t.settle();
        };
        press(QuickOrder::JoinBid, 0);
        let resting: Vec<_> = t
            .cache
            .read()
            .open_orders(&account, es())
            .cloned()
            .collect();
        assert_eq!(resting.len(), 1);
        assert_eq!(
            resting[0].kind,
            lines_trading::OrderKind::Limit { price: px(5999.75) }
        );
        t.submit(
            Source::Hotkey,
            Instant::now(),
            TradeCommand::CancelAll {
                account: account.clone(),
                contract: Some(es()),
            },
        );
        t.settle();
        press(QuickOrder::BuyMarket, 0);
        assert_eq!(
            t.cache.read().position(&account, es()).map(|p| p.qty),
            Some(2)
        );
        press(QuickOrder::Reverse, 2);
        assert_eq!(
            t.cache.read().position(&account, es()).map(|p| p.qty),
            Some(-2),
            "one market order for twice the position"
        );
        // Every order the buttons sent (and the cancel) carried a click time to the core.
        let (_, _, n) = t.handle().latency();
        assert_eq!(n, 4);
        // Disarmed: nothing is built, so nothing is sent.
        assert!(
            quick(
                QuickOrder::BuyMarket,
                Arming::default(),
                &entry("paper", 0, (1.0, 2.0))
            )
            .is_err()
        );
    }

    /// Every DOM click is timed (behaviour only; the budget is the perf test below).
    #[test]
    fn dom_clicks_record_click_to_order_sent() {
        let (p50, p99, n) = dom_click_latency(40);
        assert!(n >= 40, "{n}");
        assert!(p50.is_some() && p99.is_some());
    }

    /// The budget: under 1 ms from the DOM click to the order leaving the core. Perf test, run
    /// alone in release: `cargo test --release -p lines-app --lib -- --ignored
    /// dom_click_latency_budget --nocapture`.
    #[test]
    #[ignore = "perf: wall-clock budget; run alone in release"]
    fn dom_click_latency_budget() {
        let (p50, p99, n) = dom_click_latency(1000);
        println!("DOM click -> order sent: p50 {p50:?} us, p99 {p99:?} us over {n} commands");
        assert!(p99.unwrap() < 1_000, "p99 {p99:?} us");
    }

    /// The journal through the real service: a scripted paper session (two round trips, one
    /// a scale-out with a resting limit filled by a print) arrives as closed trades with
    /// exact P&L and fees, MAE/MFE from the prints, and the fills' source.
    #[test]
    fn paper_round_trips_reach_the_journal() {
        let t = trading(vec![AccountConfig::paper("paper", 50_000)]);
        let start = UtcMillis::parse_iso("2026-11-05T15:00:00Z").unwrap();
        t.handle().advance_to(start);
        t.handle().market(
            es(),
            vec![
                MarketInput::BookClear,
                MarketInput::Book {
                    side: lines_core::Side::Bid,
                    price: px(5999.75),
                    qty: Qty(50),
                },
                MarketInput::Book {
                    side: lines_core::Side::Ask,
                    price: px(6000.00),
                    qty: Qty(50),
                },
            ],
        );
        let es_product = catalog::get(catalog::ES).unwrap();
        let a = AccountId::new("paper");
        let steps = parse_script(
            "buy 2 mkt; ack; print 5998.00; wait 30; sell 1 @ 6002.00; ack; print 6002.25; wait 30; sell 1 mkt; ack; \
             wait 60; sell 1 mkt; ack; buy 1 mkt; ack",
            &es_product,
            es(),
            &a,
        )
        .unwrap();
        t.run_script(&steps, start, 25);
        let snap = t.journal.snapshot().unwrap();
        let trades = &snap.trades;
        assert_eq!(trades.len(), 2, "{trades:#?}");
        let first = &trades[0];
        // Bought 2 at 6000.00 (the ask); sold 1 at 6002.00 (resting, filled by the 6002.25
        // print) and 1 at 5999.75 (the bid): +2.00 − 0.25 points = $87.50; $1 fees per side.
        assert_eq!(first.realized, lines_trading::Money::cents(8750));
        assert_eq!(first.fees, lines_trading::Money::cents(400));
        assert_eq!((first.max_qty, first.fills.len()), (2, 3));
        // Worst: 2 × −2.00 at the 5998.00 print; best: 2 × +2.25 at 6002.25.
        assert_eq!(first.mae, lines_trading::Money::cents(-20_000));
        assert_eq!(first.mfe, lines_trading::Money::cents(22_500));
        assert!(first.fills.iter().all(|f| f.source == Some(Source::Test)));
        // Short 1 at the bid, back at the ask: −0.25 points.
        assert_eq!(trades[1].side, lines_trading::Side::Sell);
        assert_eq!(trades[1].realized, lines_trading::Money::cents(-1250));
        assert_eq!(snap.open, 0);
    }

    /// Review round 2: the core reports positions on every market batch while one is open.
    /// Those must not republish the journal (the pane would re-format every row per batch);
    /// only opening, closing or editing a trade does.
    #[test]
    fn market_batches_with_an_open_position_dont_republish_the_journal() {
        let t = trading(vec![AccountConfig::paper("paper", 50_000)]);
        let start = UtcMillis::parse_iso("2026-11-05T15:00:00Z").unwrap();
        t.handle().advance_to(start);
        let book = |bid: f64, ask: f64| {
            vec![
                MarketInput::BookClear,
                MarketInput::Book {
                    side: lines_core::Side::Bid,
                    price: px(bid),
                    qty: Qty(50),
                },
                MarketInput::Book {
                    side: lines_core::Side::Ask,
                    price: px(ask),
                    qty: Qty(50),
                },
            ]
        };
        t.handle().market(es(), book(5999.75, 6000.00));
        let es_product = catalog::get(catalog::ES).unwrap();
        let a = AccountId::new("paper");
        let open = parse_script("buy 1 mkt; ack", &es_product, es(), &a).unwrap();
        t.run_script(&open, start, 25);
        let opened = t.journal.revision();
        assert_eq!(t.journal.snapshot().unwrap().open, 1);
        for i in 0..20 {
            let p = 6000.00 + f64::from(i) * 0.25;
            let mut batch = book(p - 0.25, p);
            batch.push(MarketInput::Trade {
                time: UtcMillis(start.0 + 100 + i64::from(i)),
                price: px(p),
                qty: Qty(1),
            });
            t.handle().market(es(), batch);
        }
        t.settle();
        assert_eq!(
            t.journal.revision(),
            opened,
            "market batches (and the core's position reports) don't publish"
        );
        let close = parse_script("sell 1 mkt; ack", &es_product, es(), &a).unwrap();
        t.run_script(&close, UtcMillis(start.0 + 1_000), 25);
        assert!(t.journal.revision() > opened, "a close publishes");
        let snap = t.journal.snapshot().unwrap();
        assert_eq!((snap.trades.len(), snap.open), (1, 0));
        assert!(
            snap.trades[0].mfe > lines_trading::Money::ZERO,
            "MAE/MFE still follow"
        );
    }

    #[test]
    fn scripts_parse_into_commands() {
        let es_product = catalog::get(catalog::ES).unwrap();
        let a = AccountId::new("paper");
        let steps = parse_script(
            "buy 2 mkt; ack; buy 1 @ 5999.50 bracket 8/16; sell 1 stop 5990.00; account paper-eval; flatten",
            &es_product,
            es(),
            &a,
        )
        .unwrap();
        assert_eq!(steps.len(), 6);
        assert_eq!(steps[1], Step::Ack);
        let Step::Command(TradeCommand::PlaceBracket(b)) = &steps[2] else {
            panic!("{:?}", steps[2])
        };
        assert_eq!(b.take_profit, Some(px(5999.50 + 4.0)));
        assert_eq!(b.stop_loss, Some(px(5999.50 - 2.0)));
        assert!(matches!(
            &steps[5],
            Step::Command(TradeCommand::Flatten { account, .. }) if account.0 == "paper-eval"
        ));
        for bad in [
            "buy 1 @ 5999.10",
            "buy x mkt",
            "hold 1",
            "buy 1 mkt bracket 8/16",
        ] {
            assert!(parse_script(bad, &es_product, es(), &a).is_err(), "{bad}");
        }
    }
}
