//! The strategy gate (decision 0025): strategy controls, the checks every strategy command
//! passes before prop rules, risk and guardrails, and the pauses the core makes itself.
//! See [`crate::strategy`].

use std::time::Instant;

use lines_core::{ContractKey, Price, Qty, UtcMillis};

use super::TradingCore;
use crate::broker::Origin;
use crate::command::{Command, Envelope, OrderSpec, Reject, RejectCode, Source};
use crate::event::Event;
use crate::guardrails::GuardState;
use crate::order::{Order, Role};
use crate::rules::TradingState;
use crate::strategy::{HEARTBEAT_TIMEOUT_MS, Instance, StrategyId, StrategyState};
use crate::types::{AccountId, Money, OrderId, Side};

fn deny(message: impl Into<String>) -> Reject {
    Reject::new(RejectCode::Strategy, message)
}

impl TradingCore {
    /// A strategy instance's state and the reason it last paused or stopped.
    pub fn strategy_state(&self, id: StrategyId) -> Option<(StrategyState, Option<&str>)> {
        self.strategies
            .get(&id)
            .map(|i| (i.state, i.reason.as_deref()))
    }

    /// The strategy host is alive (decision 0025): armed instances pause after
    /// [`HEARTBEAT_TIMEOUT_MS`] of core time without one.
    pub fn strategy_heartbeat(&mut self, now: UtcMillis) {
        for i in self.strategies.values_mut() {
            i.beat = UtcMillis(i.beat.0.max(now.0));
        }
    }

    /// Strategy controls (add, remove, arm, pause, stop): `None` for every other command.
    pub(super) fn strategy_control(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
    ) -> Option<Result<Vec<OrderId>, Reject>> {
        let from = match env.source {
            Source::Strategy(s) => Some(s),
            _ => None,
        };
        Some(match &env.command {
            Command::AddStrategy(cfg) => {
                if from.is_some() {
                    return Some(Err(deny("a strategy can't add strategies")));
                }
                self.add_strategy(now, cfg.clone())
            }
            Command::RemoveStrategy { id } => {
                if from.is_some() {
                    return Some(Err(deny("a strategy can't remove strategies")));
                }
                self.remove_strategy(now, *id)
            }
            Command::ArmStrategy { id } => {
                // Only a person arms: never a strategy, a rule or the API.
                if Origin::of(env.source) != Origin::Manual {
                    return Some(Err(deny("only you can arm a strategy")));
                }
                self.arm_strategy(now, *id)
            }
            Command::PauseStrategy { id, reason } => {
                if from.is_some_and(|s| s != *id) {
                    return Some(Err(deny("a strategy may pause only itself")));
                }
                match self.strategies.get(id) {
                    None => Err(deny(format!("no {id}"))),
                    Some(i) if i.state != StrategyState::Armed => Ok(Vec::new()),
                    Some(_) => {
                        self.pause_strategy(now, *id, StrategyState::Paused, reason.clone());
                        Ok(Vec::new())
                    }
                }
            }
            Command::StopStrategies { id, flatten } => {
                if from.is_some() && (*id != from) {
                    return Some(Err(deny("a strategy may stop only itself")));
                }
                self.stop_strategies(now, env.source, *id, *flatten)
            }
            _ => return None,
        })
    }

