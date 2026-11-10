//! The core's triggers on an option's underlying (decision 0029; the rules are in
//! `crate::trigger`). Nothing here places an order of its own kind: an entry is a `PlaceOrder`,
//! so every new-order check runs when it fires; a fired Stop is a `Flatten` of the series
//! (reduce-only, market, past the guard, as Flatten always is); a fired Target is a `PlaceExit`
//! re-priced by `ModifyOrder`. All of them go through `submit_internal` as `Source::Rule`.

use std::collections::{BTreeMap, HashMap};

use lines_core::{ContractKey, Price, ProductId, Qty, Right, UtcMillis};

use super::{Mark, TradingCore};
use crate::command::{Command, Envelope, OrderSpec, Reject, RejectCode, Source};
use crate::event::Event;
use crate::order::{OrderState, Role};
use crate::risk::OptionGuard;
use crate::strategy::StrategyState;
use crate::trigger::{
    Direction, SavedTrigger, TriggerId, TriggerKind, TriggerSpec, TriggerState, TriggerView,
    closing_side,
};
use crate::types::{AccountId, OrderId, OrderKind};

impl Triggers {
    /// A replay jumped back: the prints seen on the later clock would refuse every replayed
    /// one as older.
    pub(super) fn rewound(&mut self) {
        self.prints.clear();
    }
}

/// The core's trigger state.
#[derive(Default)]
pub(super) struct Triggers {
    next: u64,
    live: BTreeMap<TriggerId, Live>,
    /// The newest usable trade seen per underlying: its stamp and price (`marks` takes any trade,
    /// a late one too).
    prints: HashMap<ProductId, (UtcMillis, Price)>,
    /// A fired exit's own command is going through: past the account's risk limits, which block
    /// entries, never exits.
    pub(super) exempt: bool,
    /// Inside a step: commands it issues don't start another.
    stepping: bool,
    /// Saved exits waiting for the first rebuild.
    restoring: Vec<SavedTrigger>,
    /// Fired entries' limits, canceled if not filled in time.
    entries: Vec<FiredEntry>,
}

/// A fired Entry's limit and when it's canceled if still working.
struct FiredEntry {
    order: OrderId,
    until: UtcMillis,
    /// How long it was given, for the reason.
    wait: i64,
    /// The Entry as it ended, reported again with the cancel's reason.
    view: TriggerView,
}

struct Live {
    view: TriggerView,
    /// A fired exit's working order.
    order: Option<OrderId>,
    /// When a fired Target next re-prices.
    next_try: UtcMillis,
    /// The quote a fired exit last tried on: a Stop the broker couldn't fill waits for a new one.
    tried: Option<UtcMillis>,
    /// A person's Flatten is closing its position: if that's what ends it, it's no news. Cleared
    /// once that Flatten is over with the position still open.
    flattened: bool,
}

fn underlying(contract: ContractKey) -> ContractKey {
    ContractKey::equity(contract.product)
}

fn in_hours(product: ProductId, t: UtcMillis) -> bool {
    crate::paper::option_session_open(product, t)
}

impl Mark {
    /// The side of the quote that closes a position, while the quote is fresh.
    fn touch(&self, position: i64, now: UtcMillis, stale_after_ms: i64) -> Option<Price> {
        let at = self.quoted_at?;
        if now.0 - at.0 > stale_after_ms {
            return None;
        }
        if position > 0 { self.bid } else { self.ask }
    }
}

impl TradingCore {
    /// A strategy instance is on this contract in a way a trigger would trade under: armed or
    /// paused, or holding a position of its own. One merely added, or stopped and flat, isn't.
    fn strategy_holds(&self, account: &AccountId, contract: ContractKey) -> bool {
        self.strategies.values().any(|i| {
            &i.cfg.account == account
                && i.cfg.contract == contract
                && (matches!(i.state, StrategyState::Armed | StrategyState::Paused)
                    || i.position.qty() != 0)
        })
    }

    /// Why a strategy can't arm on this contract: a trigger is alive on it. The trigger stays: the
    /// trader decides, and an exit is never ended to make room.
    pub(super) fn trigger_blocks_strategy(
        &self,
        account: &AccountId,
        contract: ContractKey,
    ) -> Option<String> {
        self.triggers
            .live
            .values()
            .find(|l| &l.view.account == account && l.view.contract == contract)
            .map(|l| format!("{} trigger is on this option", l.view.kind.article_word()))
    }

    fn guard_of(&self, account: &AccountId) -> OptionGuard {
        self.accounts
            .get(account)
            .and_then(|a| a.cfg.limits.option_guard.clone())
            .unwrap_or_default()
    }

