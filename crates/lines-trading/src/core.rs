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
//! broker. Flatten, Cancel All and cancels skip rules and risk: they always work.
//!
//! Orders, positions and balances are the **broker's**; the core keeps a cache updated from
//! broker events and replaced wholesale by [`TradingCore::rebuild`] after a (re)connect.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use lines_core::{ContractKey, Price, ProductId, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::broker::{Broker, BrokerEvent, BrokerRequest, PaneContext, RequestContext};
use crate::command::{Command, Envelope, OrderSpec, Reject, RejectCode, Source};
use crate::event::{AccountView, Event, FillEvent, PositionView};
use crate::market::MarketInput;
use crate::order::{Order, OrderState, Role};
use crate::position::Position;
use crate::risk::{self, RiskContext, RiskLimits};
use crate::rules::{RulePreset, RuleTracker, TradingState};
use crate::types::{AccountId, CommandId, ContractSpec, Money, OrderId, OrderKind, Side};

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
    bid: Option<Price>,
    ask: Option<Price>,
}

impl Mark {
    fn price(&self) -> Option<Price> {
        self.last.or(match (self.bid, self.ask) {
            (Some(b), Some(a)) => Some(Price((b.0 + a.0).div_euclid(2))),
            (b, a) => b.or(a),
        })
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
    next_command: u64,
    latency: Latency,
    events: Vec<Event>,
    broker_events: Vec<BrokerEvent>,
    /// Terminal orders kept for display, oldest dropped first.
    finished: VecDeque<OrderId>,
    /// Orders with a cancel on its way (not sent twice).
    cancelling: HashSet<OrderId>,
    overfill: OverfillPolicy,
    /// Execution ids already applied, and their order (to forget the oldest).
    fill_ids: HashSet<String>,
    fill_order: VecDeque<String>,
}

const KEEP_FINISHED: usize = 2000;
/// Execution ids remembered for de-duplication (oldest forgotten first).
const KEEP_FILL_IDS: usize = 100_000;

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
            next_command: 1,
            latency: Latency::default(),
            events: Vec::new(),
            broker_events: Vec::new(),
            finished: VecDeque::new(),
            cancelling: HashSet::new(),
            overfill: OverfillPolicy::default(),
            fill_ids: HashSet::new(),
            fill_order: VecDeque::new(),
        }
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

    pub fn add_account(&mut self, cfg: AccountConfig, tracker: Option<RuleTracker>) {
        let start = Money::whole(cfg.starting_balance);
        self.accounts.insert(
            cfg.id.clone(),
            AccountCache {
                tracker: tracker.unwrap_or_else(|| RuleTracker::new(start)),
                balance: start,
                positions: BTreeMap::new(),
                cfg,
                submits: VecDeque::new(),
                modifies: VecDeque::new(),
            },
        );
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

    /// Handle a command whose id was assigned elsewhere (the threaded runtime assigns ids at
    /// the caller so it can return them without waiting).
    pub fn handle(&mut self, now: UtcMillis, env: Envelope) {
        self.next_command = self.next_command.max(env.id.0 + 1);
        let result = self.run(now, &env);
        self.events.push(Event::Command {
            id: env.id,
            source: env.source,
            at: now,
            result,
        });
        // Requests may complete at once (zero latency): apply what the broker has.
        self.poll(now);
    }

    fn run(&mut self, now: UtcMillis, env: &Envelope) -> Result<Vec<OrderId>, Reject> {
        let account = env.command.account().clone();
        if !self.accounts.contains_key(&account) {
            return Err(Reject::new(
                RejectCode::UnknownAccount,
                format!("no account {account}"),
            ));
        }
        self.roll_day(now, &account);
        match env.command.clone() {
            Command::PlaceOrder(spec) => {
                self.check_new(now, &spec)?;
                let o = self.new_order(now, &spec, Role::Single, None);
                let id = o.id;
                self.send(env, now, BrokerRequest::Place(o));
                Ok(vec![id])
            }
            Command::PlaceBracket(b) => {
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
                let spec = self.spec(o.contract).expect("known");
                if let Some(p) = price {
                    if !spec.on_tick_grid(p) {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            "price is off the tick grid",
                        ));
                    }
                    // A changed price is a new price: fat-finger band again.
                    let mut kind = o.kind;
                    if let OrderKind::Limit { price: lp } = &mut kind {
                        *lp = p;
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
            Command::Flatten { contract, .. } => {
                let mut ids = self.cancel_all(env, now, &account, contract);
                let positions: Vec<(ContractKey, i64)> = self.accounts[&account]
                    .positions
                    .iter()
                    .filter(|(k, _)| contract.is_none_or(|c| c == **k))
                    .map(|(k, p)| (*k, p.qty()))
                    .filter(|(_, q)| *q != 0)
                    .collect();
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
        let ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| &o.account == account && o.is_open())
            .filter(|o| contract.is_none_or(|c| c == o.contract))
            .map(|o| o.id)
            .collect();
        for &id in &ids {
            if self.cancelling.insert(id) {
                self.send(env, now, BrokerRequest::Cancel { id });
            }
        }
        ids
    }

    /// Validation, prop rules and risk for a new order.
    fn check_new(&mut self, now: UtcMillis, spec: &OrderSpec) -> Result<(), Reject> {
        let cs = self
            .spec(spec.contract)
            .ok_or_else(|| Reject::new(RejectCode::UnknownContract, "unknown contract"))?;
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

    fn open_qty(&self, account: &AccountId, contract: ContractKey) -> (i64, i64) {
        let mut b = 0;
        let mut s = 0;
        for o in self.open_orders(account).filter(|o| o.contract == contract) {
            // Held bracket exits are not exposure (they only ever close the entry).
            if matches!(o.role, Role::TakeProfit | Role::StopLoss) {
                continue;
            }
            match o.side {
                Side::Buy => b += o.remaining().0,
                Side::Sell => s += o.remaining().0,
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
        self.poll(now);
    }

    /// When the broker next needs a tick.
    pub fn next_wake(&self) -> Option<UtcMillis> {
        self.broker.next_wake()
    }

    fn poll(&mut self, now: UtcMillis) {
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
        for a in touched {
            self.account_changed(now, &a);
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
            }
        }
    }

    fn apply(&mut self, e: BrokerEvent) -> Option<AccountId> {
        match e {
            BrokerEvent::Acked { id, at } => self.transition(id, OrderState::Acked, at),
            BrokerEvent::Working { id, at } => self.transition(id, OrderState::Working, at),
            BrokerEvent::Rejected { id, reason, at } => {
                self.transition(id, OrderState::Rejected { reason }, at)
            }
            BrokerEvent::Cancelled { id, at, .. } => self.transition(id, OrderState::Cancelled, at),
            BrokerEvent::Modified { order } => {
                let o = self.orders.get_mut(&order.id)?;
                o.qty = order.qty;
                o.kind = order.kind;
                o.updated = order.updated;
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
                let acct = self.accounts.get_mut(&o.account)?;
                let pos = acct.positions.entry(o.contract).or_default();
                let realised = spec.money(pos.apply_fill(o.side, price, qty), 1);
                acct.balance += realised - fee;
                if pos.is_flat() {
                    acct.positions.remove(&o.contract);
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
        self.spec(contract);
        for i in inputs {
            let m = self.marks.entry(contract).or_default();
            match *i {
                MarketInput::Trade { price, .. } | MarketInput::Last(price) => m.last = Some(price),
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
        let Some(a) = self.accounts.get(account) else {
            return;
        };
        let Some(product) = lines_feed::catalog::get(ProductId(a.cfg.day_product)) else {
            return;
        };
        let date = lines_feed::disk::partition::trade_date(&product, now);
        let balance = a.balance;
        let a = self.accounts.get_mut(account).expect("checked");
        let preset = a.cfg.rules.clone().unwrap_or_else(no_rules);
        a.tracker.roll_day(&preset, date, balance);
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
        let view = AccountView {
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
        };
        self.events.push(Event::Account(view));
        for b in breaches {
            self.events.push(Event::Breach {
                account: account.clone(),
                breach: b,
            });
            if liquidate {
                // The rule closes the account's exposure, through the same command path.
                self.submit(
                    now,
                    Source::Rule,
                    Instant::now(),
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
        let snap = self.broker.snapshot();
        let mut open = 0;
        let mut seen: Vec<OrderId> = Vec::new();
        for ba in &snap.accounts {
            let Some(a) = self.accounts.get_mut(&ba.id) else {
                continue;
            };
            a.balance = ba.balance;
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
                self.orders.insert(o.id, o.clone());
                self.events.push(Event::Order(o.clone()));
                // The broker filled more than the cache knew (executions missed while
                // disconnected): report them as one fill with a deterministic id, so a repeat
                // reconciliation (another reconnect, a restart) never counts them twice.
                if let Some((filled, value)) = known
                    && o.filled.0 > filled
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
            let contracts: Vec<ContractKey> = self.accounts[&a].positions.keys().copied().collect();
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
    }
}