    fn add_strategy(
        &mut self,
        now: UtcMillis,
        cfg: crate::strategy::StrategyConfig,
    ) -> Result<Vec<OrderId>, Reject> {
        if !self.accounts.contains_key(&cfg.account) {
            return Err(Reject::new(
                RejectCode::UnknownAccount,
                format!("no account {}", cfg.account),
            ));
        }
        if self.spec(cfg.contract).is_none() {
            return Err(Reject::new(RejectCode::UnknownContract, "unknown contract"));
        }
        cfg.limits.validate().map_err(deny)?;
        // One instance per account and contract: its position is the account's position.
        if let Some(other) = self.strategies.values().find(|i| {
            i.cfg.id != cfg.id && i.cfg.account == cfg.account && i.cfg.contract == cfg.contract
        }) {
            return Err(deny(format!(
                "{} already trades this account and contract",
                other.cfg.label
            )));
        }
        let id = cfg.id;
        match self.strategies.get_mut(&id) {
            Some(i) if i.state == StrategyState::Armed => {
                return Err(deny(format!(
                    "{} is armed; pause or stop it first",
                    i.cfg.label
                )));
            }
            Some(i) if i.cfg.account != cfg.account || i.cfg.contract != cfg.contract => {
                if i.orders
                    .iter()
                    .any(|o| self.orders.get(o).is_some_and(|o| o.is_open()))
                    || !i.position.is_flat()
                {
                    return Err(deny(format!(
                        "{} still has orders or a position; stop it with Flatten first",
                        i.cfg.label
                    )));
                }
                *i = Instance::new(cfg, now);
            }
            Some(i) => {
                i.cfg = cfg;
                i.state = StrategyState::Disarmed;
                i.reason = None;
            }
            None => {
                self.strategies.insert(id, Instance::new(cfg, now));
            }
        }
        self.strategy_event(id, false);
        Ok(Vec::new())
    }

    fn remove_strategy(&mut self, now: UtcMillis, id: StrategyId) -> Result<Vec<OrderId>, Reject> {
        if !self.strategies.contains_key(&id) {
            return Err(deny(format!("no {id}")));
        }
        self.pause_strategy(now, id, StrategyState::Stopped, "removed".into());
        self.strategy_event(id, true);
        self.strategies.remove(&id);
        // Its remaining orders (exits it left working) are nobody's: a new instance with the
        // same id must not inherit their fills.
        self.strategy_orders.retain(|_, s| *s != id);
        Ok(Vec::new())
    }

    fn arm_strategy(&mut self, now: UtcMillis, id: StrategyId) -> Result<Vec<OrderId>, Reject> {
        if !self.strategies.contains_key(&id) {
            return Err(deny(format!("no {id}")));
        }
        // Its own books must agree with the account (a person may have closed or changed the
        // position while it was paused): flat adopts flat; anything else must be flattened.
        // First, so a position a person closed doesn't count toward its daily loss.
        let (account, contract, own) = {
            let i = &self.strategies[&id];
            (i.cfg.account.clone(), i.cfg.contract, i.position.qty())
        };
        let held = self.position(&account, contract);
        if held != own {
            if held != 0 {
                return Err(deny(format!(
                    "can't arm: the account holds {held} and the strategy's own books {own}; flatten first"
                )));
            }
            self.strategies.get_mut(&id).expect("checked").position = Default::default();
        }
        if let Some(why) = self.strategy_blocker(now, id, false) {
            return Err(deny(format!("can't arm: {why}")));
        }
        let i = self.strategies.get_mut(&id).expect("checked");
        if i.state != StrategyState::Armed {
            i.state = StrategyState::Armed;
            i.reason = None;
            i.beat = now;
            self.strategy_event(id, false);
        }
        Ok(Vec::new())
    }

    fn stop_strategies(
        &mut self,
        now: UtcMillis,
        source: Source,
        id: Option<StrategyId>,
        flatten: bool,
    ) -> Result<Vec<OrderId>, Reject> {
        let ids: Vec<StrategyId> = match id {
            Some(id) if !self.strategies.contains_key(&id) => return Err(deny(format!("no {id}"))),
            Some(id) => vec![id],
            None => self.strategies.keys().copied().collect(),
        };
        // Everything stops first, so nothing a strategy queued can enter in between.
        for &id in &ids {
            if self.strategies[&id].state != StrategyState::Stopped {
                self.pause_strategy(now, id, StrategyState::Stopped, "stopped".into());
            }
        }
        if flatten {
            let mut targets: Vec<(AccountId, ContractKey)> = ids
                .iter()
                .map(|id| {
                    let c = &self.strategies[id].cfg;
                    (c.account.clone(), c.contract)
                })
                .collect();
            targets.sort();
            targets.dedup();
            for (account, contract) in targets {
                self.submit(
                    now,
                    source,
                    Instant::now(),
                    Command::Flatten {
                        account,
                        contract: Some(contract),
                    },
                );
            }
        }
        Ok(Vec::new())
    }