    /// Arm a trigger: refused for what it could never do, and what's wrong now.
    pub(super) fn trigger_arm(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
        spec: TriggerSpec,
    ) -> Result<Vec<OrderId>, Reject> {
        if matches!(env.source, Source::Strategy(_)) {
            return Err(Reject::new(
                RejectCode::Strategy,
                "strategies can't use triggers yet",
            ));
        }
        let Some(key) = spec.contract.option_key() else {
            return Err(Reject::new(
                RejectCode::Invalid,
                "triggers are on options, at a price of their underlying",
            ));
        };
        // Never together on a contract: firing would trade under the strategy, and its book would
        // miss the fills (`trigger_blocks_strategy` is the other way round).
        if self.strategy_holds(&spec.account, spec.contract) {
            return Err(Reject::new(
                RejectCode::Strategy,
                "a strategy is on this option: stop it and close what it holds first",
            ));
        }
        if self.accounts[&spec.account].cfg.rules.is_some() {
            return Err(Reject::new(
                RejectCode::PropRule,
                crate::risk::PROP_OPTIONS_REFUSAL,
            ));
        }
        if !self.broker.simulated() {
            return Err(Reject::new(
                RejectCode::Broker,
                format!(
                    "{} takes no option orders here: paper only",
                    self.broker.name()
                ),
            ));
        }
        if super::option_close(spec.contract).is_some_and(|c| now >= c) {
            return Err(Reject::new(RejectCode::Invalid, "this option has expired"));
        }
        let under = underlying(spec.contract);
        let Some(shares) = self.spec(under) else {
            return Err(Reject::new(
                RejectCode::UnknownContract,
                "unknown underlying",
            ));
        };
        if spec.level.0 <= 0 || !shares.on_tick_grid(spec.level) {
            return Err(Reject::new(
                RejectCode::Invalid,
                "the level isn't on the underlying's tick grid",
            ));
        }
        let guard = self.guard_of(&spec.account);
        let fresh_last = self
            .triggers
            .prints
            .get(&spec.contract.product)
            .filter(|(t, _)| now.0 - t.0 <= guard.trigger_stale_ms)
            .map(|(_, p)| *p);
        // Once an exit is closing the position, there's nothing left to arm on the series: an
        // entry would reopen it, or be sold by the exit.
        let closing = self.triggers.live.values().any(|l| {
            l.view.account == spec.account
                && l.view.contract == spec.contract
                && l.view.state == TriggerState::Working
        });
        if closing {
            return Err(Reject::new(
                RejectCode::Invalid,
                "an exit is already closing this position",
            ));
        }
        let direction = match spec.kind {
            TriggerKind::Entry { qty, max_price } => {
                if qty.0 <= 0 {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "quantity must be positive",
                    ));
                }
                let Some(cs) = self.spec(spec.contract) else {
                    return Err(Reject::new(RejectCode::UnknownContract, "unknown contract"));
                };
                if max_price.0 <= 0 || !cs.on_tick_grid(max_price) {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "the max price isn't on the option's tick grid",
                    ));
                }
                // A lockout now refuses it now; it's asked again when it fires. So does a buy
                // through a short to a long, while the trader is looking.
                let buy = OrderSpec {
                    account: spec.account.clone(),
                    contract: spec.contract,
                    side: crate::types::Side::Buy,
                    qty,
                    kind: OrderKind::Limit { price: max_price },
                };
                self.check_guardrails(now, &buy)?;
                self.check_option_through_flat(&buy)?;
                // Which way it fires comes from where the underlying is, so it has to be known.
                let Some(last) = fresh_last else {
                    return Err(Reject::new(
                        RejectCode::Risk,
                        "no fresh underlying price: an entry can't tell which way it fires",
                    ));
                };
                if last == spec.level {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "the underlying is already at this level",
                    ));
                }
                if last < spec.level {
                    Direction::AtOrAbove
                } else {
                    Direction::AtOrBelow
                }
            }
            kind => {
                let position = self.position(&spec.account, spec.contract);
                if position == 0 {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "no open position for this exit to close",
                    ));
                }
                let direction = Direction::of_exit(kind, key.right, position);
                if fresh_last.is_some_and(|l| direction.reached(spec.level, l)) {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        format!("the underlying is already past this {}", kind.word()),
                    ));
                }
                // A Stop and a Target on the same or crossed levels would be one print from both
                // firing; the two levels keep them apart, so one print can only reach one.
                let other = self
                    .triggers
                    .live
                    .values()
                    .find(|l| {
                        l.view.account == spec.account
                            && l.view.contract == spec.contract
                            && l.view.kind.is_exit()
                            && l.view.kind != kind
                            && l.view.state == TriggerState::Armed
                    })
                    .map(|l| l.view.level);
                if let Some(o) = other {
                    let stop_level = if kind == TriggerKind::Stop {
                        spec.level
                    } else {
                        o
                    };
                    let target_level = if kind == TriggerKind::Stop {
                        o
                    } else {
                        spec.level
                    };
                    let stop_dir = Direction::of_exit(TriggerKind::Stop, key.right, position);
                    let apart = match stop_dir {
                        Direction::AtOrBelow => stop_level < target_level,
                        Direction::AtOrAbove => stop_level > target_level,
                    };
                    if !apart {
                        return Err(Reject::new(
                            RejectCode::Invalid,
                            "the Stop and the Target must be apart, the Stop on the losing side",
                        ));
                    }
                }
                // One of each per position: a new one replaces the last.
                let same: Vec<TriggerId> = self
                    .triggers
                    .live
                    .values()
                    .filter(|l| {
                        l.view.account == spec.account
                            && l.view.contract == spec.contract
                            && l.view.kind == kind
                    })
                    .map(|l| l.view.id)
                    .collect();
                for id in same {
                    self.trigger_end(id, "replaced");
                }
                direction
            }
        };
        let id = self.trigger_id();
        let view = TriggerView {
            id,
            account: spec.account,
            contract: spec.contract,
            kind: spec.kind,
            level: spec.level,
            direction,
            state: TriggerState::Armed,
            armed_by: Some(env.id),
            stale: false,
            restored: false,
            note: None,
            reason: None,
        };
        self.trigger_insert(view);
        Ok(Vec::new())
    }

    fn trigger_id(&mut self) -> TriggerId {
        self.triggers.next = self.triggers.next.max(1);
        let id = TriggerId(self.triggers.next);
        self.triggers.next += 1;
        id
    }

    fn trigger_insert(&mut self, view: TriggerView) {
        self.events.push(Event::Trigger(view.clone()));
        self.triggers.live.insert(
            view.id,
            Live {
                view,
                order: None,
                next_try: UtcMillis(i64::MIN),
                tried: None,
                flattened: false,
            },
        );
    }

    /// End a trigger, with its reason: its last event.
    fn trigger_end(&mut self, id: TriggerId, reason: &str) -> Option<TriggerView> {
        let mut l = self.triggers.live.remove(&id)?;
        l.view.state = TriggerState::Ended;
        l.view.note = None;
        l.view.reason = Some(reason.to_owned());
        self.events.push(Event::Trigger(l.view.clone()));
        Some(l.view)
    }

    /// The trader moves one, as a single step: refused, it stays where it was.
    pub(super) fn trigger_move(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
        contract: ContractKey,
        id: TriggerId,
        level: Price,
    ) -> Result<Vec<OrderId>, Reject> {
        let account = env.command.account().cloned();
        let Some(view) = self
            .triggers
            .live
            .get(&id)
            .filter(|l| Some(&l.view.account) == account.as_ref() && l.view.contract == contract)
            .map(|l| l.view.clone())
        else {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!("no armed trigger {}", id.0),
            ));
        };
        if view.state != TriggerState::Armed {
            return Err(Reject::new(
                RejectCode::Invalid,
                "an exit is already closing this position",
            ));
        }
        let spec = TriggerSpec {
            account: view.account.clone(),
            contract: view.contract,
            kind: view.kind,
            level,
        };
        // An exit's own swap: every check of a new one, and the old stays if refused.
        if view.kind.is_exit() {
            return self.trigger_arm(now, env, spec);
        }
        if matches!(env.source, Source::Strategy(_)) {
            return Err(Reject::new(
                RejectCode::Strategy,
                "strategies can't use triggers yet",
            ));
        }
        let under = underlying(view.contract);
        let on_grid = self.spec(under).is_some_and(|s| s.on_tick_grid(level));
        if level.0 <= 0 || !on_grid {
            return Err(Reject::new(
                RejectCode::Invalid,
                "the level isn't on the underlying's tick grid",
            ));
        }
        if let TriggerKind::Entry { qty, max_price } = view.kind {
            self.check_guardrails(
                now,
                &OrderSpec {
                    account: view.account.clone(),
                    contract: view.contract,
                    side: crate::types::Side::Buy,
                    qty,
                    kind: OrderKind::Limit { price: max_price },
                },
            )?;
        }
        let guard = self.guard_of(&view.account);
        let fresh_last = self
            .triggers
            .prints
            .get(&view.contract.product)
            .filter(|(t, _)| now.0 - t.0 <= guard.trigger_stale_ms)
            .map(|(_, p)| *p);
        let Some(last) = fresh_last else {
            return Err(Reject::new(
                RejectCode::Risk,
                "no fresh underlying price: an entry can't tell which way it fires",
            ));
        };
        if last == level {
            return Err(Reject::new(
                RejectCode::Invalid,
                "the underlying is already at this level",
            ));
        }
        // It's the same entry: across the market it would be another trade.
        if view.direction.reached(level, last) {
            return Err(Reject::new(
                RejectCode::Invalid,
                "the wrong side of the market: it would fire the other way",
            ));
        }
        if let Some(l) = self.triggers.live.get_mut(&id) {
            l.view.level = level;
            let moved = l.view.clone();
            self.events.push(Event::Trigger(moved));
        }
        Ok(Vec::new())
    }

    /// The trader cancels one: a fired Target's order goes with it.
    pub(super) fn trigger_cancel(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        id: TriggerId,
    ) -> Result<Vec<OrderId>, Reject> {
        let Some(l) = self
            .triggers
            .live
            .get(&id)
            .filter(|l| &l.view.account == account && l.view.contract == contract)
        else {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!("no armed trigger {}", id.0),
            ));
        };
        let order = l.order;
        self.trigger_end(id, "canceled");
        self.trigger_cancel_order(now, account, order);
        Ok(Vec::new())
    }

    fn trigger_cancel_order(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        order: Option<OrderId>,
    ) {
        if let Some(order) = order.filter(|o| self.orders.get(o).is_some_and(|o| o.is_open())) {
            self.submit_internal(
                now,
                Source::Rule,
                Command::CancelOrder {
                    account: account.clone(),
                    order,
                },
            );
        }
    }

    /// A person's Cancel All ends the triggers it covers. A person's Flatten ends only entries,
    /// which would reopen what it closes: a Stop or Target ends by itself once the position is
    /// flat, so a Flatten that can't close leaves it protecting the position. A person cancelling
    /// a chasing Target's order stops the chase. Internal commands (`Source::Rule`) never do.
    pub(super) fn trigger_person_acted(&mut self, env: &Envelope) {
        if crate::broker::Origin::of(env.source) != crate::broker::Origin::Manual {
            return;
        }
        let (account, contract) = match &env.command {
            Command::CancelAll { account, contract } | Command::Flatten { account, contract } => {
                (account, *contract)
            }
            // Entries armed on it would add to the new position or reopen the old: they end. Its
            // exits end by themselves once the position flips.
            Command::Reverse {
                account, contract, ..
            } => {
                let entries: Vec<TriggerId> = self
                    .triggers
                    .live
                    .values()
                    .filter(|l| &l.view.account == account && l.view.contract == *contract)
                    .filter(|l| !l.view.kind.is_exit())
                    .map(|l| l.view.id)
                    .collect();
                for id in entries {
                    self.trigger_end(id, "you reversed");
                }
                return;
            }
            Command::CancelOrder { account, order } => {
                let chased: Vec<TriggerId> = self
                    .triggers
                    .live
                    .values()
                    .filter(|l| &l.view.account == account && l.order == Some(*order))
                    .map(|l| l.view.id)
                    .collect();
                for id in chased {
                    self.trigger_end(id, "you canceled its order");
                }
                return;
            }
            _ => return,
        };
        let flatten = matches!(env.command, Command::Flatten { .. });
        if flatten {
            for l in self.triggers.live.values_mut() {
                if &l.view.account == account
                    && contract.is_none_or(|c| c == l.view.contract)
                    && l.view.kind.is_exit()
                    && l.view.state == TriggerState::Armed
                {
                    l.flattened = true;
                }
            }
        }
        let covered: Vec<TriggerId> = self
            .triggers
            .live
            .values()
            .filter(|l| &l.view.account == account && contract.is_none_or(|c| c == l.view.contract))
            .filter(|l| !(flatten && l.view.kind.is_exit()))
            .map(|l| l.view.id)
            .collect();
        let why = match env.command {
            Command::Flatten { .. } => "you flattened",
            _ => "you canceled all",
        };
        for id in covered {
            self.trigger_end(id, why);
        }
    }

    /// A trade of an underlying (`time`: its stamp): what it reaches fires. Nothing else ever
    /// does, so a feed that stops can't fire one.
    pub(super) fn trigger_print(
        &mut self,
        now: UtcMillis,
        contract: ContractKey,
        price: Price,
        time: UtcMillis,
    ) {
        if contract != underlying(contract) {
            return;
        }
        let product = contract.product;
        let time = time.min(now);
        let prev = self.triggers.prints.get(&product).map(|(t, _)| *t);
        // A late print never replaces a newer one, nor fires.
        if prev.is_some_and(|p| time < p) {
            return;
        }
        // Too old to act on (a backlog arriving): no evidence the feed is back, so it doesn't
        // close a gap either.
        let limit = self
            .triggers
            .live
            .values()
            .map(|l| self.guard_of(&l.view.account).trigger_stale_ms)
            .max()
            .unwrap_or(crate::risk::DEFAULT_TRIGGER_STALE_MS);
        if now.0 - time.0 > limit {
            return;
        }
        self.triggers.prints.insert(product, (time, price));
        // Never in premarket or after hours.
        if !in_hours(product, time) {
            return;
        }
        let mut fired: Vec<(TriggerId, bool)> = Vec::new();
        for l in self.triggers.live.values() {
            let v = &l.view;
            if v.contract.product != product
                || v.state != TriggerState::Armed
                || !v.direction.reached(v.level, price)
            {
                continue;
            }
            let guard = self.guard_of(&v.account);
            // An old print says nothing about now.
            if now.0 - time.0 > guard.trigger_stale_ms {
                continue;
            }
            // After a gap (the feed stale, or the market opening) it's already past the level
            // rather than reaching it.
            let gap =
                prev.is_none_or(|p| !in_hours(product, p) || time.0 - p.0 > guard.trigger_stale_ms);
            fired.push((v.id, gap));
        }
        // A Stop first: one print can't reach both, but this order is the safe one if it could.
        fired.sort_by_key(|(id, _)| {
            self.triggers.live.get(id).map_or(2, |l| match l.view.kind {
                TriggerKind::Stop => 0,
                TriggerKind::Target => 1,
                TriggerKind::Entry { .. } => 2,
            })
        });
        // The commands these issue mustn't step the triggers underneath them.
        let stepping = std::mem::replace(&mut self.triggers.stepping, true);
        for (id, gap) in fired {
            let Some(l) = self.triggers.live.get(&id) else {
                continue; // its partner fired just now
            };
            match l.view.kind {
                TriggerKind::Entry { .. } if gap => {
                    self.trigger_end(
                        id,
                        "the underlying was already past the level when data returned",
                    );
                }
                TriggerKind::Entry { qty, max_price } => {
                    self.trigger_fire_entry(now, id, qty, max_price)
                }
                _ => self.trigger_fire_exit(now, id),
            }
        }
        self.triggers.stepping = stepping;
    }

    /// An entry: a limit at the ask, within the cap, past every new-order check, or its reason.
    /// One try: it never chases.
    fn trigger_fire_entry(&mut self, now: UtcMillis, id: TriggerId, qty: Qty, max_price: Price) {
        let Some(l) = self.triggers.live.get(&id) else {
            return;
        };
        let (account, contract) = (l.view.account.clone(), l.view.contract);
        let cs_price = |p: Price, s: &TradingCore| {
            s.specs.get(&contract).map_or_else(
                || p.0.to_string(),
                |c| c.product.price.format::<32>(p).as_str().to_owned(),
            )
        };
        let Some(ask) = self.marks.get(&contract).and_then(|m| m.ask) else {
            self.trigger_end(id, "no ask to buy at");
            return;
        };
        // A stale ask is no reason to give: the order check below says it's stale.
        let fresh = self
            .marks
            .get(&contract)
            .and_then(|m| m.quoted_at)
            .is_some_and(|t| now.0 - t.0 <= crate::risk::option_quote_stale_ms(contract, now));
        if fresh && ask > max_price {
            let why = format!(
                "the ask {} is above your max {}",
                cs_price(ask, self),
                cs_price(max_price, self)
            );
            self.trigger_end(id, &why);
            return;
        }
        // At least a second: a limit canceled before it could land would just flash.
        let wait = self.guard_of(&account).entry_fill_ms.max(1_000);
        let spec = OrderSpec {
            account,
            contract,
            side: crate::types::Side::Buy,
            qty,
            kind: OrderKind::Limit { price: ask },
        };
        let issued = self.trigger_issue(now, Command::PlaceOrder(spec));
        let why = match &issued {
            Ok(ids) => format!(
                "fired: limit at {}{}",
                cs_price(ask, self),
                ids.first()
                    .map_or(String::new(), |o| format!(", order {}", o.0))
            ),
            Err(r) => r.message.clone(),
        };
        let ended = self.trigger_end(id, &why);
        if let (Ok(ids), Some(view)) = (issued, ended)
            && let Some(&order) = ids.first()
        {
            self.triggers.entries.push(FiredEntry {
                order,
                until: UtcMillis(now.0.saturating_add(wait)),
                wait,
                view,
            });
        }
    }

    /// Fired entries' limits: done once they finish, canceled once their time is up. One with no
    /// known outcome (unresolved) is kept: a rebuild may find it working, and then it goes.
    fn entries_step(&mut self, now: UtcMillis) {
        let mut i = 0;
        while i < self.triggers.entries.len() {
            let e = &self.triggers.entries[i];
            let order = self.orders.get(&e.order);
            let working = order.is_some_and(|o| o.is_open());
            let unresolved = order.is_some_and(|o| o.state == OrderState::Unresolved);
            if unresolved || (working && now < e.until) {
                i += 1;
                continue;
            }
            let e = self.triggers.entries.remove(i);
            if !working {
                // The broker rejected it: nobody watches a ticket for a fired Entry, so its row
                // says so, in the broker's words.
                if let Some(OrderState::Rejected { reason }) = order.map(|o| &o.state) {
                    let mut view = e.view;
                    view.reason =
                        Some(format!("the broker rejected order {}: {reason}", e.order.0));
                    self.events.push(Event::Trigger(view));
                }
                continue;
            }
            let (filled, qty) = order.map_or((0, 0), |o| (o.filled.0, o.qty.0));
            let cancel = Command::CancelOrder {
                account: e.view.account.clone(),
                order: e.order,
            };
            let time = if e.wait % 1000 == 0 {
                format!("{} s", e.wait / 1000)
            } else {
                format!("{} ms", e.wait)
            };
            let why = match self.trigger_issue(now, cancel) {
                Ok(_) if filled > 0 => format!(
                    "filled {filled} of {qty} in {time}: canceling the rest of order {}",
                    e.order.0
                ),
                // Sent, not yet confirmed: a fill may still beat it, and the position says so.
                Ok(_) => format!("not filled in {time}: canceling order {}", e.order.0),
                Err(r) => format!(
                    "not filled in {time}, and order {} couldn't be canceled: {}",
                    e.order.0, r.message
                ),
            };
            let mut view = e.view;
            view.reason = Some(why);
            self.events.push(Event::Trigger(view));
        }
    }

    /// When a fired Entry's limit is next due to be canceled.
    pub(super) fn entries_wake(&self) -> Option<UtcMillis> {
        // Never one already past: it would spin the service's loop.
        self.triggers
            .entries
            .iter()
            .map(|e| e.until)
            .filter(|w| *w > self.seen_now)
            .min()
    }

    /// An exit fires: its partner ends, and it starts closing.
    fn trigger_fire_exit(&mut self, now: UtcMillis, id: TriggerId) {
        let Some(l) = self.triggers.live.get_mut(&id) else {
            return;
        };
        l.view.state = TriggerState::Working;
        // It closes the position now, if the Flatten didn't.
        l.flattened = false;
        let (account, contract, word) =
            (l.view.account.clone(), l.view.contract, l.view.kind.word());
        let view = l.view.clone();
        self.events.push(Event::Trigger(view));
        let partner: Vec<TriggerId> = self
            .triggers
            .live
            .values()
            .filter(|p| {
                // Its partner, and an entry that would reopen what this is closing.
                p.view.id != id && p.view.account == account && p.view.contract == contract
            })
            .map(|p| p.view.id)
            .collect();
        for p in partner {
            self.trigger_end(p, &format!("the {word} fired"));
        }
        self.trigger_step_exit(now, id);
    }

    /// A fired exit's command: past the account's risk limits (a lockout, a daily loss, a size
    /// limit or a price band blocks entries, never exits).
    fn trigger_issue_exit(
        &mut self,
        now: UtcMillis,
        command: Command,
    ) -> Result<Vec<OrderId>, Reject> {
        self.triggers.exempt = true;
        let r = self.trigger_issue(now, command);
        self.triggers.exempt = false;
        r
    }

    /// A command the core issues for a trigger, and its answer.
    fn trigger_issue(&mut self, now: UtcMillis, command: Command) -> Result<Vec<OrderId>, Reject> {
        let cmd = self.submit_internal(now, Source::Rule, command);
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Command { id, result, .. } if *id == cmd => Some(result.clone()),
                _ => None,
            })
            .unwrap_or_else(|| Err(Reject::new(RejectCode::Invalid, "no answer")))
    }

    /// Every trigger's watch: positions gone or flipped, expired series, a stale feed, and the
    /// fired exits' next move. Runs after everything that can change those.
    pub(super) fn triggers_step(&mut self, now: UtcMillis) {
        if self.triggers.stepping
            || (self.triggers.live.is_empty() && self.triggers.entries.is_empty())
        {
            return;
        }
        self.triggers.stepping = true;
        self.entries_step(now);
        let ids: Vec<TriggerId> = self.triggers.live.keys().copied().collect();
        for id in ids {
            let Some(l) = self.triggers.live.get(&id) else {
                continue;
            };
            let (account, contract, state, kind) = (
                l.view.account.clone(),
                l.view.contract,
                l.view.state,
                l.view.kind,
            );
            if super::option_close(contract).is_some_and(|c| now >= c) {
                self.trigger_end(id, "the option expired");
                continue;
            }
            if kind.is_exit() {
                let position = self.position(&account, contract);
                if position == 0 {
                    let flattened = self.triggers.live.get(&id).is_some_and(|l| l.flattened);
                    let why = if flattened {
                        "you flattened"
                    } else if state == TriggerState::Working {
                        "closed"
                    } else {
                        "the position closed"
                    };
                    self.trigger_end(id, why);
                    continue;
                }
                // That Flatten failed: what closes the position later isn't it.
                if self.triggers.live.get(&id).is_some_and(|l| l.flattened)
                    && !self.flatten_under_way(&account, contract, now)
                    && let Some(l) = self.triggers.live.get_mut(&id)
                {
                    l.flattened = false;
                }
                let right = contract.option_key().map_or(Right::Call, |k| k.right);
                let flipped =
                    self.triggers.live.get(&id).is_some_and(|l| {
                        Direction::of_exit(kind, right, position) != l.view.direction
                    });
                if flipped {
                    // It was armed for the other side: it never closes this one.
                    let order = self.triggers.live.get(&id).and_then(|l| l.order);
                    self.trigger_end(id, "the position flipped");
                    self.trigger_cancel_order(now, &account, order);
                    continue;
                }
                if state == TriggerState::Working {
                    self.trigger_step_exit(now, id);
                }
            }
            self.trigger_stale(now, id);
        }
        self.triggers.stepping = false;
    }

    /// Report a stale feed: in regular hours, a trigger with no recent print can't fire.
    fn trigger_stale(&mut self, now: UtcMillis, id: TriggerId) {
        let Some(l) = self.triggers.live.get(&id) else {
            return;
        };
        let product = l.view.contract.product;
        let limit = self.guard_of(&l.view.account).trigger_stale_ms;
        let stale = l.view.state == TriggerState::Armed
            && in_hours(product, now)
            && self
                .triggers
                .prints
                .get(&product)
                .is_none_or(|(t, _)| now.0 - t.0 > limit);
        if let Some(l) = self.triggers.live.get_mut(&id)
            && l.view.stale != stale
        {
            l.view.stale = stale;
            let v = l.view.clone();
            self.events.push(Event::Trigger(v));
        }
    }

    fn trigger_note(&mut self, id: TriggerId, note: Option<String>) {
        if let Some(l) = self.triggers.live.get_mut(&id)
            && l.view.note != note
        {
            l.view.note = note;
            let v = l.view.clone();
            self.events.push(Event::Trigger(v));
        }
    }

    /// The broker's own words when it rejected this trigger's last order: nobody watches a ticket
    /// for a fired trigger, so its row says it.
    fn trigger_rejected(&self, id: TriggerId) -> Option<String> {
        let order = self.triggers.live.get(&id)?.order?;
        match &self.orders.get(&order)?.state {
            OrderState::Rejected { reason } => {
                Some(format!("the broker rejected order {}: {reason}", order.0))
            }
            _ => None,
        }
    }

    /// A fired exit's next move. It always closes what the position is now.
    fn trigger_step_exit(&mut self, now: UtcMillis, id: TriggerId) {
        let Some(l) = self.triggers.live.get(&id) else {
            return;
        };
        let (account, contract, kind) = (l.view.account.clone(), l.view.contract, l.view.kind);
        let position = self.position(&account, contract);
        if position == 0 {
            return; // the step ends it
        }
        let guard = self.guard_of(&account);
        let mark = self.marks.get(&contract).copied().unwrap_or_default();
        let touch = mark.touch(
            position,
            now,
            crate::risk::option_quote_stale_ms(contract, now),
        );
        let side_word = if position > 0 { "bid" } else { "ask" };
        if !in_hours(contract.product, now) {
            self.trigger_note(
                id,
                Some("the option's market is closed: waiting".to_owned()),
            );
            return;
        }
        match kind {
            TriggerKind::Stop => {
                // Its market order is on its way or working.
                let working = self.orders.values().any(|o| {
                    o.is_open()
                        && o.role == Role::Flatten
                        && o.account == account
                        && o.contract == contract
                });
                if working {
                    return;
                }
                if touch.is_none() {
                    self.trigger_note(id, Some(format!("no usable {side_word}: waiting to exit")));
                    return;
                }
                // The broker couldn't fill on this quote, or rejected it: a new one, not a loop.
                let rejected = self.trigger_rejected(id);
                let tried = self.triggers.live.get(&id).and_then(|l| l.tried);
                if tried.is_some() && tried == mark.quoted_at {
                    let why = rejected.as_deref().map_or_else(
                        || format!("the {side_word} couldn't fill it"),
                        str::to_owned,
                    );
                    self.trigger_note(id, Some(format!("{why}: waiting for a new quote")));
                    return;
                }
                // Tried again, the rejection stays said until something else is.
                self.trigger_note(id, rejected.map(|r| format!("{r}: trying again")));
                if let Some(l) = self.triggers.live.get_mut(&id) {
                    l.tried = mark.quoted_at;
                }
                let flatten = Command::Flatten {
                    account: account.clone(),
                    contract: Some(contract),
                };
                match self.trigger_issue_exit(now, flatten) {
                    Ok(ids) => {
                        let market = ids
                            .iter()
                            .copied()
                            .find(|o| self.orders.get(o).is_some_and(|o| o.role == Role::Flatten));
                        if let Some(l) = self.triggers.live.get_mut(&id) {
                            l.order = market;
                        }
                    }
                    Err(r) => self.trigger_note(id, Some(r.message)),
                }
            }
            TriggerKind::Target => {
                self.trigger_step_target(
                    now, id, &account, contract, position, touch, side_word, &guard,
                );
            }
            TriggerKind::Entry { .. } => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn trigger_step_target(
        &mut self,
        now: UtcMillis,
        id: TriggerId,
        account: &AccountId,
        contract: ContractKey,
        position: i64,
        touch: Option<Price>,
        side_word: &str,
        guard: &OptionGuard,
    ) {
        let Some(price) = touch else {
            self.trigger_note(id, Some(format!("no usable {side_word}: waiting to exit")));
            return;
        };
        let (order, due, first) = match self.triggers.live.get(&id) {
            Some(l) => (l.order, l.next_try, l.next_try == UtcMillis(i64::MIN)),
            None => return,
        };
        let rejected = self.trigger_rejected(id);
        if now < due {
            // A rejection says so at once, not at the next chase.
            if let Some(r) = &rejected {
                self.trigger_note(id, Some(format!("{r}: trying again")));
            }
            return;
        }
        let open = order
            .and_then(|o| self.orders.get(&o))
            .filter(|o| o.is_open());
        let result = match open {
            // Still unanswered: no re-price on top of it.
            Some(o) if o.state == OrderState::Submitted => return,
            // At the bid already.
            Some(o) if matches!(o.kind, OrderKind::Limit { price: p } if p == price) => {
                Ok(Vec::new())
            }
            Some(o) => {
                let order = o.id;
                self.trigger_issue_exit(
                    now,
                    Command::ModifyOrder {
                        account: account.clone(),
                        order,
                        qty: None,
                        price: Some(price),
                    },
                )
            }
            None => {
                // First, what else works on the series: it mustn't take the exit's place, nor
                // be left to open a position once this one closes.
                if first {
                    let _ = self.trigger_issue(
                        now,
                        Command::CancelAll {
                            account: account.clone(),
                            contract: Some(contract),
                        },
                    );
                }
                self.trigger_issue_exit(
                    now,
                    Command::PlaceExit(OrderSpec {
                        account: account.clone(),
                        contract,
                        side: closing_side(position),
                        qty: Qty(position.abs()),
                        kind: OrderKind::Limit { price },
                    }),
                )
            }
        };
        let note = match result {
            Ok(ids) => {
                if let (Some(o), Some(l)) = (ids.first(), self.triggers.live.get_mut(&id)) {
                    l.order = Some(*o);
                }
                // Placed again after a rejection: the broker's words stay until it answers.
                rejected
                    .filter(|_| !ids.is_empty())
                    .map(|r| format!("{r}: trying again"))
            }
            Err(r) => Some(r.message),
        };
        if let Some(l) = self.triggers.live.get_mut(&id) {
            l.next_try = UtcMillis(now.0 + guard.target_chase_ms);
        }
        self.trigger_note(id, note);
    }

    /// When a trigger next needs a look: a chase's re-price, or a feed going stale.
    pub(super) fn triggers_wake(&self) -> Option<UtcMillis> {
        let now = self.seen_now;
        self.triggers
            .live
            .values()
            .filter_map(|l| {
                let product = l.view.contract.product;
                let limit = self.guard_of(&l.view.account).trigger_stale_ms;
                match l.view.state {
                    TriggerState::Working => Some(l.next_try),
                    TriggerState::Armed if !l.view.stale && in_hours(product, now) => self
                        .triggers
                        .prints
                        .get(&product)
                        .map(|(t, _)| UtcMillis(t.0 + limit + 1)),
                    _ => None,
                }
            })
            // Never one already past: it would spin the service's loop. Quotes and prints wake
            // it anyway for what waits on them.
            .filter(|w| *w > now)
            .min()
    }

    /// What the trading file keeps: Stops and Targets on a position still open. A fired one comes
    /// back armed: its first print past the level fires it again.
    pub fn saved_triggers(&self) -> Vec<SavedTrigger> {
        self.triggers
            .live
            .values()
            .filter(|l| l.view.kind.is_exit())
            .map(|l| SavedTrigger {
                id: l.view.id,
                account: l.view.account.clone(),
                contract: l.view.contract,
                kind: l.view.kind,
                level: l.view.level,
            })
            .collect()
    }

    /// Saved exits to restore after a restart. Positions come back with the core's first rebuild,
    /// so they wait for it: then armed again and marked restored, if the position is still open.
    pub fn restore_triggers(&mut self, saved: Vec<SavedTrigger>) {
        self.triggers.restoring = saved;
    }

    /// The first rebuild has the positions: bring the saved exits back. Reports each.
    pub(super) fn triggers_restore(&mut self) {
        let saved = std::mem::take(&mut self.triggers.restoring);
        let mut next = self.triggers.next.max(1);
        for t in saved {
            next = next.max(t.id.0 + 1);
            let position = self.position(&t.account, t.contract);
            let Some(key) = t.contract.option_key() else {
                continue;
            };
            if position == 0 || !t.kind.is_exit() {
                continue;
            }
            self.trigger_insert(TriggerView {
                id: t.id,
                account: t.account,
                contract: t.contract,
                kind: t.kind,
                level: t.level,
                direction: Direction::of_exit(t.kind, key.right, position),
                state: TriggerState::Armed,
                armed_by: None,
                stale: false,
                restored: true,
                note: None,
                reason: None,
            });
        }
        self.triggers.next = next;
    }

    /// Say what's alive: a rebuild is the full state as a subscriber's first events, so a restored
    /// exit's notice reaches one that wasn't listening yet.
    pub(super) fn triggers_publish(&mut self) {
        let views: Vec<TriggerView> = self
            .triggers
            .live
            .values()
            .map(|l| l.view.clone())
            .collect();
        for v in views {
            self.events.push(Event::Trigger(v));
        }
    }

    /// The triggers alive now.
    pub fn triggers(&self) -> impl Iterator<Item = &TriggerView> {
        self.triggers.live.values().map(|l| &l.view)
    }
}
