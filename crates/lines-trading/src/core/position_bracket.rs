//! Position brackets: one stop and one target for a whole manual position on an account and
//! contract ([`BracketMode::Position`]), instead of a bracket per entry.
//!
//! - The pair is placed when the position opens, at the mode's offsets from the average fill
//!   price (rounded onto the tick grid toward the entry), through the same exit path as a
//!   person's chart exit (`PlaceExit`: reduce-only, one-cancels-other).
//! - It follows the position: an add or a reduce resizes both legs (their prices are kept, so a
//!   person's move stays), flat cancels them, a flip replaces them. Resizes go to the broker
//!   directly, so guardrail lockouts and modify limits never hold back protection, and never
//!   above the position (reduce-only).
//! - Entries carry no exits of their own in this mode (a bracket order's exits are dropped), and
//!   a second exit is refused while the pair stands.
//! - Strategies keep their own exits: the mode is refused, and does nothing, while a strategy
//!   instance trades the account and contract.

use lines_core::{ContractKey, Price, UtcMillis};

use super::TradingCore;
use crate::broker::BrokerRequest;
use crate::command::{BracketMode, Command, Envelope, OrderSpec, Reject, RejectCode, Source};
use crate::event::Event;
use crate::order::{Order, Role};
use crate::types::{AccountId, CommandId, OrderId, OrderKind, Side};

/// One account and contract in position mode.
#[derive(Clone, Debug)]
pub(super) struct PositionBracket {
    stop_ticks: u32,
    target_ticks: u32,
    stop: Option<OrderId>,
    target: Option<OrderId>,
    /// The position side (`Buy`: long) this episode's pair was placed for; `None` until it is.
    placed_for: Option<Side>,
    /// The position size (unsigned) the legs were last sized for.
    sized_for: i64,
}

