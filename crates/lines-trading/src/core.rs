//! The trading core (decisions 0012, 0018): one deterministic state machine above every broker.
//!
//! Inputs: commands ([`TradingCore::submit`]), market data ([`TradingCore::market`]) and time
//! ([`TradingCore::tick`]). Output: the event stream ([`TradingCore::drain`]). It never does
//! I/O and never reads a clock: the caller passes `now` (the threaded runtime uses the wall
//! clock, replay tests use recorded time), so a scripted session always produces the same
//! events.
//!
//! A command passes, in order: validation (account, contract, quantity, tick grid, broker
//! capabilities) → prop rules (blocked after a breach; contract cap) → pre-trade risk → the
//! broker. Flatten, Cancel All and cancels skip rules and risk: they always work. The news
//! blackout (roadmap 8b) reads the economic calendar given with
//! [`TradingCore::set_econ_schedule`] (the bundled one by default) on the core's clock.
//!
//! Orders, positions and balances are the **broker's**; the core keeps a cache updated from
//! broker events and replaced wholesale by [`TradingCore::rebuild`] after a (re)connect.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::econ::EconSchedule;
use lines_core::latency::{self as lat, LatencyWindow};

use lines_core::sessions::SessionKind;
use lines_core::{CivilDate, ContractKey, Price, ProductId, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::broker::{Broker, BrokerEvent, BrokerRequest, PaneContext, RequestContext};
use crate::command::{Command, Envelope, OrderSpec, Reject, RejectCode, Source};
use crate::event::{AccountView, Event, FillEvent, PositionView};
use crate::guardrails::{Entry, GuardChange, GuardState, GuardrailTracker};
use crate::market::MarketInput;
use crate::news::{self, NewsAction, NewsBlackout};
use crate::order::{Order, OrderState, Role};
use crate::position::Position;
use crate::risk::{self, RiskContext, RiskLimits};
use crate::rules::{PendingRules, RulePreset, RuleTracker, TradingState};
use crate::strategy::{Instance, StrategyId};
use crate::types::{AccountId, CommandId, ContractSpec, Money, OrderId, OrderKind, Side};

/// The order id a fill made outside Lines carries ([`BrokerEvent::ExternalFill`]): no order.
pub const EXTERNAL_ORDER: OrderId = OrderId(0);
mod position_bracket;
mod strategy_gate;

/// An account as configured (saved with the trading state).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: AccountId,
    pub name: String,
    /// Whole currency units.
    pub starting_balance: i64,
    #[serde(default)]
    pub limits: RiskLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<RulePreset>,
    /// The account group (for the future copier, roadmap 14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Whose trading day the account's day (daily loss, end-of-day drawdown) follows.
    #[serde(default = "default_day_product")]
    pub day_product: u32,
}

fn default_day_product() -> u32 {
    lines_feed::catalog::ES.0
}

impl AccountConfig {
    pub fn paper(id: &str, starting_balance: i64) -> AccountConfig {
        AccountConfig {
            id: AccountId::new(id),
            name: id.to_owned(),
            starting_balance,
            limits: RiskLimits::default(),
            rules: None,
            group: None,
            day_product: default_day_product(),
        }
    }
}

struct AccountCache {
    cfg: AccountConfig,
    balance: Money,
    positions: BTreeMap<ContractKey, Position>,
    tracker: RuleTracker,
    /// The trader's guardrails and their progress (roadmap 8b).
    guard: GuardrailTracker,
    /// The guardrail state last reported (an event when it changes).
    guard_seen: GuardChange,
    /// Prop rules asked for that are looser than those in force: they take over with the
    /// next trade date (`Command::SetRules`).
    pending_rules: Option<PendingRules>,
    /// A venue account's saved trade date and opening balance, applied when the broker
    /// first lists it (its real balance known).
    restore_day: Option<(CivilDate, Money)>,
    /// When recent new orders and modifies were sent (rate limits, decision 0006).
    submits: VecDeque<UtcMillis>,
    modifies: VecDeque<UtcMillis>,
}

/// Count `times` within the last second of `now` (dropping older ones).
fn in_last_second(times: &mut VecDeque<UtcMillis>, now: UtcMillis) -> u32 {
    while times.front().is_some_and(|t| now.0 - t.0 >= 1000) {
        times.pop_front();
    }
    times.len() as u32
}

#[derive(Copy, Clone, Debug, Default)]
struct Mark {
    last: Option<Price>,
    /// When `last` was seen (an underlying's close, for settling its options).
    last_at: Option<UtcMillis>,
    bid: Option<Price>,
    ask: Option<Price>,
    /// When an option's quote was stamped.
    quoted_at: Option<UtcMillis>,
}

impl Mark {
    fn price(&self) -> Option<Price> {
        self.last.or(match (self.bid, self.ask) {
            (Some(b), Some(a)) => Some(Price((b.0 + a.0).div_euclid(2))),
            (b, a) => b.or(a),
        })
    }
}

/// When an option stops trading and settles: its expiry's regular close, or the end of that
/// day for an expiry that isn't a trade date (`None`: not an option).
fn option_close(contract: ContractKey) -> Option<UtcMillis> {
    let key = contract.option_key()?;
    let close = lines_feed::catalog::sessions(contract.product)
        .and_then(|c| c.session(key.expiry, lines_core::SessionKind::Rth))
        .map(|s| s.close);
    Some(close.unwrap_or_else(|| key.expiry.add_days(1).start_utc()))
}

/// Order latency for the latency HUD (roadmap 6b) over the last 60 s of the core's clock:
/// click → sent, and the broker's ack round trip (sent → acked, on the broker's clock; the
/// paper broker simulates it). Recorded only while the HUD is shown. Both sides only
/// `try_lock` (decision 0012: the core never waits on a lock the UI holds); a sample that
/// meets the UI reading the windows is dropped.
#[derive(Debug, Default)]
pub struct OrderLatency {
    pub click_sent: LatencyWindow,
    pub ack_rtt: LatencyWindow,
    /// The core's newest time (ms): the windows' "now" for display.
    pub at_ms: Option<i64>,
}

impl OrderLatency {
    fn saw(&mut self, now: UtcMillis) {
        self.at_ms = Some(self.at_ms.map_or(now.0, |a| a.max(now.0)));
    }
}

/// Click → order-sent latency (decision 0012), in microseconds.
#[derive(Debug, Default)]
pub struct Latency {
    samples: VecDeque<u64>,
}

impl Latency {
    const KEEP: usize = 4096;

    fn record(&mut self, us: u64) {
        if self.samples.len() == Self::KEEP {
            self.samples.pop_front();
        }
        self.samples.push_back(us);
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }

    /// The `p`-th percentile (0–100) of recent samples, µs.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        let mut v: Vec<u64> = self.samples.iter().copied().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let i = ((v.len() - 1) as f64 * p / 100.0).round() as usize;
        Some(v[i])
    }
}

pub struct TradingCore {
    broker: Box<dyn Broker>,
    accounts: BTreeMap<AccountId, AccountCache>,
    orders: BTreeMap<OrderId, Order>,
    marks: HashMap<ContractKey, Mark>,
    specs: HashMap<ContractKey, ContractSpec>,
    next_order: u64,
    /// Set by `PlaceExit` for the order it is about to create: reduce-only at the broker.
    next_reduce_only: bool,
    next_command: u64,
    /// Ids for commands the core issues itself (a pause's cancels, Stop with flatten, a rule's
    /// liquidation), from [`INTERNAL_COMMAND_IDS`] up: never an id a `TradingHandle` gave out.
    next_internal: u64,
    latency: Latency,
    hud: Arc<Mutex<OrderLatency>>,
    events: Vec<Event>,
    broker_events: Vec<BrokerEvent>,
    /// Terminal orders kept for display, oldest dropped first.
    finished: VecDeque<OrderId>,
    /// Orders with a cancel on its way (not sent twice).
    cancelling: HashSet<OrderId>,
    /// Expired option positions already sent to settle (one try per run).
    settled: HashSet<(AccountId, ContractKey)>,
    overfill: OverfillPolicy,
    /// Execution ids already applied, and their order (to forget the oldest).
    fill_ids: HashSet<String>,
    fill_order: VecDeque<String>,
    /// Orders first seen in a broker snapshot, not placed through this core (tracked only for
    /// brokers that report external fills; see `BrokerCaps::external_fills`).
    adopted: HashSet<OrderId>,
    /// External fills for accounts not added yet (a broker can report executions before its
    /// accounts join): applied when they do. Oldest dropped past `KEEP_HELD`.
    held: VecDeque<BrokerEvent>,
    /// Strategy instances (decision 0025), and which instance placed each order.
    strategies: BTreeMap<StrategyId, Instance>,
    strategy_orders: HashMap<OrderId, StrategyId>,
    /// Accounts and contracts whose manual trades have one bracket for the position.
    position_brackets: BTreeMap<(AccountId, ContractKey), position_bracket::PositionBracket>,
    /// The economic calendar for news blackouts (`None`: none loaded, never assumed clear).
    econ: Option<Arc<EconSchedule>>,
    /// Positions a news blackout already flattened: (account, contract, release time). Each
    /// window flattens a contract once; entries stay refused for the rest of it.
    news_flattened: HashSet<(AccountId, ContractKey, UtcMillis)>,
    /// The latest time the core was given (for `next_wake`, which takes none).
    seen_now: UtcMillis,
    /// Guardrails for accounts joining later (`set_join_guardrails`).
    join_guards: JoinGuards,
}

/// Saved guardrail progress (by id) and settings (by name or id) for accounts that join.
#[derive(Default)]
struct JoinGuards {
    defaults: BTreeMap<String, crate::guardrails::Guardrails>,
    saved: BTreeMap<String, crate::persist::VenueGuard>,
}

const KEEP_FINISHED: usize = 2000;
/// The first id of commands the core issues itself (see [`TradingCore::submit_internal`]).
pub const INTERNAL_COMMAND_IDS: u64 = 1 << 62;
/// Execution ids remembered for de-duplication (oldest forgotten first).
const KEEP_FILL_IDS: usize = 100_000;
/// External fills held for accounts not added yet.
const KEEP_HELD: usize = 10_000;

/// What to do with a fill for more than an order had left (it should never happen; when it
/// does it is reported, never clamped).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverfillPolicy {
    /// Apply the whole fill (the broker's position is the truth) and report it.
    #[default]
    Record,
    /// Don't apply it; report it (reconciliation with the broker settles the position).
    Reject,
}

