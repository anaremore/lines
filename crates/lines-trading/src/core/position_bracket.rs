//! Position brackets: one stop and one target for a whole manual position on an account and
//! contract ([`BracketMode::Position`]), instead of a bracket per entry.
//!
//! - The pair is placed when the position opens, at the mode's offsets from the average fill
//!   price (rounded onto the tick grid toward the entry), through the same exit path as a
//!   person's chart exit (`PlaceExit`: reduce-only, one-cancels-other). A leg that would fill or
//!   trigger at once is not sent.
//! - It follows the position: every sync compares each leg with the position and resizes it
//!   (prices kept, so a person's move stays); flat cancels the pair, a flip replaces it. Resizes
//!   go to the broker directly, so guardrail lockouts and modify limits never hold back
//!   protection; one still unanswered is sent again a second later, three times at most.
//! - A leg that ends while the position is open (filled while the position grew, or ended by
//!   the broker) is placed again; one refused (by the core or the broker) is tried again once a
//!   second, three tries in all. A leg a person cancelled stays cancelled: their own exits are
//!   theirs (not resized).
//! - Entries carry no exits of their own in this mode (a bracket order's exits are dropped),
//!   and a second exit of a kind whose leg stands is refused.
//! - Strategies keep their own exits: the mode is refused, and does nothing, while a strategy
//!   instance trades the account and contract (and a strategy can't be added while it is on).

use lines_core::{ContractKey, Price, Qty, UtcMillis};

use super::TradingCore;
use crate::broker::{BrokerRequest, Origin};
use crate::command::{BracketMode, Command, Envelope, OrderSpec, Reject, RejectCode, Source};
use crate::event::Event;
use crate::order::{Order, OrderState, Role};
use crate::types::{AccountId, CommandId, OrderId, OrderKind, Side};

/// A refused leg is tried again after this long (ms), and a resize not answered is sent again.
const RETRY_MS: i64 = 1_000;
/// Tries per leg (the first placement included) before it is left to the person.
const MAX_TRIES: u32 = 3;

/// How long a new pair waits for other exits to finish before saying so (ms).
const WAIT_REPORT_MS: i64 = 5_000;

/// One leg of the pair.
#[derive(Clone, Debug, Default)]
struct Leg {
    /// The working order.
    id: Option<OrderId>,
    /// Placements so far this position (a fill of the leg starts the count again).
    tries: u32,
    /// Place it (again) from this time on.
    retry_at: Option<UtcMillis>,
    /// A person cancelled it, or placed an exit of its kind while it was down: it is theirs
    /// until the position is flat.
    person_cancelled: bool,
    /// Its last price (a person may have moved it): used again when it is placed again.
    price: Option<Price>,
    /// Out of tries, and that was said.
    gave_up: bool,
    /// A resize sent and not seen yet: the quantity asked for, when, and how many times.
    asked: Option<(i64, UtcMillis, u32)>,
}

/// One account and contract in position mode.
#[derive(Clone, Debug)]
pub(super) struct PositionBracket {
    stop_ticks: u32,
    target_ticks: u32,
    stop: Leg,
    target: Leg,
    /// The position side (`Buy`: long) this episode's pair was placed for; `None` until it is.
    placed_for: Option<Side>,
    /// Since when a new pair has waited for other exits to finish, and whether that was said.
    waiting: Option<(UtcMillis, bool)>,
}

fn is_stop_kind(k: &OrderKind) -> bool {
    matches!(k, OrderKind::Stop { .. } | OrderKind::StopLimit { .. })
}

fn is_stop(o: &Order) -> bool {
    is_stop_kind(&o.kind)
}

/// `num / den` (den > 0) rounded down onto a grid of `tick`.
fn floor_tick(num: i128, den: i128, tick: i128) -> i64 {
    (num.div_euclid(den * tick) * tick) as i64
}

/// `num / den` (den > 0) rounded up onto a grid of `tick`.
fn ceil_tick(num: i128, den: i128, tick: i128) -> i64 {
    (-((-num).div_euclid(den * tick)) * tick) as i64
}

impl TradingCore {
    /// The bracket mode of manual trades on this account and contract.
    pub fn bracket_mode(&self, account: &AccountId, contract: ContractKey) -> BracketMode {
        self.position_brackets
            .get(&(account.clone(), contract))
            .map_or(BracketMode::PerEntry, |b| BracketMode::Position {
                stop_ticks: b.stop_ticks,
                target_ticks: b.target_ticks,
            })
    }