fn is_stop(o: &Order) -> bool {
    matches!(o.kind, OrderKind::Stop { .. } | OrderKind::StopLimit { .. })
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
    /// cancel (0 when there are none, or when they already are one stop and one target, which
    /// are kept). What the confirmation names.
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

    /// Exits that already are a position bracket: at most one stop and one target, both
    /// reduce-only exits on the side that closes the open position.
    fn adoptable(&self, account: &AccountId, contract: ContractKey, exits: &[OrderId]) -> bool {
        let pos = self.position(account, contract);
        if pos == 0 || exits.is_empty() {
            return exits.is_empty();
        }
        let closing = if pos > 0 { Side::Sell } else { Side::Buy };
        let os: Vec<&Order> = exits.iter().filter_map(|i| self.orders.get(i)).collect();
        os.iter()
            .all(|o| o.reduce_only && o.role == Role::Single && o.side == closing)
            && os.iter().filter(|o| is_stop(o)).count() <= 1
            && os.iter().filter(|o| !is_stop(o)).count() <= 1
    }

    /// Whether a strategy instance trades the account and contract (its exits are its own).
    fn strategy_trades(&self, account: &AccountId, contract: ContractKey) -> bool {
        self.strategies
            .values()
            .any(|i| &i.cfg.account == account && i.cfg.contract == contract)
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
            stop: None,
            target: None,
            placed_for: None,
            sized_for: 0,
        };
        let mut cancelled = Vec::new();
        if self.adoptable(account, contract, &exits) {
            // One stop and one target already (as after a restart): they are the pair, resized
            // to the position on the next sync.
            for id in &exits {
                let o = &self.orders[id];
                if is_stop(o) {
                    b.stop = Some(o.id);
                } else {
                    b.target = Some(o.id);
                }
            }
            if !exits.is_empty() {
                let pos = self.position(account, contract);
                b.placed_for = Some(if pos > 0 { Side::Buy } else { Side::Sell });
            }
        } else if !replace {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "switching to one bracket for the position cancels {} working exit order{}; confirm to replace them",
                    exits.len(),
                    if exits.len() == 1 { "" } else { "s" }
                ),
            ));
        } else {
            for id in exits {
                if self.cancelling.insert(id) {
                    self.send(env, now, BrokerRequest::Cancel { id });
                }
                cancelled.push(id);
            }
        }
        self.position_brackets.insert(key, b);
        Ok(cancelled)
    }

    /// Position mode on a person's command: a bracket order's exits are dropped (the position's
    /// pair covers it), and a second exit is refused while the pair stands. `Err` refuses the
    /// command; `Ok(Some)` replaces it.
    pub(super) fn bracket_rewrite(&self, env: &Envelope) -> Result<Option<Command>, Reject> {
        if matches!(env.source, Source::Strategy(_) | Source::Rule) {
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
                match self.position_brackets.get(&(s.account.clone(), s.contract)) {
                    Some(b)
                        if b.placed_for.is_some()
                            && !self.strategy_trades(&s.account, s.contract) =>
                    {
                        Err(Reject::new(
                            RejectCode::Invalid,
                            "this position has one bracket: move its stop or target instead",
                        ))
                    }
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// Keep every position bracket in step with its position (after each command and tick,
    /// once the broker's events are applied).
    pub(super) fn position_brackets_sync(&mut self, now: UtcMillis) {
        let keys: Vec<(AccountId, ContractKey)> = self.position_brackets.keys().cloned().collect();
        for (account, contract) in keys {
            if !self.accounts.contains_key(&account) || self.strategy_trades(&account, contract) {
                continue;
            }
            self.position_bracket_sync(now, &account, contract);
        }
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
        // Legs that are gone (filled, cancelled, rejected) or on their way out.
        let live = |id: Option<OrderId>, core: &TradingCore| {
            id.filter(|i| !core.cancelling.contains(i))
                .filter(|i| core.orders.get(i).is_some_and(Order::is_open))
        };
        b.stop = live(b.stop, self);
        b.target = live(b.target, self);
        let pos = self.position(account, contract);
        let side = if pos > 0 { Side::Buy } else { Side::Sell };
        if pos == 0 || b.placed_for.is_some_and(|s| s != side) {
            // Flat: the pair goes. Flipped: it goes, and the new side gets its own pair.
            for id in [b.stop.take(), b.target.take()].into_iter().flatten() {
                let _ = self.bracket_command(
                    now,
                    Command::CancelOrder {
                        account: account.clone(),
                        order: id,
                    },
                );
            }
            b.placed_for = None;
            b.sized_for = 0;
            if pos == 0 {
                self.position_brackets.insert(key, b);
                return;
            }
        }
        let size = pos.abs();
        if b.placed_for.is_none() {
            // Wait for any exit still working here (one being replaced or cancelled) to go, so
            // exits never add up to more than the position.
            if !self.exits_on(account, contract).is_empty() {
                self.position_brackets.insert(key, b);
                return;
            }
            b.placed_for = Some(side);
            b.sized_for = size;
            let (stop, target) = self.bracket_prices(account, contract, &b);
            let closing = side.opposite();
            for (kind, leg) in [
                (OrderKind::Stop { trigger: stop }, true),
                (OrderKind::Limit { price: target }, false),
            ] {
                let placed = self.bracket_command(
                    now,
                    Command::PlaceExit(OrderSpec {
                        account: account.clone(),
                        contract,
                        side: closing,
                        qty: lines_core::Qty(size),
                        kind,
                    }),
                );
                let id = placed.ok().and_then(|ids| ids.first().copied());
                if leg {
                    b.stop = id;
                } else {
                    b.target = id;
                }
            }
            self.position_brackets.insert(key, b);
            return;
        }
        if size != b.sized_for {
            // An add or a reduce: both legs to the position, prices kept. Straight to the
            // broker (no guardrail, rate or growth checks: this is protection, and it never
            // exceeds the position).
            for id in [b.stop, b.target].into_iter().flatten() {
                let o = &self.orders[&id];
                let want = o.filled.0 + size;
                if o.qty.0 != want {
                    self.bracket_resize(now, id, lines_core::Qty(want));
                }
            }
            b.sized_for = size;
        }
        self.position_brackets.insert(key, b);
    }

    /// The pair's prices: the offsets from the exact average fill price, each rounded onto the
    /// tick grid toward the entry (a stop never further than its offset, a target never
    /// further either).
    fn bracket_prices(
        &mut self,
        account: &AccountId,
        contract: ContractKey,
        b: &PositionBracket,
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
        let stop_off = b.stop_ticks as i128 * tick * n;
        let target_off = b.target_ticks as i128 * tick * n;
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
    fn bracket_resize(&mut self, now: UtcMillis, id: OrderId, qty: lines_core::Qty) {
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