    /// Move an instance to Paused or Stopped with a reason, and cancel its entry orders
    /// (protective exits stay while a position is open).
    pub(super) fn pause_strategy(
        &mut self,
        now: UtcMillis,
        id: StrategyId,
        state: StrategyState,
        reason: String,
    ) {
        self.pause_keeping(now, id, state, reason, None);
    }

    /// [`Self::pause_strategy`], leaving `keep` alone (the order a person is modifying or
    /// cancelling themselves).
    fn pause_keeping(
        &mut self,
        now: UtcMillis,
        id: StrategyId,
        state: StrategyState,
        reason: String,
        keep: Option<OrderId>,
    ) {
        let Some(i) = self.strategies.get_mut(&id) else {
            return;
        };
        i.state = state;
        i.reason = Some(reason);
        let account = i.cfg.account.clone();
        let contract = i.cfg.contract;
        let mine: Vec<OrderId> = i.orders.iter().copied().collect();
        self.strategy_event(id, false);
        let position = self.position(&account, contract);
        let entries: Vec<OrderId> = mine
            .into_iter()
            .filter_map(|o| self.orders.get(&o))
            .filter(|o| o.is_open() && !self.cancelling.contains(&o.id) && Some(o.id) != keep)
            .filter(|o| match o.role {
                Role::Entry => true,
                // A single order that only closes part of the position is an exit.
                Role::Single => {
                    !(o.side.sign() == -position.signum() && o.remaining().0 <= position.abs())
                }
                Role::TakeProfit | Role::StopLoss | Role::Flatten => false,
            })
            .map(|o| o.id)
            .collect();
        for order in entries {
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

    fn strategy_event(&mut self, id: StrategyId, removed: bool) {
        if let Some(i) = self.strategies.get(&id) {
            self.events.push(Event::Strategy {
                id,
                state: i.state,
                reason: i.reason.clone(),
                removed,
            });
        }
    }

    /// Why an instance may not be armed (or must pause) now, if anything. `watch`: also the
    /// host's heartbeat.
    fn strategy_blocker(&mut self, now: UtcMillis, id: StrategyId, watch: bool) -> Option<String> {
        let (account, beat, limit) = {
            let i = self.strategies.get(&id)?;
            (
                i.cfg.account.clone(),
                i.beat,
                Money::whole(i.cfg.limits.max_daily_loss),
            )
        };
        if !self.broker.simulated() {
            return Some("strategies trade paper only until Live automation is enabled".into());
        }
        if !self.broker.connected() {
            return Some("the broker is disconnected".into());
        }
        let state = match self.trading_state(&account) {
            TradingState::Active => None,
            TradingState::ReducingOnly => Some("reducing only"),
            TradingState::Halted => Some("halted"),
        };
        if let Some(state) = state {
            let why = self.accounts[&account]
                .tracker
                .blocked()
                .map_or_else(String::new, |b| format!(": {b:?} limit breached"));
            return Some(format!("the account is {state}{why}"));
        }
        if self.guard_state(&account) != GuardState::Active {
            let why = self.accounts[&account].guard.change().reason;
            return Some(why.map_or_else(|| "a guardrail lockout".into(), |r| r.to_string()));
        }
        if self.strategy_day_pnl(now, id) <= -limit {
            return Some("its daily loss limit was reached".into());
        }
        if watch && now.0 - beat.0 > HEARTBEAT_TIMEOUT_MS {
            return Some("the strategy host stopped responding".into());
        }
        None
    }

    /// Pause armed instances that may no longer trade (after every command, broker tick and
    /// market batch).
    pub(super) fn strategy_watch(&mut self, now: UtcMillis) {
        let armed: Vec<StrategyId> = self
            .strategies
            .values()
            .filter(|i| i.state == StrategyState::Armed)
            .map(|i| i.cfg.id)
            .collect();
        for id in armed {
            if let Some(why) = self.strategy_blocker(now, id, true) {
                self.pause_strategy(now, id, StrategyState::Paused, why);
            }
        }
    }

    /// A person's order action on an account (and contract): pause the armed instances that
    /// trade it (decision 0023: automation pauses until re-enabled).
    pub(super) fn strategy_manual(&mut self, now: UtcMillis, account: &AccountId, cmd: &Command) {
        let keep = match cmd {
            Command::ModifyOrder { order, .. } | Command::CancelOrder { order, .. } => Some(*order),
            _ => None,
        };
        let contract = match cmd {
            Command::PlaceOrder(s) => Some(s.contract),
            Command::PlaceBracket(b) => Some(b.entry.contract),
            Command::ModifyOrder { order, .. } | Command::CancelOrder { order, .. } => {
                self.orders.get(order).map(|o| o.contract)
            }
            Command::CancelAll { contract, .. } | Command::Flatten { contract, .. } => *contract,
            _ => return,
        };
        let hit: Vec<(StrategyId, ContractKey)> = self
            .strategies
            .values()
            .filter(|i| i.state == StrategyState::Armed && &i.cfg.account == account)
            .filter(|i| contract.is_none_or(|c| c == i.cfg.contract))
            .map(|i| (i.cfg.id, i.cfg.contract))
            .collect();
        for (id, k) in hit {
            let label = self.spec(k).map_or_else(
                || "this contract".into(),
                |s| k.label(&s.product).as_str().to_owned(),
            );
            self.pause_keeping(
                now,
                id,
                StrategyState::Paused,
                format!("manual order activity on {label}"),
                keep,
            );
        }
    }

    /// The gate for a command from a strategy (before prop rules, risk and guardrails).
    pub(super) fn strategy_gate(
        &mut self,
        now: UtcMillis,
        id: StrategyId,
        cmd: &Command,
    ) -> Result<(), Reject> {
        let Some(i) = self.strategies.get(&id) else {
            return Err(deny(format!("no {id}")));
        };
        if !self.broker.simulated() {
            return Err(deny(
                "strategies trade paper only until Live automation is enabled",
            ));
        }
        let (account, contract, label) =
            (i.cfg.account.clone(), i.cfg.contract, i.cfg.label.clone());
        let mine = |o: &OrderId| i.orders.contains(o);
        let entry: Option<OrderSpec> = match cmd {
            Command::PlaceOrder(s) => Some(s.clone()),
            Command::PlaceBracket(b) => Some(b.entry.clone()),
            Command::ModifyOrder {
                account: a,
                order,
                qty,
                ..
            } => {
                if a != &account || !mine(order) {
                    return Err(deny(format!("{label} may change only its own orders")));
                }
                let Some(o) = self.orders.get(order) else {
                    return Err(deny(format!("{label} may change only its own orders")));
                };
                // Changing an entry (its price too: a new price can make it marketable) is an
                // entry; so is adding quantity to anything. Moving an exit is not.
                let position = self.position(&account, contract);
                let closing =
                    o.side.sign() == -position.signum() && o.remaining().0 <= position.abs();
                let entry_order = match o.role {
                    Role::Entry => true,
                    Role::Single => !closing,
                    Role::TakeProfit | Role::StopLoss | Role::Flatten => false,
                };
                let added = qty.map_or(0, |q| (q.0 - o.qty.0).max(0));
                (entry_order || added > 0).then(|| OrderSpec {
                    account: account.clone(),
                    contract,
                    side: o.side,
                    qty: Qty(added),
                    kind: o.kind,
                })
            }
            Command::CancelOrder { account: a, order } => {
                if a != &account || !mine(order) {
                    return Err(deny(format!("{label} may cancel only its own orders")));
                }
                return Ok(());
            }
            Command::CancelAll {
                account: a,
                contract: c,
            }
            | Command::Flatten {
                account: a,
                contract: c,
            } => {
                if a != &account || *c != Some(contract) {
                    return Err(deny(format!(
                        "{label} may cancel or flatten only its own contract"
                    )));
                }
                return Ok(());
            }
            _ => return Err(deny(format!("{label} can't send this command"))),
        };
        let Some(spec) = &entry else {
            // A modify that adds nothing (a new price, less quantity): an exit's adjustment.
            return Ok(());
        };
        if spec.account != account || spec.contract != contract {
            return Err(deny(format!(
                "{label} trades only its own account and contract"
            )));
        }
        // An order that only closes part of the position (less what open orders already
        // close) is an exit: never limited by state, hours, size, loss or rate. A bracket
        // always opens.
        let position = self.position(&account, contract);
        let (open_buy, open_sell) = self.open_qty(&account, contract);
        // Held bracket exits close the position too (each bracket once: its two exits are
        // one-cancels-other).
        let (held_buy, held_sell) = self.held_exits(&account, contract);
        let (open_buy, open_sell) = (open_buy + held_buy, open_sell + held_sell);
        let closable = match spec.side {
            Side::Sell if position > 0 => position - open_sell,
            Side::Buy if position < 0 => -position - open_buy,
            _ => 0,
        };
        let adds = matches!(cmd, Command::ModifyOrder { .. } | Command::PlaceBracket(_));
        if !adds && spec.qty.0 <= closable {
            return Ok(());
        }
        {
            let i = &self.strategies[&id];
            if i.state != StrategyState::Armed {
                let why = i
                    .reason
                    .as_deref()
                    .map_or_else(String::new, |r| format!(": {r}"));
                return Err(deny(format!("{label} is {}{why}", i.state.label())));
            }
            if !i.cfg.limits.hours.allows(now) {
                return Err(deny(format!("{label}: outside its allowed hours")));
            }
            let max = i.cfg.limits.max_position;
            let (mut b, mut s) = self.open_qty(&account, contract);
            match spec.side {
                Side::Buy => b += spec.qty.0,
                Side::Sell => s += spec.qty.0,
            }
            let worst = (position + b).abs().max((position - s).abs());
            if worst > max {
                return Err(deny(format!(
                    "{label}: would hold up to {worst}, over its max position of {max}"
                )));
            }
            let limit = Money::whole(self.strategies[&id].cfg.limits.max_daily_loss);
            if self.strategy_day_pnl(now, id) <= -limit {
                return Err(deny(format!("{label}: its daily loss limit was reached")));
            }
        }
        let i = self.strategies.get_mut(&id).expect("checked");
        let max = i.cfg.limits.max_orders_per_min;
        if i.rate(now) >= max {
            return Err(deny(format!(
                "{label}: more than {max} orders in a minute; slow down"
            )));
        }
        i.counting = true;
        Ok(())
    }

    /// After a strategy command: an entry the core accepted counts toward the instance's order
    /// rate (one refused later by prop rules, risk or guardrails doesn't).
    pub(super) fn strategy_counted(&mut self, id: StrategyId, now: UtcMillis, accepted: bool) {
        if let Some(i) = self.strategies.get_mut(&id)
            && std::mem::take(&mut i.counting)
            && accepted
        {
            i.recent.push_back(now);
        }
    }

    /// Open bracket exits on an account and contract, by side: each bracket's larger exit
    /// (its take-profit and stop-loss cancel each other, so they close the position once).
    fn held_exits(&self, account: &AccountId, contract: ContractKey) -> (i64, i64) {
        let mut per: std::collections::BTreeMap<OrderId, (Side, i64)> = Default::default();
        for o in self.open_orders(account).filter(|o| o.contract == contract) {
            if matches!(o.role, Role::TakeProfit | Role::StopLoss) {
                let key = o.parent.unwrap_or(o.id);
                let e = per.entry(key).or_insert((o.side, 0));
                e.1 = e.1.max(o.remaining().0);
            }
        }
        per.values().fold((0, 0), |(b, s), (side, q)| match side {
            Side::Buy => (b + q, s),
            Side::Sell => (b, s + q),
        })
    }

    /// The orders a strategy's Cancel All or Flatten may cancel: only its own (decision 0025:
    /// it never touches the trader's orders). `None` for everyone else's commands.
    pub(super) fn cancel_scope(
        &self,
        source: Source,
    ) -> Option<std::collections::BTreeSet<OrderId>> {
        match source {
            Source::Strategy(id) => Some(
                self.strategies
                    .get(&id)
                    .map(|i| i.orders.clone())
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }

    /// Orders a strategy command created belong to the instance.
    pub(super) fn strategy_owns(&mut self, id: StrategyId, cmd: &Command, ids: &[OrderId]) {
        // Only what it created: new orders (bracket exits with their entry) and its Flatten's
        // own orders, never the orders a Cancel All or Flatten cancelled.
        let ids: Vec<OrderId> = match cmd {
            Command::PlaceOrder(_) | Command::PlaceBracket(_) => ids.to_vec(),
            Command::Flatten { .. } => ids
                .iter()
                .copied()
                .filter(|o| self.orders.get(o).is_some_and(|o| o.role == Role::Flatten))
                .collect(),
            _ => return,
        };
        let ids = &ids[..];
        // Bracket exits are created with their entry; they belong to it too.
        let children: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| o.parent.is_some_and(|p| ids.contains(&p)))
            .map(|o| o.id)
            .collect();
        if let Some(i) = self.strategies.get_mut(&id) {
            for o in ids.iter().chain(children.iter()) {
                i.orders.insert(*o);
                self.strategy_orders.insert(*o, id);
            }
        }
    }

    /// A fill: the owning instance's own position and day P&L.
    ///
    /// A Flatten that someone else sent (a person's Stop with flatten, a prop rule's
    /// liquidation) closes the instance's own position too: its fills on the instance's
    /// account and contract count, up to what closes that position (pro rata fee).
    pub(super) fn strategy_fill(
        &mut self,
        order: &Order,
        price: Price,
        qty: Qty,
        fee: Money,
        at: UtcMillis,
    ) {
        let owner = self.strategy_orders.get(&order.id).copied();
        let id = match owner {
            Some(id) => id,
            None if order.role == Role::Flatten => {
                let Some(i) = self.strategies.values().find(|i| {
                    i.cfg.account == order.account
                        && i.cfg.contract == order.contract
                        && i.position.qty().signum() == -order.side.sign()
                }) else {
                    return;
                };
                i.cfg.id
            }
            None => return,
        };
        let Some(account) = self.strategies.get(&id).map(|i| i.cfg.account.clone()) else {
            return;
        };
        let day = self.trade_day(&account, at).map(|(d, _)| d);
        let Some(i) = self.strategies.get_mut(&id) else {
            return;
        };
        let Some(spec) = self.specs.get(&i.cfg.contract) else {
            return;
        };
        let (qty, fee) = if owner.is_some() && order.role != Role::Flatten {
            (qty, fee)
        } else {
            let closing = qty.0.min(i.position.qty().abs());
            (
                Qty(closing),
                Money(fee.0 * closing as i128 / qty.0.max(1) as i128),
            )
        };
        i.roll_day(day);
        let realised = spec.money(i.position.apply_fill(order.side, price, qty), 1);
        i.day_realised += realised - fee;
    }

    /// An order finished: forget it once nothing more can happen to it.
    pub(super) fn strategy_forget(&mut self, order: OrderId) {
        if let Some(id) = self.strategy_orders.remove(&order)
            && let Some(i) = self.strategies.get_mut(&id)
        {
            i.orders.remove(&order);
        }
    }

    /// The instance's P&L on today's trade date: realised (fees included) plus unrealised on
    /// its own position.
    fn strategy_day_pnl(&mut self, now: UtcMillis, id: StrategyId) -> Money {
        let Some(account) = self.strategies.get(&id).map(|i| i.cfg.account.clone()) else {
            return Money::ZERO;
        };
        let day = self.trade_day(&account, now).map(|(d, _)| d);
        let Some(i) = self.strategies.get_mut(&id) else {
            return Money::ZERO;
        };
        i.roll_day(day);
        let k = i.cfg.contract;
        let open = match (
            self.marks.get(&k).and_then(super::Mark::price),
            self.specs.get(&k),
        ) {
            (Some(m), Some(s)) => s.money(i.position.unrealised(m), 1),
            _ => Money::ZERO,
        };
        i.day_realised + open
    }
}