impl TradingCore {
    pub fn new(broker: Box<dyn Broker>) -> TradingCore {
        TradingCore {
            broker,
            accounts: BTreeMap::new(),
            orders: BTreeMap::new(),
            marks: HashMap::new(),
            specs: HashMap::new(),
            next_order: 1,
            next_reduce_only: false,
            next_command: 1,
            next_internal: INTERNAL_COMMAND_IDS,
            latency: Latency::default(),
            hud: Arc::default(),
            events: Vec::new(),
            broker_events: Vec::new(),
            finished: VecDeque::new(),
            cancelling: HashSet::new(),
            settled: HashSet::new(),
            overfill: OverfillPolicy::default(),
            fill_ids: HashSet::new(),
            fill_order: VecDeque::new(),
            adopted: HashSet::new(),
            held: VecDeque::new(),
            strategies: BTreeMap::new(),
            strategy_orders: HashMap::new(),
            position_brackets: BTreeMap::new(),
            econ: Some(lines_feed::econ::bundled()),
            news_flattened: HashSet::new(),
            seen_now: UtcMillis(i64::MIN),
            join_guards: JoinGuards::default(),
        }
    }

    /// The economic calendar news blackouts read (`None`: none, and a blackout rule that
    /// refuses entries refuses them all, since nothing can be confirmed clear).
    pub fn set_econ_schedule(&mut self, schedule: Option<Arc<EconSchedule>>) {
        self.econ = schedule;
    }

    pub fn econ_schedule(&self) -> Option<&Arc<EconSchedule>> {
        self.econ.as_ref()
    }

    /// The news blackout in force for an account: its guardrail and its prop preset's, the
    /// tighter of each part.
    pub fn news_rule(&self, account: &AccountId) -> Option<NewsBlackout> {
        let a = self.accounts.get(account)?;
        NewsBlackout::tighter(
            a.guard.active.news,
            a.cfg.rules.as_ref().and_then(|r| r.news),
        )
    }

    /// Guardrails for accounts that join later through [`TradingCore::add_accounts`] (a
    /// broker venue's, discovered after it connects): the progress saved for each (by id,
    /// lockouts included) is restored when it joins, then the trader's settings (by name,
    /// else id) are asked for as `SetGuardrails` would (tighter now, looser with the next
    /// trade date; nothing clears a lockout).
    pub fn set_join_guardrails(
        &mut self,
        defaults: BTreeMap<String, crate::guardrails::Guardrails>,
        saved: BTreeMap<String, crate::persist::VenueGuard>,
    ) {
        self.join_guards = JoinGuards { defaults, saved };
    }

    /// Every account's guardrail progress, by id (a venue's saved guardrails file), with the
    /// saved progress of accounts that haven't joined yet (never dropped by a save before the
    /// broker lists them).
    pub fn guard_file(&self) -> crate::persist::GuardFile {
        let mut accounts = self.join_guards.saved.clone();
        accounts.extend(
            self.accounts
                .iter()
                .filter(|(_, a)| !a.guard.is_default())
                .map(|(id, a)| {
                    (
                        id.0.clone(),
                        crate::persist::VenueGuard {
                            guardrails: a.guard.clone(),
                            day: a.tracker.day,
                            day_open_balance: a.tracker.day_open_balance,
                        },
                    )
                }),
        );
        crate::persist::GuardFile {
            version: crate::persist::GUARDS_VERSION,
            accounts,
        }
    }

    /// A joining account's guardrails: saved progress, then settings; a restored lockout is
    /// reported (the UI learns of it like any other).
    fn join_guardrails(&mut self, account: &AccountId) {
        let Some(a) = self.accounts.get_mut(account) else {
            return;
        };
        if let Some(saved) = self.join_guards.saved.remove(account.0.as_str()) {
            a.guard = saved.guardrails;
            // The same trade date again: today's loss still counts from its opening balance,
            // once the broker lists the account with its real balance (`rebuild`); before
            // that its balance is unknown and the day's loss would read wrong.
            a.restore_day = saved.day.map(|d| (d, saved.day_open_balance));
        }
        let wanted = self
            .join_guards
            .defaults
            .get(&a.cfg.name)
            .or_else(|| self.join_guards.defaults.get(account.0.as_str()))
            .cloned();
        if let Some(g) = wanted {
            a.guard.request(g);
        }
        self.guard_report(account);
    }

    pub fn set_overfill_policy(&mut self, p: OverfillPolicy) {
        self.overfill = p;
    }

    /// The account's trading state (active unless a prop breach restricts it).
    pub fn trading_state(&self, account: &AccountId) -> TradingState {
        self.accounts
            .get(account)
            .map_or(TradingState::Active, |a| {
                a.cfg
                    .rules
                    .as_ref()
                    .map_or(TradingState::Active, |r| a.tracker.trading_state(r))
            })
    }

    /// Remember an execution id; `false` if it was seen already.
    fn first_fill(&mut self, trade_id: &str) -> bool {
        if trade_id.is_empty() {
            return true; // a broker without execution ids: nothing to de-duplicate on
        }
        if !self.fill_ids.insert(trade_id.to_owned()) {
            return false;
        }
        self.fill_order.push_back(trade_id.to_owned());
        if self.fill_order.len() > KEEP_FILL_IDS
            && let Some(old) = self.fill_order.pop_front()
        {
            self.fill_ids.remove(&old);
        }
        true
    }

    /// Add accounts the core doesn't know yet (a broker that discovers its accounts after
    /// connecting), then rebuild from the broker so they show its state.
    pub fn add_accounts(&mut self, now: UtcMillis, accounts: Vec<AccountConfig>) {
        let mut added = false;
        for cfg in accounts {
            if !self.accounts.contains_key(&cfg.id) {
                let id = cfg.id.clone();
                self.add_account(cfg, None);
                self.join_guardrails(&id);
                added = true;
            }
        }
        if added {
            // Fills reported before their account joined, then the snapshot.
            let held: Vec<BrokerEvent> = self.held.drain(..).collect();
            for e in held {
                self.apply(e);
            }
            self.rebuild(now);
        }
    }

    pub fn add_account(&mut self, cfg: AccountConfig, tracker: Option<RuleTracker>) {
        let start = Money::whole(cfg.starting_balance);
        self.accounts.insert(
            cfg.id.clone(),
            AccountCache {
                tracker: tracker.unwrap_or_else(|| RuleTracker::new(start)),
                guard: GuardrailTracker::default(),
                guard_seen: GuardrailTracker::default().change(),
                pending_rules: None,
                restore_day: None,
                balance: start,
                positions: BTreeMap::new(),
                cfg,
                submits: VecDeque::new(),
                modifies: VecDeque::new(),
            },
        );
    }

    /// Prop rules asked for that wait for the next trade date (they are looser).
    pub fn pending_rules(&self, account: &AccountId) -> Option<&PendingRules> {
        self.accounts.get(account)?.pending_rules.as_ref()
    }

    /// Restore an account's saved prop rules and the looser ones still waiting.
    pub fn restore_rules(
        &mut self,
        account: &AccountId,
        rules: Option<RulePreset>,
        pending: Option<PendingRules>,
    ) {
        if let Some(a) = self.accounts.get_mut(account) {
            a.cfg.rules = rules;
            a.pending_rules = pending;
        }
    }

    /// Restore an account's saved guardrail progress (a restart keeps a lockout).
    pub fn restore_guardrails(&mut self, account: &AccountId, guard: GuardrailTracker) {
        if let Some(a) = self.accounts.get_mut(account) {
            a.guard_seen = guard.change();
            a.guard = guard;
        }
    }

    /// An account's guardrails and their progress (saved with the account).
    pub fn guardrails(&self, account: &AccountId) -> Option<&GuardrailTracker> {
        self.accounts.get(account).map(|a| &a.guard)
    }

    /// The account's guardrail state (active, cooldown, locked).
    pub fn guard_state(&self, account: &AccountId) -> GuardState {
        self.accounts
            .get(account)
            .map_or(GuardState::Active, |a| a.guard.state())
    }

    pub fn accounts(&self) -> impl Iterator<Item = (&AccountConfig, &RuleTracker)> {
        self.accounts.values().map(|a| (&a.cfg, &a.tracker))
    }

    /// Account groups (name → members) from the accounts' configs.
    pub fn groups(&self) -> BTreeMap<String, Vec<AccountId>> {
        let mut g: BTreeMap<String, Vec<AccountId>> = BTreeMap::new();
        for a in self.accounts.values() {
            if let Some(name) = &a.cfg.group {
                g.entry(name.clone()).or_default().push(a.cfg.id.clone());
            }
        }
        g
    }

    pub fn broker(&self) -> &dyn Broker {
        self.broker.as_ref()
    }

    pub fn broker_mut(&mut self) -> &mut dyn Broker {
        self.broker.as_mut()
    }

    /// Swap in a new broker session (a reconnect); follow with [`TradingCore::rebuild`].
    pub fn set_broker(&mut self, broker: Box<dyn Broker>) {
        self.broker = broker;
        self.broker_events.clear();
    }

    pub fn latency(&self) -> &Latency {
        &self.latency
    }

    /// The latency HUD's order windows (shared; the UI reads them with `try_lock`).
    pub fn order_latency(&self) -> Arc<Mutex<OrderLatency>> {
        self.hud.clone()
    }

    /// Move the HUD windows' "now" to the core's clock (so old samples leave on screen
    /// while no orders are sent). Nothing while the HUD is hidden.
    pub fn note_time(&self, now: UtcMillis) {
        if lat::enabled()
            && let Ok(mut h) = self.hud.try_lock()
        {
            h.saw(now);
        }
    }

    pub fn next_order_id(&self) -> u64 {
        self.next_order
    }

    /// Continue numbering after a restart (ids are the broker's client order ids).
    pub fn set_next_order_id(&mut self, n: u64) {
        self.next_order = self.next_order.max(n);
    }

    /// Orders sent without a known outcome (saved with the trading file: after a restart
    /// they come back [`OrderState::Unresolved`] unless the broker lists them).
    pub fn in_flight(&self) -> impl Iterator<Item = &Order> {
        self.orders
            .values()
            .filter(|o| matches!(o.state, OrderState::Submitted | OrderState::Unresolved))
    }