    /// How many working exits switching this account and contract to a position bracket would
    /// cancel (0 when there are none, or when they already are one stop and one target for the
    /// whole position, which are kept). What the confirmation names.
    pub fn exits_replaced_by_position_bracket(
        &self,
        account: &AccountId,
        contract: ContractKey,
    ) -> usize {
        let exits = self.exits_on(account, contract);
        if self.adoptable(account, contract, &exits) {
            0
        } else {
            exits.len()
        }
    }

    /// Open exits on the account and contract: reduce-only orders and bracket exits.
    fn exits_on(&self, account: &AccountId, contract: ContractKey) -> Vec<OrderId> {
        self.orders
            .values()
            .filter(|o| o.is_open() && &o.account == account && o.contract == contract)
            .filter(|o| o.reduce_only || matches!(o.role, Role::TakeProfit | Role::StopLoss))
            .map(|o| o.id)
            .collect()
    }

    /// Exits that already are a position bracket: at most one stop and one target, each a
    /// reduce-only exit for the whole open position on its closing side.
    fn adoptable(&self, account: &AccountId, contract: ContractKey, exits: &[OrderId]) -> bool {
        let pos = self.position(account, contract);
        if pos == 0 || exits.is_empty() {
            return exits.is_empty();
        }
        let closing = if pos > 0 { Side::Sell } else { Side::Buy };
        let os: Vec<&Order> = exits.iter().filter_map(|i| self.orders.get(i)).collect();
        os.iter().all(|o| {
            o.reduce_only
                && o.role == Role::Single
                && o.side == closing
                && o.qty.0 - o.filled.0 == pos.abs()
        }) && os.iter().filter(|o| is_stop(o)).count() <= 1
            && os.iter().filter(|o| !is_stop(o)).count() <= 1
    }

    /// Whether a strategy instance trades the account and contract (its exits are its own).
    fn strategy_trades(&self, account: &AccountId, contract: ContractKey) -> bool {
        self.strategies
            .values()
            .any(|i| &i.cfg.account == account && i.cfg.contract == contract)
    }

    /// An order working and not on its way out.
    fn live_order(&self, id: Option<OrderId>) -> Option<OrderId> {
        id.filter(|i| !self.cancelling.contains_key(i))
            .filter(|i| self.orders.get(i).is_some_and(Order::is_open))
    }

    /// Why an exit at this price would fill or trigger at once, if it would (on the last bid,
    /// ask or trade the core has; nothing known, nothing refused).
    fn bracket_crosses(
        &self,
        contract: ContractKey,
        side: Side,
        kind: &OrderKind,
    ) -> Option<String> {
        let m = self.marks.get(&contract)?;
        let market = match side {
            Side::Buy => m.ask.or(m.last),
            Side::Sell => m.bid.or(m.last),
        }?;
        let crosses = match (*kind, side) {
            (OrderKind::Limit { price }, Side::Sell) => market.0 >= price.0,
            (OrderKind::Limit { price }, Side::Buy) => market.0 <= price.0,
            (OrderKind::Stop { trigger }, Side::Sell) => market.0 <= trigger.0,
            (OrderKind::Stop { trigger }, Side::Buy) => market.0 >= trigger.0,
            _ => false,
        };
        crosses.then(|| {
            if is_stop_kind(kind) {
                "the market is already through the bracket's stop".to_owned()
            } else {
                "the market is already past the bracket's target".to_owned()
            }
        })
    }

    /// `Command::SetBracketMode`.
    pub(super) fn set_bracket_mode(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        mode: BracketMode,
        replace: bool,
    ) -> Result<Vec<OrderId>, Reject> {
        let key = (account.clone(), contract);
        let BracketMode::Position {
            stop_ticks,
            target_ticks,
        } = mode
        else {
            // Back to per entry: the pair stays as two ordinary exits, the person's now.
            self.position_brackets.remove(&key);
            return Ok(Vec::new());
        };
        if stop_ticks == 0 || target_ticks == 0 {
            return Err(Reject::new(
                RejectCode::Invalid,
                "a position bracket needs a stop and a target at least a tick away",
            ));
        }
        if self.spec(contract).is_none() {
            return Err(Reject::new(RejectCode::UnknownContract, "unknown contract"));
        }
        let caps = self.broker.capabilities();
        if !(caps.oco && caps.stop_orders && caps.modify) {
            return Err(Reject::new(
                RejectCode::Broker,
                format!(
                    "{} can't hold a position bracket (one-cancels-other stop and target it can resize)",
                    self.broker.name()
                ),
            ));
        }
        if self.strategy_trades(account, contract) {
            return Err(Reject::new(
                RejectCode::Invalid,
                "a strategy trades this contract on this account: its exits are its own",
            ));
        }
        // Already in position mode: new offsets apply to the next position; the pair stands.
        if let Some(b) = self.position_brackets.get_mut(&key) {
            b.stop_ticks = stop_ticks;
            b.target_ticks = target_ticks;
            return Ok(Vec::new());
        }
        let exits = self.exits_on(account, contract);
        let mut b = PositionBracket {
            stop_ticks,
            target_ticks,
            stop: Leg::default(),
            target: Leg::default(),
            placed_for: None,
            waiting: None,
        };
        let pos = self.position(account, contract);
        let adopt = self.adoptable(account, contract, &exits);
        if !adopt && !exits.is_empty() && !replace {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "switching to one bracket for the position cancels {} working exit order{}; confirm to replace them",
                    exits.len(),
                    if exits.len() == 1 { "" } else { "s" }
                ),
            ));
        }
        // A pair placed now for an open position must not fill or trigger at once.
        if pos != 0 && (exits.is_empty() || !adopt) {
            let (stop, target) = self.bracket_prices(account, contract, stop_ticks, target_ticks);
            let closing = if pos > 0 { Side::Sell } else { Side::Buy };
            for kind in [
                OrderKind::Stop { trigger: stop },
                OrderKind::Limit { price: target },
            ] {
                if let Some(why) = self.bracket_crosses(contract, closing, &kind) {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        format!("{why}: place this position's exits by hand"),
                    ));
                }
            }
        }
        let mut cancelled = Vec::new();
        if adopt {
            // One stop and one target already (as after a restart): they are the pair.
            for id in &exits {
                if is_stop(&self.orders[id]) {
                    b.stop.id = Some(*id);
                } else {
                    b.target.id = Some(*id);
                }
            }
            if !exits.is_empty() {
                b.placed_for = Some(if pos > 0 { Side::Buy } else { Side::Sell });
                // Only one of the two there: the other is placed (checked like any leg).
                for leg in [&mut b.stop, &mut b.target] {
                    if leg.id.is_none() {
                        leg.retry_at = Some(now);
                    }
                }
            }
        } else {
            for id in exits {
                self.send_cancel(env, now, id);
                cancelled.push(id);
            }
        }
        self.position_brackets.insert(key, b);
        Ok(cancelled)
    }

    /// Position mode on a person's command: a bracket order's exits are dropped (the position's
    /// pair covers it), and an exit of a kind whose leg stands is refused. `Err` refuses the
    /// command; `Ok(Some)` replaces it.
    pub(super) fn bracket_rewrite(&self, env: &Envelope) -> Result<Option<Command>, Reject> {
        if Origin::of(env.source) != Origin::Manual {
            return Ok(None);
        }
        match &env.command {
            Command::PlaceBracket(b)
                if self
                    .position_brackets
                    .contains_key(&(b.entry.account.clone(), b.entry.contract))
                    && !self.strategy_trades(&b.entry.account, b.entry.contract) =>
            {
                Ok(Some(Command::PlaceOrder(b.entry.clone())))
            }
            Command::PlaceExit(s) => {
                let Some(b) = self.position_brackets.get(&(s.account.clone(), s.contract)) else {
                    return Ok(None);
                };
                let leg = if is_stop_kind(&s.kind) {
                    &b.stop
                } else {
                    &b.target
                };
                if self.live_order(leg.id).is_some() {
                    return Err(Reject::new(
                        RejectCode::Invalid,
                        "this position has one bracket: move its stop or target instead",
                    ));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Keep every position bracket in step with its position (after each command, tick and
    /// market batch, once the broker's events are applied).
    pub(super) fn position_brackets_sync(&mut self, now: UtcMillis) {
        let keys: Vec<(AccountId, ContractKey)> = self.position_brackets.keys().cloned().collect();
        for (account, contract) in keys {
            if !self.accounts.contains_key(&account) || self.strategy_trades(&account, contract) {
                continue;
            }
            self.position_bracket_sync(now, &account, contract);
        }
    }

    /// A person's cancel (× or Cancel All) of a leg, or their own exit of a leg's kind while it
    /// is down: that leg is theirs while the position is open (called by `handle` with the
    /// command's result).
    pub(super) fn bracket_note_cancel(
        &mut self,
        env: &Envelope,
        result: &Result<Vec<OrderId>, Reject>,
    ) {
        if Origin::of(env.source) != Origin::Manual {
            return;
        }
        let Ok(ids) = result else {
            return;
        };
        match &env.command {
            Command::CancelOrder { .. } | Command::CancelAll { .. } => {
                for b in self.position_brackets.values_mut() {
                    for leg in [&mut b.stop, &mut b.target] {
                        if leg.id.is_some_and(|i| ids.contains(&i)) {
                            leg.person_cancelled = true;
                        }
                    }
                }
            }
            Command::PlaceExit(spec) => {
                if let Some(b) = self
                    .position_brackets
                    .get_mut(&(spec.account.clone(), spec.contract))
                {
                    let leg = if is_stop_kind(&spec.kind) {
                        &mut b.stop
                    } else {
                        &mut b.target
                    };
                    if leg.id.is_none() {
                        leg.person_cancelled = true;
                        leg.retry_at = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// What became of a leg's order: still working; cancelled by a person (it stays so); or
    /// ended otherwise (filled, refused or ended by the broker), so it is placed again for what
    /// is still open (a fill starts the count of tries again; a refusal waits a second).
    /// Whether the leg filled (then the position it protected is over: a new price applies).
    fn leg_refresh(&self, now: UtcMillis, leg: &mut Leg) -> bool {
        let Some(id) = leg.id else {
            return false;
        };
        if self.live_order(Some(id)).is_some() {
            return false;
        }
        let state = self.orders.get(&id).map(|o| o.state.clone());
        // Sent with no known outcome (a reconnect): never assumed gone, never placed again.
        if matches!(state, Some(OrderState::Unresolved)) && !self.cancelling.contains_key(&id) {
            return false;
        }
        leg.id = None;
        leg.asked = None;
        if leg.person_cancelled {
            leg.retry_at = None;
            return false;
        }
        let filled = matches!(state, Some(OrderState::Filled));
        match state {
            Some(OrderState::Filled) => {
                leg.tries = 0;
                leg.retry_at = Some(now);
            }
            Some(OrderState::Rejected { .. }) => {
                leg.retry_at = (leg.tries < MAX_TRIES).then_some(UtcMillis(now.0 + RETRY_MS));
            }
            _ => leg.retry_at = (leg.tries < MAX_TRIES).then_some(now),
        }
        filled
    }

    fn position_bracket_sync(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
    ) {
        let key = (account.clone(), contract);
        let Some(mut b) = self.position_brackets.get(&key).cloned() else {
            return;
        };
        let a = self.leg_refresh(now, &mut b.stop);
        let c = self.leg_refresh(now, &mut b.target);
        if a || c {
            // A leg filled: what is open now is a new position (flat and reopened, or an add
            // left over), so its legs go at the offsets from its own average.
            b.stop.price = None;
            b.target.price = None;
        }
        let pos = self.position(account, contract);
        let side = if pos > 0 { Side::Buy } else { Side::Sell };
        if pos == 0 || b.placed_for.is_some_and(|s| s != side) {
            // Flat: the pair goes. Flipped: it goes, and the new side gets its own pair.
            for id in [b.stop.id, b.target.id].into_iter().flatten() {
                let _ = self.bracket_command(
                    now,
                    Command::CancelOrder {
                        account: account.clone(),
                        order: id,
                    },
                );
            }
            b.stop = Leg::default();
            b.target = Leg::default();
            b.placed_for = None;
            b.waiting = None;
            if pos == 0 {
                self.position_brackets.insert(key, b);
                return;
            }
        }
        let size = pos.abs();
        let closing = side.opposite();
        // A Flatten on its way closes the position: nothing is placed meanwhile (what it
        // leaves open, if it only partly fills, gets its legs after).
        let flattening = self.orders.values().any(|o| {
            o.is_open()
                && o.role == Role::Flatten
                && &o.account == account
                && o.contract == contract
        });
        if flattening {
            self.position_brackets.insert(key, b);
            return;
        }
        if b.placed_for.is_none() {
            // Wait for any exit still working here (one being replaced or cancelled) to go, so
            // exits never add up to more than the position.
            let waiting = self.exits_on(account, contract);
            if !waiting.is_empty() {
                let (since, said) = *b.waiting.get_or_insert((now, false));
                if !said && now.0 - since.0 >= WAIT_REPORT_MS {
                    // A cancel never confirmed: say so (the position has no pair yet).
                    let env = self.bracket_envelope(Command::CancelAll {
                        account: account.clone(),
                        contract: Some(contract),
                    });
                    self.events.push(Event::Command {
                        id: env.id,
                        source: env.source,
                        at: now,
                        result: Err(Reject::new(
                            RejectCode::Broker,
                            format!(
                                "the position's bracket waits for {} exit order{} to finish; this position has no bracket yet",
                                waiting.len(),
                                if waiting.len() == 1 { "" } else { "s" }
                            ),
                        )),
                    });
                    b.waiting = Some((since, true));
                }
                self.position_brackets.insert(key, b);
                return;
            }
            b.waiting = None;
            b.placed_for = Some(side);
            let mut stop = Leg::default();
            let mut target = Leg::default();
            self.bracket_place(now, account, contract, &b, true, closing, size, &mut stop);
            self.bracket_place(
                now,
                account,
                contract,
                &b,
                false,
                closing,
                size,
                &mut target,
            );
            b.stop = stop;
            b.target = target;
            self.position_brackets.insert(key, b);
            return;
        }
        for is_stop_leg in [true, false] {
            let mut leg = if is_stop_leg {
                b.stop.clone()
            } else {
                b.target.clone()
            };
            if leg.id.is_none() && !leg.person_cancelled && leg.tries >= MAX_TRIES && !leg.gave_up {
                leg.gave_up = true;
                let env = self.bracket_envelope(Command::CancelAll {
                    account: account.clone(),
                    contract: Some(contract),
                });
                self.events.push(Event::Command {
                    id: env.id,
                    source: env.source,
                    at: now,
                    result: Err(Reject::new(
                        RejectCode::Broker,
                        format!(
                            "the position's {} could not be placed again after {MAX_TRIES} tries: place it by hand",
                            if is_stop_leg { "stop" } else { "target" }
                        ),
                    )),
                });
            }
            if leg.id.is_none()
                && !leg.person_cancelled
                && leg.tries < MAX_TRIES
                && leg.retry_at.is_some_and(|t| now.0 >= t.0)
            {
                self.bracket_place(
                    now,
                    account,
                    contract,
                    &b,
                    is_stop_leg,
                    closing,
                    size,
                    &mut leg,
                );
            }
            if let Some(id) = leg.id
                && self.orders[&id].state != OrderState::Unresolved
            {
                leg.price = self.orders[&id].kind.prices().next();
                // The leg to the position, prices kept. Straight to the broker (no guardrail,
                // rate or growth checks: this is protection, and it never exceeds the
                // position); one unanswered is sent again a second later, three times in all,
                // then the miss is said once.
                let o = &self.orders[&id];
                let want = o.filled.0 + size;
                match leg.asked {
                    _ if o.qty.0 == want => leg.asked = None,
                    Some((q, at, n)) if q == want => {
                        if n < MAX_TRIES && now.0 - at.0 >= RETRY_MS {
                            self.bracket_resize(now, id, Qty(want));
                            leg.asked = Some((want, now, n + 1));
                        } else if n == MAX_TRIES && now.0 - at.0 >= RETRY_MS {
                            let env = self.bracket_envelope(Command::ModifyOrder {
                                account: account.clone(),
                                order: id,
                                qty: Some(Qty(want)),
                                price: None,
                            });
                            self.events.push(Event::Command {
                                id: env.id,
                                source: env.source,
                                at: now,
                                result: Err(Reject::new(
                                    RejectCode::Broker,
                                    format!(
                                        "the broker didn't resize the position's {} to {size}",
                                        if is_stop_leg { "stop" } else { "target" }
                                    ),
                                )),
                            });
                            leg.asked = Some((want, at, n + 1));
                        }
                    }
                    _ => {
                        self.bracket_resize(now, id, Qty(want));
                        leg.asked = Some((want, now, 1));
                    }
                }
            }
            if is_stop_leg {
                b.stop = leg;
            } else {
                b.target = leg;
            }
        }
        self.position_brackets.insert(key, b);
    }

    /// Place one leg for `size`, at its offset from the position's average; not sent when it
    /// would fill or trigger at once. A refusal (published as the command's result) is tried
    /// again a second later while tries are left.
    #[allow(clippy::too_many_arguments)]
    fn bracket_place(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        b: &PositionBracket,
        is_stop_leg: bool,
        closing: Side,
        size: i64,
        leg: &mut Leg,
    ) {
        leg.tries += 1;
        let (stop, target) = self.bracket_prices(account, contract, b.stop_ticks, b.target_ticks);
        let at = |p: Price| {
            if is_stop_leg {
                OrderKind::Stop { trigger: p }
            } else {
                OrderKind::Limit { price: p }
            }
        };
        let offset = at(if is_stop_leg { stop } else { target });
        // A leg placed again keeps the price it had (a person may have moved it), unless the
        // market is past it now.
        let kind = match leg.price.map(at) {
            Some(k) if self.bracket_crosses(contract, closing, &k).is_none() => k,
            _ => offset,
        };
        let command = Command::PlaceExit(OrderSpec {
            account: account.clone(),
            contract,
            side: closing,
            qty: Qty(size),
            kind,
        });
        let result = match self.bracket_crosses(contract, closing, &kind) {
            Some(why) => {
                let env = self.bracket_envelope(command);
                let r = Reject::new(RejectCode::Invalid, why);
                self.events.push(Event::Command {
                    id: env.id,
                    source: env.source,
                    at: now,
                    result: Err(r.clone()),
                });
                Err(r)
            }
            None => self.bracket_command(now, command),
        };
        match result.ok().and_then(|ids| ids.first().copied()) {
            Some(id) => {
                leg.id = Some(id);
                leg.retry_at = None;
            }
            None => {
                leg.retry_at = (leg.tries < MAX_TRIES).then_some(UtcMillis(now.0 + RETRY_MS));
            }
        }
    }

    /// The pair's prices: the offsets from the exact average fill price, each rounded onto the
    /// tick grid toward the entry (a stop never further than its offset, a target never
    /// further either).
    fn bracket_prices(
        &mut self,
        account: &AccountId,
        contract: ContractKey,
        stop_ticks: u32,
        target_ticks: u32,
    ) -> (Price, Price) {
        let tick = self.spec(contract).map_or(1, |s| s.tick()) as i128;
        let lots = self.accounts[account]
            .positions
            .get(&contract)
            .map(|p| p.lots.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let n: i128 = lots
            .iter()
            .map(|l| l.qty.abs() as i128)
            .sum::<i128>()
            .max(1);
        let sum: i128 = lots
            .iter()
            .map(|l| l.price as i128 * l.qty.abs() as i128)
            .sum();
        let stop_off = stop_ticks as i128 * tick * n;
        let target_off = target_ticks as i128 * tick * n;
        let long = lots.first().is_none_or(|l| l.qty > 0);
        if long {
            (
                Price(ceil_tick(sum - stop_off, n, tick)),
                Price(floor_tick(sum + target_off, n, tick)),
            )
        } else {
            (
                Price(floor_tick(sum + stop_off, n, tick)),
                Price(ceil_tick(sum - target_off, n, tick)),
            )
        }
    }

    /// A command the bracket issues: run as `Source::Rule` with an internal id, its result
    /// published like any command's (the broker's events are applied by the caller's poll).
    fn bracket_command(
        &mut self,
        now: UtcMillis,
        command: Command,
    ) -> Result<Vec<OrderId>, Reject> {
        let env = self.bracket_envelope(command);
        let result = self.run(now, &env);
        if let Err(r) = &result {
            tracing::warn!(target: "lines::trading", "position bracket: {}", r.message);
        }
        self.events.push(Event::Command {
            id: env.id,
            source: env.source,
            at: now,
            result: result.clone(),
        });
        result
    }

    /// Resize a leg at the broker, with no checks but the broker's own (reduce-only).
    fn bracket_resize(&mut self, now: UtcMillis, id: OrderId, qty: Qty) {
        let env = self.bracket_envelope(Command::ModifyOrder {
            account: self.orders[&id].account.clone(),
            order: id,
            qty: Some(qty),
            price: None,
        });
        self.send(
            &env,
            now,
            BrokerRequest::Modify {
                id,
                qty: Some(qty),
                price: None,
            },
        );
        self.events.push(Event::Command {
            id: env.id,
            source: env.source,
            at: now,
            result: Ok(vec![id]),
        });
    }

    fn bracket_envelope(&mut self, command: Command) -> Envelope {
        let id = CommandId(self.next_internal);
        self.next_internal += 1;
        Envelope {
            id,
            source: Source::Rule,
            issued: std::time::Instant::now(),
            command,
            pane: None,
        }
    }
}