    /// Restore orders that were in flight when the app stopped, as unresolved. The next
    /// [`TradingCore::rebuild`] replaces any the broker knows.
    pub fn restore_in_flight(&mut self, orders: Vec<Order>) {
        for mut o in orders {
            o.state = OrderState::Unresolved;
            self.next_order = self.next_order.max(o.id.0 + 1);
            self.events.push(Event::Order(o.clone()));
            self.orders.insert(o.id, o);
        }
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    pub fn open_orders(&self, account: &AccountId) -> impl Iterator<Item = &Order> {
        self.orders
            .values()
            .filter(move |o| &o.account == account && o.is_open())
    }

    pub fn position(&self, account: &AccountId, contract: ContractKey) -> i64 {
        self.accounts
            .get(account)
            .and_then(|a| a.positions.get(&contract))
            .map_or(0, Position::qty)
    }

    pub fn balance(&self, account: &AccountId) -> Option<Money> {
        self.accounts.get(account).map(|a| a.balance)
    }

    /// Events since the last drain, in order.
    pub fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn spec(&mut self, contract: ContractKey) -> Option<ContractSpec> {
        if let Some(s) = self.specs.get(&contract) {
            return Some(s.clone());
        }
        let p = lines_feed::catalog::get(contract.product)?;
        let s = ContractSpec::new(p, contract, Money::ZERO)?;
        self.specs.insert(contract, s.clone());
        Some(s)
    }

    // ---- commands -------------------------------------------------------------------

    /// No time yet: a replay's start (market or manual clock) before its first data, which
    /// is also what the service's market clock reads then. Nothing at or before it rolls the
    /// trade date (startup's rebuilds and settings), so the first data's date is never taken
    /// for a new one, which would bring in looser rules waiting for the next session early.
    pub const BEFORE_DATA: UtcMillis = UtcMillis(0);

    /// The single entry point (decision 0018). Returns the command's id; its result is an
    /// [`Event::Command`] in the stream.
    pub fn submit(
        &mut self,
        now: UtcMillis,
        source: Source,
        issued: Instant,
        command: Command,
    ) -> CommandId {
        self.submit_from_pane(now, source, issued, None, command)
    }

    /// [`TradingCore::submit`] for a command issued from a pane: the broker gets the pane's
    /// contract, how it was chosen and the front month with every request.
    pub fn submit_from_pane(
        &mut self,
        now: UtcMillis,
        source: Source,
        issued: Instant,
        pane: Option<PaneContext>,
        command: Command,
    ) -> CommandId {
        let id = CommandId(self.next_command);
        self.next_command += 1;
        self.handle(
            now,
            Envelope {
                id,
                source,
                issued,
                command,
                pane,
            },
        );
        id
    }

    /// A command the core issues itself, with an id from the internal range (so it can't take
    /// an id a `TradingHandle` already gave a caller whose command is still on its way).
    pub(crate) fn submit_internal(
        &mut self,
        now: UtcMillis,
        source: Source,
        command: Command,
    ) -> CommandId {
        let id = CommandId(self.next_internal);
        self.next_internal += 1;
        self.handle(
            now,
            Envelope {
                id,
                source,
                issued: Instant::now(),
                command,
                pane: None,
            },
        );
        id
    }

    /// Handle a command whose id was assigned elsewhere (the threaded runtime assigns ids at
    /// the caller so it can return them without waiting).
    pub fn handle(&mut self, now: UtcMillis, env: Envelope) {
        self.seen_now = self.seen_now.max(now);
        if env.id.0 < INTERNAL_COMMAND_IDS {
            self.next_command = self.next_command.max(env.id.0 + 1);
        }
        // Position brackets: a person's bracket order loses its exits, a second exit is refused.
        let (env, result) = match self.bracket_rewrite(&env) {
            Err(r) => (env, Err(r)),
            Ok(Some(command)) => {
                let env = Envelope { command, ..env };
                let result = self.run(now, &env);
                (env, result)
            }
            Ok(None) => {
                let result = self.run(now, &env);
                (env, result)
            }
        };
        self.bracket_note_cancel(&env, &result);
        if let Source::Strategy(s) = env.source {
            self.strategy_counted(s, now, result.is_ok());
            if let Ok(ids) = &result {
                self.strategy_owns(s, &env.command, ids);
            }
        }
        self.events.push(Event::Command {
            id: env.id,
            source: env.source,
            at: now,
            result,
        });
        // Requests may complete at once (zero latency): apply what the broker has.
        self.poll(now);
        self.strategy_watch(now);
        self.position_brackets_sync(now);
    }

    fn run(&mut self, now: UtcMillis, env: &Envelope) -> Result<Vec<OrderId>, Reject> {
        if let Some(r) = self.strategy_control(now, env) {
            return r;
        }
        let account = env
            .command
            .account()
            .expect("every other command names its account")
            .clone();
        if !self.accounts.contains_key(&account) {
            return Err(Reject::new(
                RejectCode::UnknownAccount,
                format!("no account {account}"),
            ));
        }
        self.roll_day(now, &account);
        // Strategies (decision 0025): their commands pass the strategy gate first; a person's
        // order action pauses the strategies trading the same account and contract.
        if let Source::Strategy(s) = env.source {
            self.strategy_gate(now, s, &env.command)?;
        } else if crate::broker::Origin::of(env.source) == crate::broker::Origin::Manual
            && env.command.is_order_action()
        {
            self.strategy_manual(now, &account, &env.command);
        }
        match env.command.clone() {
            Command::PlaceOrder(spec) => {
                self.check_new(now, &spec)?;
                let o = self.new_order(now, &spec, Role::Single, None);
                let id = o.id;
                self.send(env, now, BrokerRequest::Place(o));
                Ok(vec![id])
            }
            Command::PlaceExit(spec) => {
                let pos = self.position(&spec.account, spec.contract);
                if pos == 0 || (pos > 0) == (spec.side == Side::Buy) {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "no open position for this exit to close",
                    ));
                }
                if spec.qty.0 <= 0 || spec.qty.0 > pos.abs() {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "an exit can't be larger than the position",
                    ));
                }
                self.check_exit(now, &spec)?;
                // A stop and a target for the same position: only one may fill (OCO), so the
                // broker budgets them as one and cancels the other when one closes the position.
                let stop = matches!(
                    spec.kind,
                    OrderKind::Stop { .. } | OrderKind::StopLimit { .. }
                );
                // A sibling still holding the pair: open and not being cancelled (× then a quick
                // re-add must pair the new exit, not leave it alone).
                let cancelling = &self.cancelling;
                let open = |id: Option<OrderId>, orders: &BTreeMap<OrderId, Order>| {
                    id.filter(|i| !cancelling.contains(i))
                        .and_then(|i| orders.get(&i))
                        .is_some_and(|o| o.is_open())
                };
                let sibling = self
                    .orders
                    .values()
                    .find(|o| {
                        o.is_open()
                            && !cancelling.contains(&o.id)
                            && o.reduce_only
                            && o.role != Role::Flatten
                            && !open(o.oco, &self.orders)
                            && o.account == spec.account
                            && o.contract == spec.contract
                            && o.side == spec.side
                            && matches!(
                                o.kind,
                                OrderKind::Stop { .. } | OrderKind::StopLimit { .. }
                            ) != stop
                    })
                    .map(|o| o.id);
                self.next_reduce_only = true;
                let mut o = self.new_order(now, &spec, Role::Single, None);
                if let Some(s) = sibling {
                    o.oco = Some(s);
                    if let Some(x) = self.orders.get_mut(&o.id) {
                        x.oco = Some(s);
                    }
                    // Both ways: the pair is one budget whichever side the broker looks from.
                    if let Some(x) = self.orders.get_mut(&s) {
                        x.oco = Some(o.id);
                        // The app's cache sees the pair from either side (breakeven reads it).
                        let x = x.clone();
                        self.events.push(Event::Order(x));
                    }
                    // The new exit was published before it was linked: publish it again, so the
                    // app never holds half a pair.
                    self.events.push(Event::Order(o.clone()));
                }
                let id = o.id;
                self.send(env, now, BrokerRequest::Place(o));
                Ok(vec![id])
            }
            Command::PlaceBracket(b) => {
                if b.entry.contract.option_key().is_some() {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "no brackets on options: single-leg limits only",
                    ));
                }
                if !self.broker.capabilities().server_brackets {
                    return Err(Reject::new(
                        RejectCode::Broker,
                        format!("{} doesn't hold brackets", self.broker.name()),
                    ));
                }
                self.check_new(now, &b.entry)?;
                if b.take_profit.is_none() && b.stop_loss.is_none() {
                    return Err(Reject::new(RejectCode::Invalid, "a bracket needs an exit"));
                }
                let spec = self.spec(b.entry.contract).expect("checked");
                for p in b.take_profit.iter().chain(b.stop_loss.iter()) {
                    if !spec.on_tick_grid(*p) {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            "exit price is off the tick grid",
                        ));
                    }
                }
                let entry = self.new_order(now, &b.entry, Role::Entry, None);
                let exit_side = b.entry.side.opposite();
                let mut exits = Vec::new();
                let mk = |kind| OrderSpec {
                    kind,
                    side: exit_side,
                    ..b.entry.clone()
                };
                if let Some(p) = b.take_profit {
                    exits.push(self.new_order(
                        now,
                        &mk(OrderKind::Limit { price: p }),
                        Role::TakeProfit,
                        Some(entry.id),
                    ));
                }
                if let Some(p) = b.stop_loss {
                    exits.push(self.new_order(
                        now,
                        &mk(OrderKind::Stop { trigger: p }),
                        Role::StopLoss,
                        Some(entry.id),
                    ));
                }
                if exits.len() == 2 {
                    let (a, c) = (exits[0].id, exits[1].id);
                    exits[0].oco = Some(c);
                    exits[1].oco = Some(a);
                    for x in &exits {
                        self.orders.insert(x.id, x.clone());
                    }
                }
                let ids: Vec<OrderId> = std::iter::once(entry.id)
                    .chain(exits.iter().map(|x| x.id))
                    .collect();
                for x in &exits {
                    self.events.push(Event::Order(x.clone()));
                }
                self.send(env, now, BrokerRequest::Bracket { entry, exits });
                Ok(ids)
            }
            Command::ModifyOrder {
                order, qty, price, ..
            } => {
                let o = self.open_order(&account, order)?;
                let a = self.accounts.get_mut(&account).expect("checked");
                if let Some(max) = a.cfg.limits.max_modifies_per_sec
                    && in_last_second(&mut a.modifies, now) >= max
                {
                    return Err(Reject::new(
                        RejectCode::Risk,
                        format!("more than {max} modifies in a second; slow down"),
                    ));
                }
                if !self.broker.capabilities().modify {
                    return Err(Reject::new(
                        RejectCode::Broker,
                        "this broker can't modify orders",
                    ));
                }
                if let Some(q) = qty
                    && q.0 <= o.filled.0
                {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "new quantity is not above the filled quantity",
                    ));
                }
                // A reduce-only order never grows: a larger exit could flip the position (a
                // stop sell 2 on a long 2 grown to 5 leaves a short 3). Place another exit.
                if let Some(q) = qty
                    && q.0 > o.qty.0
                    && o.reduce_only
                {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "reduce-only orders can't grow; place another exit",
                    ));
                }
                // More quantity is more exposure: the increase is checked as a new order of
                // its own (guardrails, prop state and the contract cap, risk, the submit rate),
                // and the whole order against the order-size limit. Less quantity, or a price
                // change alone, needs none of that.
                if let Some(q) = qty
                    && q.0 > o.qty.0
                {
                    self.check_new(
                        now,
                        &OrderSpec {
                            account: account.clone(),
                            contract: o.contract,
                            side: o.side,
                            qty: Qty(q.0 - o.qty.0),
                            kind: o.kind,
                        },
                    )?;
                    if let Some(max) = self.accounts[&account].cfg.limits.max_order_qty
                        && q.0 > max
                    {
                        return Err(Reject::new(
                            RejectCode::Risk,
                            format!("order quantity {} is over the maximum {max}", q.0),
                        ));
                    }
                }
                let spec = self.spec(o.contract).expect("known");
                if let Some(p) = price {
                    if !spec.on_tick_grid(p) {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            "price is off the tick grid",
                        ));
                    }
                    // A changed price is a new price: fat-finger band again (a stop's trigger
                    // too; a triggered stop-limit moves its limit, as the broker does).
                    let kind = match o.kind {
                        OrderKind::Limit { .. } => OrderKind::Limit { price: p },
                        OrderKind::Stop { .. } => OrderKind::Stop { trigger: p },
                        OrderKind::StopLimit { trigger, .. } if o.triggered => {
                            OrderKind::StopLimit { trigger, limit: p }
                        }
                        OrderKind::StopLimit { limit, .. } => {
                            OrderKind::StopLimit { trigger: p, limit }
                        }
                        k => k,
                    };
                    // A stop never moves across the market: it would trigger at once (a buy
                    // stop at or under the ask, a sell stop at or over the bid; the last trade
                    // without a book). Unknown market: the app's own check is all there is.
                    let stop = match o.kind {
                        OrderKind::Stop { .. } => true,
                        OrderKind::StopLimit { .. } => !o.triggered,
                        _ => false,
                    };
                    // A bracket exit not activated yet (its entry hasn't filled) isn't in the
                    // market: it can't trigger. An exit held after a fill (Working) is checked:
                    // the entry's next fill makes it live again.
                    let held = matches!(o.role, Role::TakeProfit | Role::StopLoss)
                        && matches!(o.state, OrderState::Submitted | OrderState::Acked)
                        && o.parent
                            .and_then(|e| self.orders.get(&e))
                            .is_some_and(Order::is_open);
                    if stop
                        && !held
                        && let Some(m) = self.marks.get(&o.contract)
                    {
                        let market = match o.side {
                            Side::Buy => m.ask.or(m.last),
                            Side::Sell => m.bid.or(m.last),
                        };
                        match (o.side, market) {
                            (Side::Buy, Some(x)) if p.0 <= x.0 => {
                                return Err(Reject::new(
                                    RejectCode::Invalid,
                                    "a buy stop must stay above the market",
                                ));
                            }
                            (Side::Sell, Some(x)) if p.0 >= x.0 => {
                                return Err(Reject::new(
                                    RejectCode::Invalid,
                                    "a sell stop must stay below the market",
                                ));
                            }
                            _ => {}
                        }
                    }
                    let limits = self.accounts[&account].cfg.limits.clone();
                    let ctx = RiskContext {
                        reference: self.marks.get(&o.contract).and_then(Mark::price),
                        ..Default::default()
                    };
                    let band_only = RiskLimits {
                        price_band_ticks: limits.price_band_ticks,
                        ..Default::default()
                    };
                    risk::check(&band_only, &spec, o.side, Qty(0), &kind, &ctx)?;
                    if o.contract.option_key().is_some() {
                        self.check_option_kind(now, &account, o.contract, &kind)?;
                    }
                }
                if let Some(a) = self.accounts.get_mut(&account) {
                    a.modifies.push_back(now);
                }
                self.send(
                    env,
                    now,
                    BrokerRequest::Modify {
                        id: order,
                        qty,
                        price,
                    },
                );
                Ok(vec![order])
            }
            Command::SetBracketMode { contract, .. } if contract.option_key().is_some() => {
                Err(Reject::new(
                    RejectCode::Invalid,
                    "no brackets on options: single-leg limits only",
                ))
            }
            Command::SetBracketMode {
                contract,
                mode,
                replace,
                ..
            } => self.set_bracket_mode(env, now, &account, contract, mode, replace),
            Command::CancelOrder { order, .. } => {
                self.open_order(&account, order)?;
                if self.cancelling.insert(order) {
                    self.send(env, now, BrokerRequest::Cancel { id: order });
                }
                Ok(vec![order])
            }
            Command::CancelAll { contract, .. } => {
                Ok(self.cancel_all(env, now, &account, contract))
            }
            Command::SetGuardrails { guardrails, .. } => {
                let unrealised = self.unrealised(&account);
                let day = (now > Self::BEFORE_DATA)
                    .then(|| self.trade_day(&account, now))
                    .flatten();
                let a = self.accounts.get_mut(&account).expect("checked");
                a.guard.request(guardrails);
                // A tighter daily loss applies to the next order: today's loss is checked
                // now (a guardrail lock never liquidates, nor blocks exits).
                if let Some((_, end)) = day {
                    let day_pnl = a.balance + unrealised - a.tracker.day_open_balance;
                    a.guard.on_day_pnl(day_pnl, end);
                }
                self.guard_report(&account);
                // The account's view says what is in force now and what waits.
                self.publish_view(&account);
                Ok(Vec::new())
            }
            Command::SetRules { rules, .. } => {
                // Prop rules stay out of options (decision 0028): their P&L would move the
                // drawdown and a breach could liquidate them.
                let options = self.accounts[&account]
                    .positions
                    .iter()
                    .any(|(k, p)| k.option_key().is_some() && !p.is_flat())
                    || self
                        .open_orders(&account)
                        .any(|o| o.contract.option_key().is_some());
                if rules.is_some() && options {
                    return Err(Reject::new(
                        RejectCode::PropRule,
                        "close the account's options before setting prop rules",
                    ));
                }
                let unrealised = self.unrealised(&account);
                let micros = self.micros_used(&account, None);
                let a = self.accounts.get_mut(&account).expect("checked");
                if let Some(w) = &rules
                    && w.account_size != a.cfg.starting_balance
                {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        format!(
                            "this account is ${}: choose rules for that size",
                            grouped(a.cfg.starting_balance)
                        ),
                    ));
                }
                // Tighter now, looser with the next trade date (a breach can't be talked
                // away): orders already sent keep the rules they were checked against.
                let in_force = crate::rules::tightest_rules(a.cfg.rules.as_ref(), rules.as_ref());
                // A tighter rule the account is already past would breach (and liquidate)
                // now, mid-order, or block its own exits: refused, with the reason.
                if let Some(p) = &in_force {
                    let equity = a.balance + unrealised;
                    let st = a.tracker.status(p, a.balance, equity, micros);
                    let was = a.cfg.rules.as_ref();
                    let changed = |f: fn(&RulePreset) -> Option<i64>| was.map(f) != Some(f(p));
                    // (Already breached: tightening is always allowed, nothing new happens.)
                    if changed(|r| r.daily_loss_limit)
                        && !a.tracker.daily_breached
                        && st.daily_loss_left.is_some_and(|l| l <= Money::ZERO)
                    {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            "today's loss is already past that daily loss limit: it would end the day now",
                        ));
                    }
                    // Amount, mode or lock: a higher lock moves a locked threshold up too.
                    if was.map(|r| &r.trailing_drawdown) != Some(&p.trailing_drawdown)
                        && !a.tracker.drawdown_breached
                        && st.to_drawdown.is_some_and(|l| l <= Money::ZERO)
                    {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            "the account is already past that drawdown: it would fail the account now",
                        ));
                    }
                    if changed(|r| r.max_micro_contracts.map(i64::from))
                        && p.max_micro_contracts.is_some_and(|m| micros > m)
                    {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            format!(
                                "{micros} micro-equivalent contracts are open or working: the cap can't be below that"
                            ),
                        ));
                    }
                }
                a.pending_rules = (in_force != rules).then_some(PendingRules { rules });
                a.cfg.rules = in_force;
                self.publish_view(&account);
                Ok(Vec::new())
            }
            Command::AddStrategy(_)
            | Command::RemoveStrategy { .. }
            | Command::ArmStrategy { .. }
            | Command::PauseStrategy { .. }
            | Command::StopStrategies { .. } => unreachable!("handled by strategy_control"),
            Command::Flatten { contract, .. } => {
                let simulated = self.broker.simulated();
                let positions: Vec<(ContractKey, i64)> = self.accounts[&account]
                    .positions
                    .iter()
                    .filter(|(k, _)| contract.is_none_or(|c| c == **k))
                    // Paper only (decision 0028, phase 4): no option order reaches a live broker.
                    .filter(|(k, _)| k.option_key().is_none() || simulated)
                    .map(|(k, p)| (*k, p.qty()))
                    .filter(|(_, q)| *q != 0)
                    .collect();
                // Closing a position cancels every order on it, whoever placed it: a trader's
                // stop left working after the position is gone would open a new one. With
                // nothing to close, a strategy's Flatten is its Cancel All (its own orders).
                let mut ids = if positions.is_empty() {
                    self.cancel_all(env, now, &account, contract)
                } else {
                    self.cancel_every(env, now, &account, contract)
                };
                for (k, q) in positions {
                    let spec = OrderSpec {
                        account: account.clone(),
                        contract: k,
                        side: if q > 0 { Side::Sell } else { Side::Buy },
                        qty: Qty(q.abs()),
                        kind: OrderKind::Market,
                    };
                    let o = self.new_order(now, &spec, Role::Flatten, None);
                    ids.push(o.id);
                    self.send(env, now, BrokerRequest::Place(o));
                }
                Ok(ids)
            }
        }
    }

    fn open_order(&self, account: &AccountId, id: OrderId) -> Result<Order, Reject> {
        match self.orders.get(&id) {
            Some(o) if &o.account == account && o.is_open() => Ok(o.clone()),
            Some(o) if &o.account == account => Err(Reject::new(
                RejectCode::UnknownOrder,
                format!("order {} is {}", id.0, o.state.label()),
            )),
            _ => Err(Reject::new(
                RejectCode::UnknownOrder,
                format!("no open order {}", id.0),
            )),
        }
    }

    fn cancel_all(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: Option<ContractKey>,
    ) -> Vec<OrderId> {
        // A strategy cancels only its own orders (decision 0025).
        let scope = self.cancel_scope(env.source);
        self.cancel_scoped(env, now, account, contract, scope)
    }

    /// Every open order on the account (and contract), whoever's: a Flatten closing positions.
    fn cancel_every(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: Option<ContractKey>,
    ) -> Vec<OrderId> {
        self.cancel_scoped(env, now, account, contract, None)
    }

    fn cancel_scoped(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: Option<ContractKey>,
        scope: Option<std::collections::BTreeSet<OrderId>>,
    ) -> Vec<OrderId> {
        let ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| &o.account == account && o.is_open())
            .filter(|o| contract.is_none_or(|c| c == o.contract))
            .filter(|o| scope.as_ref().is_none_or(|s| s.contains(&o.id)))
            .map(|o| o.id)
            .collect();
        for &id in &ids {
            if self.cancelling.insert(id) {
                self.send(env, now, BrokerRequest::Cancel { id });
            }
        }
        ids
    }

    /// An option order's own checks (decision 0028): paper only until a live broker is enabled
    /// (phase 4), the series not expired, and the guard.
    fn check_option_kind(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        kind: &OrderKind,
    ) -> Result<(), Reject> {
        if !self.broker.simulated() {
            return Err(Reject::new(
                RejectCode::Broker,
                format!(
                    "{} takes no option orders here: paper only",
                    self.broker.name()
                ),
            ));
        }
        if option_close(contract).is_some_and(|c| now >= c) {
            return Err(Reject::new(RejectCode::Invalid, "this option has expired"));
        }
        let cs = self
            .spec(contract)
            .ok_or_else(|| Reject::new(RejectCode::UnknownContract, "unknown contract"))?;
        let guard = self.accounts[account]
            .cfg
            .limits
            .option_guard
            .clone()
            .unwrap_or_default();
        let m = self.marks.get(&contract);
        let quote = risk::OptionQuoteView {
            bid: m.and_then(|m| m.bid),
            ask: m.and_then(|m| m.ask),
            at: m.and_then(|m| m.quoted_at),
        };
        risk::check_option(&guard, &cs, kind, quote, now)
    }

    /// Expired option positions settle at their intrinsic value against the underlying's close
    /// (paper only: a venue settles its own), and expired series' orders are cancelled.
    ///
    /// The tick runs before a batch's prices reach the marks, so at the first moment past the
    /// close the underlying's mark is still its last price before it. No price from the last
    /// day (the app wasn't running, or no pane follows the underlying): the position waits for
    /// the next price and settles on that, approximately (`docs/known-gaps.md`). Returns
    /// whether it asked the broker to settle anything.
    fn settle_options(&mut self, now: UtcMillis) -> bool {
        if !self.broker.simulated() {
            return false;
        }
        // One try per position: a settled one is flat, so it may settle again if it reopens.
        let accounts = &self.accounts;
        self.settled.retain(|(a, k)| {
            accounts
                .get(a)
                .and_then(|s| s.positions.get(k))
                .is_some_and(|p| !p.is_flat())
        });
        let expired = |k: &ContractKey| option_close(*k).is_some_and(|c| now >= c);
        let orders: Vec<(AccountId, OrderId)> = self
            .orders
            .values()
            .filter(|o| o.is_open() && expired(&o.contract))
            .filter(|o| !self.cancelling.contains(&o.id))
            .map(|o| (o.account.clone(), o.id))
            .collect();
        for (account, order) in orders {
            self.submit_internal(now, Source::Rule, Command::CancelOrder { account, order });
        }
        let positions: Vec<(AccountId, ContractKey, i64)> = self
            .accounts
            .iter()
            .flat_map(|(a, s)| {
                s.positions
                    .iter()
                    .filter(|(k, p)| expired(k) && !p.is_flat())
                    .map(|(k, p)| (a.clone(), *k, p.qty()))
            })
            .filter(|(a, k, _)| !self.settled.contains(&(a.clone(), *k)))
            .collect();
        let mut asked = false;
        for (account, contract, qty) in positions {
            let (Some(key), Some(close)) = (contract.option_key(), option_close(contract)) else {
                continue;
            };
            let underlying = self.marks.get(&ContractKey::equity(contract.product));
            let (Some(price), Some(at)) = (
                underlying.and_then(|m| m.last),
                underlying.and_then(|m| m.last_at),
            ) else {
                continue;
            };
            if at.0 < close.0 - 86_400_000 {
                continue;
            }
            if at > close {
                tracing::warn!("option settled on the first underlying price after its close");
            }
            let intrinsic = match key.right {
                lines_core::Right::Call => price.0 - key.strike.0,
                lines_core::Right::Put => key.strike.0 - price.0,
            }
            .max(0);
            let spec = OrderSpec {
                account: account.clone(),
                contract,
                side: if qty > 0 { Side::Sell } else { Side::Buy },
                qty: Qty(qty.abs()),
                kind: OrderKind::Limit {
                    price: Price(intrinsic),
                },
            };
            let o = self.new_order(now, &spec, Role::Flatten, None);
            self.settled.insert((account, contract));
            self.broker.settle(now, o, Price(intrinsic));
            asked = true;
        }
        asked
    }

    /// Validation, prop rules and risk for a new order.
    fn check_new(&mut self, now: UtcMillis, spec: &OrderSpec) -> Result<(), Reject> {
        let cs = self
            .spec(spec.contract)
            .ok_or_else(|| Reject::new(RejectCode::UnknownContract, "unknown contract"))?;
        if spec.contract.option_key().is_some() {
            // Prop rules don't apply to options (decision 0028): a prop account trades none, so
            // its drawdown and contract cap never see an option's P&L or size.
            if self.accounts[&spec.account].cfg.rules.is_some() {
                return Err(Reject::new(
                    RejectCode::PropRule,
                    "a prop-rule account can't open options",
                ));
            }
            self.check_option_kind(now, &spec.account, spec.contract, &spec.kind)?;
        }
        if spec.qty.0 <= 0 {
            return Err(Reject::new(
                RejectCode::Invalid,
                "quantity must be positive",
            ));
        }
        if let Some(p) = spec.kind.prices().find(|p| !cs.on_tick_grid(*p)) {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "price {} is not on the {} tick grid",
                    cs.product.price.format::<32>(p),
                    cs.product.symbol
                ),
            ));
        }
        let caps = self.broker.capabilities();
        if matches!(
            spec.kind,
            OrderKind::Stop { .. } | OrderKind::StopLimit { .. }
        ) && !caps.stop_orders
        {
            return Err(Reject::new(
                RejectCode::Broker,
                "this broker has no stop orders",
            ));
        }
        if matches!(spec.kind, OrderKind::Market) && !caps.market_orders {
            return Err(Reject::new(
                RejectCode::Broker,
                "this broker has no market orders",
            ));
        }
        // The trader's guardrails (exits always pass), then prop rules.
        self.check_guardrails(now, spec)?;
        let acct = &self.accounts[&spec.account];
        // Prop rules: a breach restricts the account (reducing only, or halted); the contract
        // cap counts micro-equivalents.
        if let Some(preset) = &acct.cfg.rules {
            let why = acct.tracker.blocked().map_or_else(String::new, |b| {
                format!("{b:?} limit breached ({})", preset.name)
            });
            match acct.tracker.trading_state(preset) {
                TradingState::Active => {}
                TradingState::Halted => {
                    return Err(Reject::new(
                        RejectCode::PropRule,
                        format!("trading halted: {why}; only Flatten and Cancel All"),
                    ));
                }
                TradingState::ReducingOnly => {
                    let position = acct.positions.get(&spec.contract).map_or(0, Position::qty);
                    let (open_buy, open_sell) = self.open_qty(&spec.account, spec.contract);
                    // What may still be closed: the position less what open orders already close.
                    let closable = match spec.side {
                        Side::Sell if position > 0 => position - open_sell,
                        Side::Buy if position < 0 => -position - open_buy,
                        _ => 0,
                    };
                    if spec.qty.0 > closable {
                        return Err(Reject::new(
                            RejectCode::PropRule,
                            format!(
                                "reducing only: {why}; an order may only close the open position ({closable} left to close)"
                            ),
                        ));
                    }
                    // A reducing order never adds exposure: the cap can't be crossed.
                    return self.check_risk(now, spec, &cs);
                }
            }
            if let Some(max) = preset.max_micro_contracts {
                let after =
                    self.micros_used(&spec.account, Some((spec.contract, spec.side, spec.qty)));
                if after > max {
                    return Err(Reject::new(
                        RejectCode::PropRule,
                        format!(
                            "would hold {after} micro-equivalent contracts, over the {max} allowed"
                        ),
                    ));
                }
            }
        }
        self.check_risk(now, spec, &cs)
    }

    /// The checks for an exit added to the open position (`PlaceExit`, already on the closing
    /// side and no larger than the position): the contract, tick grid and broker support, and
    /// risk; never the entry checks (guardrails, reducing only, the contract cap), which an exit
    /// passes by definition — the stop/target pair can only ever close the position once, so
    /// counting the other exit as "already closing" would refuse protection exactly when it
    /// matters. Refused when the broker can't hold OCO orders (the pair would outlive the
    /// position there) and when prop rules halted the account.
    fn check_exit(&mut self, now: UtcMillis, spec: &OrderSpec) -> Result<(), Reject> {
        let cs = self
            .spec(spec.contract)
            .ok_or_else(|| Reject::new(RejectCode::UnknownContract, "unknown contract"))?;
        if spec.contract.option_key().is_some() {
            self.check_option_kind(now, &spec.account, spec.contract, &spec.kind)?;
        }
        if let Some(p) = spec.kind.prices().find(|p| !cs.on_tick_grid(*p)) {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "price {} is not on the {} tick grid",
                    cs.product.price.format::<32>(p),
                    cs.product.symbol
                ),
            ));
        }
        let caps = self.broker.capabilities();
        if matches!(spec.kind, OrderKind::Market) && !caps.market_orders {
            return Err(Reject::new(
                RejectCode::Broker,
                "this broker has no market orders",
            ));
        }
        if !caps.oco {
            return Err(Reject::new(
                RejectCode::Broker,
                format!(
                    "{} can't hold a stop and a target as one-cancels-other",
                    self.broker.name()
                ),
            ));
        }
        if matches!(
            spec.kind,
            OrderKind::Stop { .. } | OrderKind::StopLimit { .. }
        ) && !caps.stop_orders
        {
            return Err(Reject::new(
                RejectCode::Broker,
                "this broker has no stop orders",
            ));
        }
        let acct = &self.accounts[&spec.account];
        if let Some(preset) = &acct.cfg.rules
            && acct.tracker.trading_state(preset) == TradingState::Halted
        {
            return Err(Reject::new(
                RejectCode::PropRule,
                format!(
                    "trading halted ({}); only Flatten and Cancel All",
                    preset.name
                ),
            ));
        }
        self.check_risk(now, spec, &cs)
    }

    /// The trader's guardrails (roadmap 8b) for a new order (or a quantity increase).
    fn check_guardrails(&mut self, now: UtcMillis, spec: &OrderSpec) -> Result<(), Reject> {
        self.roll_day(now, &spec.account);
        self.guard_report(&spec.account);
        let position = self.position(&spec.account, spec.contract);
        let (open_buy, open_sell) = self.open_qty(&spec.account, spec.contract);
        // An option's market closes with the regular session, not the equity's extended hours.
        let kind = match spec.contract.option_key() {
            Some(_) => SessionKind::Rth,
            None => SessionKind::Eth,
        };
        let session_close = lines_feed::catalog::sessions(spec.contract.product)
            .and_then(|cal| cal.session_at(now, kind))
            .map(|s| s.close);
        let entry = Entry {
            side: spec.side,
            qty: spec.qty.0,
            position,
            open_buy,
            open_sell,
            session_close,
        };
        self.accounts[&spec.account]
            .guard
            .check(now, &entry)
            .map_err(|m| Reject::new(RejectCode::Guardrail, m))?;
        // The news blackout (exits always pass, as above).
        if !entry.is_exit()
            && let Some(rule) = self.news_rule(&spec.account)
        {
            let symbol = lines_feed::catalog::get(spec.contract.product).map_or("", |p| p.symbol);
            news::check(self.econ.as_deref(), &rule, symbol, now)
                .map_err(|m| Reject::new(RejectCode::Guardrail, m))?;
        }
        Ok(())
    }

    /// Flatten-action blackouts: every position whose product is inside a window is flattened
    /// once per window (a position that opens later in the window, from an entry resting since
    /// before it, is flattened too). A calendar that can't answer flattens nothing: entries are
    /// refused, positions are the trader's (a rule command, so it passes no checks and cancels that contract's
    /// orders too).
    fn news_flatten(&mut self, now: UtcMillis) {
        let Some(sched) = self.econ.clone() else {
            return;
        };
        self.news_flattened
            .retain(|(_, _, at)| at.0 + 7 * 86_400_000 > now.0);
        let mut todo = Vec::new();
        for (id, a) in &self.accounts {
            let Some(rule) = self
                .news_rule(id)
                .filter(|r| r.action == NewsAction::Flatten)
            else {
                continue;
            };
            for (contract, p) in &a.positions {
                if p.qty() == 0 {
                    continue;
                }
                let Some(product) = lines_feed::catalog::get(contract.product) else {
                    continue;
                };
                if let Some(e) = sched.window_at(
                    Some(product.symbol),
                    now,
                    rule.before_ms(),
                    rule.after_ms(),
                    rule.min_importance(),
                ) && !self.news_flattened.contains(&(id.clone(), *contract, e.at))
                {
                    todo.push((id.clone(), *contract, e.at));
                }
            }
        }
        for (account, contract, at) in todo {
            let id = self.submit(
                now,
                Source::Rule,
                Instant::now(),
                Command::Flatten {
                    account: account.clone(),
                    contract: Some(contract),
                },
            );
            // Done for this window only if accepted; a refused flatten is tried again on the
            // next tick while the position is open.
            let accepted = self
                .events
                .iter()
                .rev()
                .any(|e| matches!(e, Event::Command { id: i, result: Ok(_), .. } if *i == id));
            if accepted {
                self.news_flattened.insert((account, contract, at));
            }
        }
    }

    /// When the next flatten-action window opens, for any account that has one.
    fn news_wake(&self, now: UtcMillis) -> Option<UtcMillis> {
        let sched = self.econ.as_ref()?;
        self.accounts
            .keys()
            .filter_map(|id| self.news_rule(id))
            .filter(|r| r.action == NewsAction::Flatten)
            .filter_map(|r| sched.next_window_start(None, now, r.before_ms(), r.min_importance()))
            .min()
    }

    /// Report the account's guardrail state if it changed since last reported.
    fn guard_report(&mut self, account: &AccountId) {
        let Some(a) = self.accounts.get_mut(account) else {
            return;
        };
        let now = a.guard.change();
        if now != a.guard_seen {
            a.guard_seen = now.clone();
            self.events.push(Event::Guardrail {
                account: account.clone(),
                state: now.state,
                reason: now.reason,
                until: now.until,
            });
        }
    }

    /// The account's trade date at `t` and when it ends (its day product's calendar).
    fn trade_day(&self, account: &AccountId, t: UtcMillis) -> Option<(CivilDate, UtcMillis)> {
        let a = self.accounts.get(account)?;
        let product = lines_feed::catalog::get(ProductId(a.cfg.day_product))?;
        let date = lines_feed::disk::partition::trade_date(&product, t);
        let (_, end) = lines_feed::disk::partition::trade_date_span(&product, date);
        Some((date, end))
    }

    /// Pre-trade risk (decision 0006) for a new order. Counts it toward the submit rate when
    /// it passes.
    fn check_risk(
        &mut self,
        now: UtcMillis,
        spec: &OrderSpec,
        cs: &ContractSpec,
    ) -> Result<(), Reject> {
        let recent_submits = in_last_second(
            &mut self
                .accounts
                .get_mut(&spec.account)
                .expect("checked")
                .submits,
            now,
        );
        let acct = &self.accounts[&spec.account];
        let position = acct.positions.get(&spec.contract).map_or(0, Position::qty);
        let (open_buy, open_sell) = self.open_qty(&spec.account, spec.contract);
        let ctx = RiskContext {
            position,
            open_buy,
            open_sell,
            reference: self.marks.get(&spec.contract).and_then(Mark::price),
            day_pnl: self.equity(&spec.account)
                - self.accounts[&spec.account].tracker.day_open_balance,
            recent_submits,
        };
        risk::check(
            &self.accounts[&spec.account].cfg.limits,
            cs,
            spec.side,
            spec.qty,
            &spec.kind,
            &ctx,
        )?;
        if let Some(a) = self.accounts.get_mut(&spec.account) {
            a.submits.push_back(now);
        }
        Ok(())
    }

    /// A bracket exit held for its entry: the entry still works and the exit has nothing live
    /// (never activated, or its sibling closed what was open). It closes only what the entry
    /// will open, so it is not exposure yet. Once the entry fills, its exits are live orders.
    fn exit_held(&self, o: &Order) -> bool {
        matches!(o.role, Role::TakeProfit | Role::StopLoss)
            && (matches!(o.state, OrderState::Submitted | OrderState::Acked)
                || o.remaining().0 == 0)
            && o.parent
                .and_then(|p| self.orders.get(&p))
                .is_some_and(Order::is_open)
    }

    /// Open quantity on each side: every working order (live bracket exits included), an OCO
    /// pair once (only one of it can fill: its larger remainder). Held bracket exits are left
    /// out.
    fn open_qty(&self, account: &AccountId, contract: ContractKey) -> (i64, i64) {
        let mut b = 0;
        let mut s = 0;
        for o in self.open_orders(account).filter(|o| o.contract == contract) {
            if self.exit_held(o) {
                continue;
            }
            let sibling = o
                .oco
                .and_then(|i| self.orders.get(&i))
                .filter(|x| x.is_open() && x.side == o.side && !self.exit_held(x));
            let q = match sibling {
                Some(x) if x.id < o.id => continue, // counted with the pair's first
                Some(x) => o.remaining().0.max(x.remaining().0),
                None => o.remaining().0,
            };
            match o.side {
                Side::Buy => b += q,
                Side::Sell => s += q,
            }
        }
        (b, s)
    }

    /// Worst-case contracts (position plus open orders), in micro-equivalents, optionally with
    /// a new order added.
    fn micros_used(&self, account: &AccountId, extra: Option<(ContractKey, Side, Qty)>) -> u32 {
        let acct = &self.accounts[account];
        let mut contracts: Vec<ContractKey> = acct.positions.keys().copied().collect();
        contracts.extend(self.open_orders(account).map(|o| o.contract));
        if let Some((k, _, _)) = extra {
            contracts.push(k);
        }
        contracts.sort();
        contracts.dedup();
        // Prop caps count futures: options are outside prop rules (decision 0028).
        contracts.retain(|k| k.option_key().is_none());
        let mut total = 0u32;
        for k in contracts {
            let pos = acct.positions.get(&k).map_or(0, Position::qty);
            let (mut b, mut s) = self.open_qty(account, k);
            if let Some((ek, side, q)) = extra
                && ek == k
            {
                match side {
                    Side::Buy => b += q.0,
                    Side::Sell => s += q.0,
                }
            }
            let worst = (pos + b).abs().max((pos - s).abs());
            let micro = lines_feed::catalog::get(k.product)
                .and_then(|p| p.micro_equivalent(Qty(worst)).map(|(_, q)| q.0))
                .unwrap_or(worst);
            total = total.saturating_add(micro.clamp(0, u32::MAX as i64) as u32);
        }
        total
    }

    fn new_order(
        &mut self,
        now: UtcMillis,
        spec: &OrderSpec,
        role: Role,
        parent: Option<OrderId>,
    ) -> Order {
        let id = OrderId(self.next_order);
        self.next_order += 1;
        // Flatten's orders, and anything placed while the account may only reduce, are
        // reduce-only at the broker too.
        let reduce_only = role == Role::Flatten
            || std::mem::take(&mut self.next_reduce_only)
            || self.trading_state(&spec.account) == TradingState::ReducingOnly;
        let o = Order {
            id,
            account: spec.account.clone(),
            contract: spec.contract,
            side: spec.side,
            qty: spec.qty,
            kind: spec.kind,
            state: OrderState::Submitted,
            filled: Qty(0),
            fill_value: 0,
            role,
            parent,
            oco: None,
            triggered: false,
            created: now,
            updated: now,
            reduce_only,
            overfill: Qty(0),
        };
        self.orders.insert(id, o.clone());
        if parent.is_none() {
            self.events.push(Event::Order(o.clone()));
        }
        o
    }

    /// Hand a request to the broker, recording click → sent latency (decision 0012: the
    /// input event's time to the moment the order leaves the core).
    fn send(&mut self, env: &Envelope, now: UtcMillis, req: BrokerRequest) {
        let ctx = RequestContext::new(env.source, env.pane);
        self.broker.request_in_context(now, req, &ctx);
        let us = env.issued.elapsed().as_micros() as u64;
        self.latency.record(us);
        if lat::enabled()
            && let Ok(mut h) = self.hud.try_lock()
        {
            h.saw(now);
            h.click_sent.record(now.0, us);
        }
        tracing::info!(
            target: "lines::latency",
            command = env.id.0,
            source = ?env.source,
            click_to_sent_us = us,
            "order sent"
        );
    }

    // ---- broker events --------------------------------------------------------------

    /// Apply whatever the broker has due by `now` (acks after latency, fills, cancels).
    pub fn tick(&mut self, now: UtcMillis) {
        self.seen_now = self.seen_now.max(now);
        // Orders due before now first: one reopening a series settles with the rest.
        self.poll(now);
        if self.settle_options(now) {
            self.poll(now);
        }
        self.news_flatten(now);
        self.strategy_watch(now);
        self.position_brackets_sync(now);
    }

    /// When the broker next needs a tick.
    pub fn next_wake(&self) -> Option<UtcMillis> {
        // A lockout's end too (its event goes out on time).
        let cooldowns = self.accounts.values().filter_map(|a| a.guard.next_wake());
        // A news window opening (flatten), on the clock last seen.
        let news = self.news_wake(self.seen_now);
        self.broker
            .next_wake()
            .into_iter()
            .chain(cooldowns)
            .chain(news)
            .min()
    }

    fn poll(&mut self, now: UtcMillis) {
        // Lockouts that have run out end (reported): a cooldown at its time, a day lock with
        // its trade date.
        let ending: Vec<AccountId> = self
            .accounts
            .iter()
            .filter(|(_, a)| a.guard.next_wake().is_some_and(|t| t <= now))
            .map(|(k, _)| k.clone())
            .collect();
        for k in ending {
            self.roll_day(now, &k);
            // Still the same trade date (never wake in a loop): wait for its real end.
            let end = self.trade_day(&k, now).map_or(now, |(_, e)| e);
            if let Some(a) = self.accounts.get_mut(&k) {
                a.guard.extend_lock(now, end);
            }
            self.guard_report(&k);
        }
        let mut evs = std::mem::take(&mut self.broker_events);
        self.broker.poll(now, &mut evs);
        let mut touched: Vec<AccountId> = Vec::new();
        for e in evs.drain(..) {
            if let Some(a) = self.apply(e)
                && !touched.contains(&a)
            {
                touched.push(a);
            }
        }
        self.broker_events = evs;
        for a in &touched {
            self.cancel_stranded_exits(now, a);
        }
        for a in touched {
            self.account_changed(now, &a);
        }
    }

    /// Exits left open with nothing to close can only open or add to a position: reduce-only
    /// orders, and bracket exits whose entry is done, on a contract the account is flat in or
    /// on the position's own side. Cancel them (the broker should already have; this is the
    /// core's own guard).
    fn cancel_stranded_exits(&mut self, now: UtcMillis, account: &AccountId) {
        let stranded: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| {
                let bracket = matches!(o.role, Role::TakeProfit | Role::StopLoss);
                // A bracket exit whose entry still works is the broker's to hold (it protects
                // what the entry fills next), never the core's to cancel.
                let entry_working = bracket
                    && o.parent
                        .and_then(|p| self.orders.get(&p))
                        .is_some_and(Order::is_open);
                let exit = (o.reduce_only || bracket) && !entry_working;
                let position = self.position(account, o.contract);
                &o.account == account
                    && o.is_open()
                    && exit
                    && !self.cancelling.contains(&o.id)
                    && (position == 0 || o.side.sign() == position.signum())
            })
            .map(|o| o.id)
            .collect();
        for order in stranded {
            self.submit(
                now,
                Source::Rule,
                Instant::now(),
                Command::CancelOrder {
                    account: account.clone(),
                    order,
                },
            );
        }
    }

    /// Move an order to `next` if the lifecycle allows it.
    fn transition(&mut self, id: OrderId, next: OrderState, at: UtcMillis) -> Option<AccountId> {
        let o = self.orders.get_mut(&id)?;
        if !o.state.can_become(&next) {
            tracing::warn!("order {}: ignoring {:?} -> {:?}", id.0, o.state, next);
            return None;
        }
        o.state = next;
        o.updated = at;
        let o = o.clone();
        if !o.is_open() {
            self.finish(id);
        }
        let a = o.account.clone();
        self.events.push(Event::Order(o));
        Some(a)
    }

    fn finish(&mut self, id: OrderId) {
        self.cancelling.remove(&id);
        self.finished.push_back(id);
        while self.finished.len() > KEEP_FINISHED {
            if let Some(old) = self.finished.pop_front() {
                self.orders.remove(&old);
                self.adopted.remove(&old);
                self.strategy_forget(old);
            }
        }
    }

    fn apply(&mut self, e: BrokerEvent) -> Option<AccountId> {
        match e {
            BrokerEvent::Acked { id, at } => {
                // The ack round trip: first ack of a sent order, on the broker's clock.
                if lat::enabled()
                    && let Some(o) = self.orders.get(&id)
                    && matches!(o.state, OrderState::Submitted)
                    && let Ok(mut h) = self.hud.try_lock()
                {
                    h.saw(at);
                    let ms = at.0.saturating_sub(o.created.0).max(0);
                    h.ack_rtt.record(at.0, ms as u64 * 1000);
                }
                self.transition(id, OrderState::Acked, at)
            }
            BrokerEvent::Working { id, at } => self.transition(id, OrderState::Working, at),
            BrokerEvent::Rejected { id, reason, at } => {
                self.transition(id, OrderState::Rejected { reason }, at)
            }
            BrokerEvent::Cancelled { id, at, .. } => self.transition(id, OrderState::Cancelled, at),
            BrokerEvent::ExternalFill {
                account,
                contract,
                side,
                price,
                qty,
                fee,
                at,
                trade_id,
            } => {
                if !self.accounts.contains_key(&account) {
                    if self.held.len() >= KEEP_HELD {
                        self.held.pop_front();
                    }
                    self.held.push_back(BrokerEvent::ExternalFill {
                        account,
                        contract,
                        side,
                        price,
                        qty,
                        fee,
                        at,
                        trade_id,
                    });
                    return None;
                }
                if !self.first_fill(&trade_id) {
                    return None;
                }
                // No order and no position change: the broker's snapshot holds both.
                self.events.push(Event::Fill(FillEvent {
                    account: account.clone(),
                    order: EXTERNAL_ORDER,
                    contract,
                    side,
                    price,
                    qty,
                    fee,
                    realised: Money::ZERO,
                    at,
                    trade_id,
                    reconciled: false,
                }));
                Some(account)
            }
            BrokerEvent::Modified { order } => {
                let o = self.orders.get_mut(&order.id)?;
                o.qty = order.qty;
                o.kind = order.kind;
                o.updated = order.updated;
                // A bracket's exits become reduce-only from the entry's first fill; never undone.
                o.reduce_only |= order.reduce_only;
                let o = o.clone();
                let a = o.account.clone();
                self.events.push(Event::Order(o));
                Some(a)
            }
            BrokerEvent::Fill {
                id,
                trade_id,
                price,
                qty,
                fee,
                at,
            } => {
                if !self.first_fill(&trade_id) {
                    tracing::warn!("order {}: execution {trade_id} already applied", id.0);
                    return None;
                }
                let policy = self.overfill;
                let o = self.orders.get_mut(&id)?;
                let excess = qty.0 - o.remaining().0;
                if excess > 0 {
                    let account = o.account.clone();
                    tracing::error!(
                        "order {}: fill of {} with {} left ({policy:?})",
                        id.0,
                        qty.0,
                        o.remaining().0
                    );
                    self.events.push(Event::Overfill {
                        account: account.clone(),
                        order: id,
                        excess: Qty(excess),
                        recorded: policy == OverfillPolicy::Record,
                    });
                    if policy == OverfillPolicy::Reject {
                        return Some(account);
                    }
                }
                let o = self.orders.get_mut(&id)?;
                // A fill that arrives after the order finished (a cancel that lost the race,
                // reported late) still happened: it counts, the order keeps its final state.
                let finished = (!o.is_open()).then(|| o.state.clone());
                if let Some(s) = &finished {
                    tracing::warn!("order {}: fill after it was {}", id.0, s.label());
                }
                o.apply_fill(price, qty, at);
                if let Some(s) = finished {
                    o.state = s;
                }
                let o = o.clone();
                if !o.is_open() {
                    self.finish(id);
                }
                let spec = self.spec(o.contract)?;
                let day = self.trade_day(&o.account, at);
                let acct = self.accounts.get_mut(&o.account)?;
                let pos = acct.positions.entry(o.contract).or_default();
                let before = pos.qty();
                let realised = spec.money(pos.apply_fill(o.side, price, qty), 1);
                let after = pos.qty();
                acct.balance += realised - fee;
                if pos.is_flat() {
                    acct.positions.remove(&o.contract);
                }
                self.strategy_fill(&o, price, qty, fee, at);
                let acct = self.accounts.get_mut(&o.account)?;
                // Guardrails: round trips, losing streaks (a fill's P&L net of its fee).
                if let Some((date, end)) = day {
                    acct.guard.roll_day(date);
                    acct.guard
                        .on_fill(o.contract, before, after, realised - fee, at, end);
                }
                self.events.push(Event::Order(o.clone()));
                self.events.push(Event::Fill(FillEvent {
                    account: o.account.clone(),
                    order: id,
                    contract: o.contract,
                    side: o.side,
                    price,
                    qty,
                    fee,
                    realised,
                    at,
                    trade_id,
                    reconciled: false,
                }));
                self.position_event(&o.account, o.contract);
                Some(o.account)
            }
        }
    }

    // ---- market data ----------------------------------------------------------------

    /// Market data for a contract: marks (P&L, risk bands) and the broker's simulation.
    pub fn market(&mut self, now: UtcMillis, contract: ContractKey, inputs: &[MarketInput]) {
        self.seen_now = self.seen_now.max(now);
        self.spec(contract);
        for i in inputs {
            let m = self.marks.entry(contract).or_default();
            match *i {
                MarketInput::Trade { price, time, .. } => {
                    m.last = Some(price);
                    m.last_at = Some(time);
                }
                MarketInput::Last(price) => {
                    m.last = Some(price);
                    m.last_at = Some(now);
                }
                MarketInput::OptionQuote {
                    bid,
                    ask,
                    last,
                    time,
                } => {
                    // An option marks at its mid; a last trade only without a two-sided quote.
                    let (b, a) = crate::market::quote_sides(bid, ask);
                    // A stamp from the future counts as now, as in the paper broker.
                    let time = time.min(now);
                    // A late or re-seeded quote never replaces a newer one.
                    if m.quoted_at.is_some_and(|t| time < t) {
                        continue;
                    }
                    // Without both sides: its last, else the mark it had (an empty quote
                    // never erases open P&L).
                    let held = m.price();
                    m.bid = b;
                    m.ask = a;
                    m.last = if b.is_some() && a.is_some() {
                        None
                    } else {
                        last.or(held)
                    };
                    m.quoted_at = Some(time);
                }
                MarketInput::Bid(p) => m.bid = Some(p),
                MarketInput::Ask(p) => m.ask = Some(p),
                _ => {}
            }
            self.broker.market(now, contract, i);
        }
        self.poll(now);
        // Open P&L moved for every account holding the contract.
        let holders: Vec<AccountId> = self
            .accounts
            .iter()
            .filter(|(_, a)| a.positions.contains_key(&contract))
            .map(|(k, _)| k.clone())
            .collect();
        for a in holders {
            self.position_event(&a, contract);
            self.account_changed(now, &a);
        }
        self.strategy_watch(now);
        self.position_brackets_sync(now);
    }

    fn mark(&self, contract: ContractKey) -> Option<Price> {
        self.marks.get(&contract).and_then(Mark::price)
    }

    fn unrealised(&self, account: &AccountId) -> Money {
        let Some(a) = self.accounts.get(account) else {
            return Money::ZERO;
        };
        let mut sum = Money::ZERO;
        for (k, p) in &a.positions {
            if let (Some(m), Some(s)) = (self.mark(*k), self.specs.get(k)) {
                sum += s.money(p.unrealised(m), 1);
            }
        }
        sum
    }

    fn equity(&self, account: &AccountId) -> Money {
        self.accounts
            .get(account)
            .map_or(Money::ZERO, |a| a.balance + self.unrealised(account))
    }

    fn position_event(&mut self, account: &AccountId, contract: ContractKey) {
        let Some(a) = self.accounts.get(account) else {
            return;
        };
        let p = a.positions.get(&contract).cloned().unwrap_or_default();
        let unrealised = match (self.mark(contract), self.specs.get(&contract)) {
            (Some(m), Some(s)) => s.money(p.unrealised(m), 1),
            _ => Money::ZERO,
        };
        self.events.push(Event::Position(PositionView {
            account: account.clone(),
            contract,
            qty: p.qty(),
            avg_price: p.avg_price(),
            unrealised,
        }));
    }

    /// A new trade date for the account's day calendar closes the previous day.
    fn roll_day(&mut self, now: UtcMillis, account: &AccountId) {
        if now <= Self::BEFORE_DATA {
            return;
        }
        let Some(a) = self.accounts.get(account) else {
            return;
        };
        let Some(product) = lines_feed::catalog::get(ProductId(a.cfg.day_product)) else {
            return;
        };
        let date = lines_feed::disk::partition::trade_date(&product, now);
        let balance = a.balance;
        let a = self.accounts.get_mut(account).expect("checked");
        let new_day = a.tracker.day.is_some_and(|d| d != date);
        // The day closes under the rules it was traded with ...
        let preset = a.cfg.rules.clone().unwrap_or_else(no_rules);
        a.tracker.roll_day(&preset, date, balance);
        // ... then looser prop rules asked for earlier take over.
        if new_day && let Some(p) = a.pending_rules.take() {
            a.cfg.rules = p.rules;
        }
        // Guardrails: a new trade date ends a lock and brings in looser settings.
        a.guard.roll_day(date);
        a.guard.expire(now);
    }

    /// Equity changed: rules, breaches (and liquidation), and the account event.
    fn account_changed(&mut self, now: UtcMillis, account: &AccountId) {
        self.roll_day(now, account);
        let unrealised = self.unrealised(account);
        let micros = self.micros_used(account, None);
        let Some(a) = self.accounts.get_mut(account) else {
            return;
        };
        let balance = a.balance;
        let equity = balance + unrealised;
        let mut breaches = Vec::new();
        let status = a.cfg.rules.clone().map(|preset| {
            breaches = a.tracker.on_equity(&preset, balance, equity);
            a.tracker.status(&preset, balance, equity, micros)
        });
        let liquidate = a.cfg.rules.as_ref().is_some_and(|r| r.liquidate_on_breach);
        let view = Self::view_of(account, a, unrealised, status);
        self.events.push(Event::Account(view));
        self.after_account_view(now, account, breaches, liquidate, equity);
    }

    /// The account's view (rules status already computed).
    fn view_of(
        account: &AccountId,
        a: &AccountCache,
        unrealised: Money,
        status: Option<crate::rules::RuleStatus>,
    ) -> AccountView {
        let balance = a.balance;
        let equity = balance + unrealised;
        AccountView {
            account: account.clone(),
            balance,
            unrealised,
            equity,
            day_pnl: equity - a.tracker.day_open_balance,
            rules: status,
            blocked: a.cfg.rules.as_ref().and(a.tracker.blocked()),
            trading: a
                .cfg
                .rules
                .as_ref()
                .map_or(TradingState::Active, |r| a.tracker.trading_state(r)),
            limits: Box::new(crate::event::AccountLimits {
                account_size: a.cfg.starting_balance,
                rules_in_force: a.cfg.rules.clone(),
                rules_pending: a.pending_rules.clone(),
                guardrails_in_force: a.guard.active.clone(),
                guardrails_pending: a.guard.pending.clone(),
            }),
        }
    }

    /// The account's view after a settings change: what is in force and what waits. No
    /// breach is checked here (the next equity update does that, with the real balance), so
    /// changing a setting never breaches, liquidates or locks by itself.
    fn publish_view(&mut self, account: &AccountId) {
        let unrealised = self.unrealised(account);
        let micros = self.micros_used(account, None);
        let Some(a) = self.accounts.get(account) else {
            return;
        };
        let equity = a.balance + unrealised;
        let status = a
            .cfg
            .rules
            .as_ref()
            .map(|p| a.tracker.status(p, a.balance, equity, micros));
        let view = Self::view_of(account, a, unrealised, status);
        self.events.push(Event::Account(view));
    }

    /// What follows an account view: the day's loss for guardrails, breach events and a
    /// liquidating rule.
    fn after_account_view(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        breaches: Vec<crate::rules::Breach>,
        liquidate: bool,
        equity: Money,
    ) {
        // Guardrails: the day's loss (realised + unrealised, fees included).
        let day_pnl = equity - self.accounts[account].tracker.day_open_balance;
        if now > Self::BEFORE_DATA
            && let Some((_, end)) = self.trade_day(account, now)
            && let Some(a) = self.accounts.get_mut(account)
        {
            a.guard.on_day_pnl(day_pnl, end);
        }
        self.guard_report(account);
        for b in breaches {
            self.events.push(Event::Breach {
                account: account.clone(),
                breach: b,
            });
            if liquidate {
                // The rule closes the account's exposure, through the same command path.
                self.submit_internal(
                    now,
                    Source::Rule,
                    Command::Flatten {
                        account: account.clone(),
                        contract: None,
                    },
                );
            }
        }
    }

    // ---- rebuild ----------------------------------------------------------------------

    /// Replace the cache with the broker's state (after a (re)connect or restart). Orders the
    /// broker doesn't know any more are cancelled in the cache; local state never wins.
    pub fn rebuild(&mut self, now: UtcMillis) {
        // A broker reporting external fills: what it already has queued first (fills before
        // the positions they made), so a fill is never both in a rebuilt position and applied
        // after it.
        let external = self.broker.capabilities().external_fills;
        if external {
            self.poll(now);
        }
        let snap = self.broker.snapshot();
        // Contracts held before: one the snapshot no longer lists is reported flat.
        let before: Vec<(AccountId, ContractKey)> = self
            .accounts
            .iter()
            .flat_map(|(a, c)| c.positions.keys().map(move |k| (a.clone(), *k)))
            .collect();
        let mut open = 0;
        let mut seen: Vec<OrderId> = Vec::new();
        for ba in &snap.accounts {
            let Some(a) = self.accounts.get_mut(&ba.id) else {
                continue;
            };
            a.balance = ba.balance;
            if let Some((day, open)) = a.restore_day.take() {
                a.tracker.day = Some(day);
                a.tracker.day_open_balance = open;
            }
            a.positions = ba
                .positions
                .iter()
                .map(|p| (p.contract, p.position.clone()))
                .collect();
            for o in &ba.orders {
                seen.push(o.id);
                open += 1;
                self.next_order = self.next_order.max(o.id.0 + 1);
                let known = self.orders.get(&o.id).map(|c| (c.filled.0, c.fill_value));
                if external && known.is_none() {
                    self.adopted.insert(o.id);
                }
                self.orders.insert(o.id, o.clone());
                self.events.push(Event::Order(o.clone()));
                // The broker filled more than the cache knew (executions missed while
                // disconnected): report them as one fill with a deterministic id, so a repeat
                // reconciliation (another reconnect, a restart) never counts them twice.
                // A broker reporting external fills already sent those of adopted orders.
                if let Some((filled, value)) = known
                    && o.filled.0 > filled
                    && !self.adopted.contains(&o.id)
                {
                    let qty = o.filled.0 - filled;
                    let trade_id = format!("recon-{}-{}", o.id.0, o.filled.0);
                    if self.first_fill(&trade_id) {
                        // The missed fills' average price, to the price unit (exact when
                        // they share one price).
                        let price = Price(((o.fill_value - value) / qty as i128) as i64);
                        self.events.push(Event::Fill(FillEvent {
                            account: o.account.clone(),
                            order: o.id,
                            contract: o.contract,
                            side: o.side,
                            price,
                            qty: Qty(qty),
                            fee: Money::ZERO,
                            realised: Money::ZERO,
                            at: now,
                            trade_id,
                            reconciled: true,
                        }));
                    }
                }
            }
        }
        let gone: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| o.is_open() && !seen.contains(&o.id))
            .map(|o| o.id)
            .collect();
        for id in gone {
            if let Some(o) = self.orders.get_mut(&id) {
                // Not at the broker. Sent with no answer yet: its outcome is unknown, so it is
                // unresolved (never assumed working, never assumed gone). Acknowledged before:
                // it finished at the broker while we weren't listening.
                o.state = if o.state == OrderState::Submitted {
                    OrderState::Unresolved
                } else {
                    OrderState::Cancelled
                };
                o.updated = now;
                let o = o.clone();
                self.events.push(Event::Order(o));
            }
        }
        self.events.push(Event::Rebuilt {
            accounts: snap.accounts.len(),
            open_orders: open,
        });
        let ids: Vec<AccountId> = self.accounts.keys().cloned().collect();
        for a in ids {
            let mut contracts: Vec<ContractKey> =
                self.accounts[&a].positions.keys().copied().collect();
            for (b, k) in &before {
                if *b == a && !contracts.contains(k) {
                    contracts.push(*k);
                }
            }
            for k in contracts {
                self.spec(k);
                self.position_event(&a, k);
            }
            self.account_changed(now, &a);
        }
    }
}

fn no_rules() -> RulePreset {
    RulePreset {
        name: String::new(),
        label: String::new(),
        account_size: 0,
        trailing_drawdown: None,
        daily_loss_limit: None,
        max_micro_contracts: None,
        profit_target: None,
        consistency: None,
        liquidate_on_breach: false,
        on_breach: crate::rules::BreachStates::default(),
        news: None,
    }
}

/// Whole dollars with thousands separators ("50,000").
fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}
