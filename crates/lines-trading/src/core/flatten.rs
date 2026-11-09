//! How a Flatten closes without giving up protection when the close can't happen.
//!
//! A broker that honors reduce-only (`BrokerCaps::oco`) gets the market order at once, and the
//! position's exits stay until the position is flat (`cancel_stranded_exits`): they can't reverse
//! it there, and if the market order is refused or finds nothing to fill on, they still protect
//! it. A broker that doesn't honor it could fill both, so its orders are cancelled first and the
//! market order waits for those answers (at most [`super::FLATTEN_ANSWER_MS`], for a venue that
//! never answers). That protection is gone if the market order then fails, and past that wait
//! both could fill: `docs/known-gaps.md`. A person's Cancel All takes back a Flatten still waiting.

use lines_core::{ContractKey, Qty, UtcMillis};

use super::{FLATTEN_ANSWER_MS, TradingCore};
use crate::broker::BrokerRequest;
use crate::command::{Envelope, OrderSpec};
use crate::order::{Order, OrderState, Role};
use crate::types::{AccountId, OrderId, OrderKind, Side};

/// A market order waiting for its contract's cancels.
pub(super) struct PendingFlatten {
    account: AccountId,
    contract: ContractKey,
    env: Envelope,
    since: UtcMillis,
}

/// Whether the one asking is a person: their Cancel All can take it back, a rule's can't be.
fn manual(env: &Envelope) -> bool {
    crate::broker::Origin::of(env.source) == crate::broker::Origin::Manual
}

/// An order protecting a position: it can only close it, so it is safe to leave working. A
/// Flatten's own order isn't one: when it's left working at all it's because it's still `closing`.
fn protects(o: &Order) -> bool {
    o.role != Role::Flatten
        && (o.reduce_only || matches!(o.role, Role::TakeProfit | Role::StopLoss))
}

/// A Flatten's own order closing its position: live or unresolved, with a word in the last
/// [`FLATTEN_ANSWER_MS`]. While it is, no second market order goes, however one is asked.
pub(super) fn closes(o: &Order, now: UtcMillis) -> bool {
    o.role == Role::Flatten
        && (o.is_open() || o.state == OrderState::Unresolved)
        && now.0.saturating_sub(o.updated.0) < FLATTEN_ANSWER_MS
}

fn flatten_spec(account: &AccountId, contract: ContractKey, qty: i64) -> OrderSpec {
    OrderSpec {
        account: account.clone(),
        contract,
        side: if qty > 0 { Side::Sell } else { Side::Buy },
        qty: Qty(qty.abs()),
        kind: OrderKind::Market,
    }
}

impl TradingCore {
    /// Which orders a Flatten of these positions leaves working on a broker honoring reduce-only:
    /// what protects them, and which the broker caps, until they're flat. A plain stop isn't: it
    /// could reverse the position in the moment after the fill (decisions 0025, 0027).
    pub(super) fn flatten_keeps(&self, o: &Order, positions: &[(ContractKey, i64)]) -> bool {
        self.broker.capabilities().oco
            && protects(o)
            && positions.iter().any(|(k, _)| *k == o.contract)
    }

    /// The market order closing a position, at once.
    pub(super) fn flatten_send(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        qty: i64,
    ) -> OrderId {
        let spec = flatten_spec(account, contract, qty);
        let o = self.new_order(now, &spec, Role::Flatten, None);
        let id = o.id;
        self.send(env, now, BrokerRequest::Place(o));
        id
    }

    /// Close a position: at once, or once its cancels are answered on a broker that could fill
    /// both. `None`: it waits.
    pub(super) fn flatten_close(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        qty: i64,
    ) -> Option<OrderId> {
        if self.broker.capabilities().oco || !self.flatten_waits(account, contract) {
            // It goes now, for whoever asked: one waiting for the same contract is this one.
            self.pending_flattens
                .retain(|p| !(&p.account == account && p.contract == contract));
            return Some(self.flatten_send(env, now, account, contract, qty));
        }
        let waiting = self
            .pending_flattens
            .iter_mut()
            .find(|p| &p.account == account && p.contract == contract);
        if let Some(p) = waiting {
            // A rule asking too: a person's Cancel All can no longer take it back.
            if manual(&p.env) && !manual(env) {
                p.env = env.clone();
            }
        } else {
            self.pending_flattens.push(PendingFlatten {
                account: account.clone(),
                contract,
                env: env.clone(),
                since: now,
            });
        }
        None
    }

    /// A cancel this Flatten sent is still unanswered on the contract, even if a rebuild has left
    /// the order unresolved since: it may still fill. Not an earlier Flatten's order: past
    /// [`FLATTEN_ANSWER_MS`] that one is replaced at once (`docs/known-gaps.md`). One whose cancel
    /// got nowhere (refused, or out of sends) has no answer to wait for.
    fn flatten_waits(&self, account: &AccountId, contract: ContractKey) -> bool {
        self.orders.values().any(|o| {
            !o.state.is_terminal()
                && &o.account == account
                && o.contract == contract
                && o.role != Role::Flatten
                && self.cancel_pending(o.id)
        })
    }

    /// A Flatten waiting for its cancels goes once they're answered, or the venue has had long
    /// enough. It closes what the position is then: an exit may have filled meanwhile.
    /// With the broker disconnected the market order can't go, and what the position is when it's
    /// back isn't known until a rebuild: it ends here, as a refused order saying so.
    pub(super) fn flatten_pending(&mut self, now: UtcMillis) {
        if !self.broker.connected() {
            for p in std::mem::take(&mut self.pending_flattens) {
                let qty = self.position(&p.account, p.contract);
                if qty != 0 {
                    let spec = flatten_spec(&p.account, p.contract, qty);
                    let id = self.new_order(now, &spec, Role::Flatten, None).id;
                    let reason = "the broker disconnected before the close could go: the position is still open and its orders may be canceled".to_owned();
                    self.transition(id, OrderState::Rejected { reason }, now);
                }
            }
            return;
        }
        let mut i = 0;
        while i < self.pending_flattens.len() {
            let p = &self.pending_flattens[i];
            let (account, contract, since) = (p.account.clone(), p.contract, p.since);
            let qty = self.position(&account, contract);
            // An earlier Flatten's order spoke up meanwhile: it's closing the position, so nothing
            // goes beside it. If it ends without closing, this one still does.
            if qty != 0
                && self
                    .orders
                    .values()
                    .any(|o| closes(o, now) && o.account == account && o.contract == contract)
            {
                i += 1;
                continue;
            }
            let late = now.0.saturating_sub(since.0) >= FLATTEN_ANSWER_MS;
            if qty != 0 && self.flatten_waits(&account, contract) && !late {
                i += 1;
                continue;
            }
            let mut p = self.pending_flattens.remove(i);
            // Anything placed while it waited goes too: it could fill or open a position.
            if qty != 0 {
                self.cancel_scoped(&p.env, now, &account, Some(contract), None);
                // Click to sent counts from here: the wait is the broker's answers, not the core.
                p.env.issued = std::time::Instant::now();
                self.flatten_send(&p.env, now, &account, contract, qty);
            } else {
                // Nothing left to close: a strategy's Flatten is its Cancel All.
                self.cancel_all(&p.env, now, &account, Some(contract));
            }
        }
    }

    /// A person's Cancel All takes back a person's Flatten still waiting: it leaves the position.
    /// A rule's stays: a news window or a breach asks once.
    pub(super) fn flatten_take_back(&mut self, env: &Envelope) {
        if !manual(env) {
            return;
        }
        if let crate::command::Command::CancelAll { account, contract } = &env.command {
            self.pending_flattens.retain(|p| {
                !(manual(&p.env)
                    && &p.account == account
                    && contract.is_none_or(|c| c == p.contract))
            });
        }
    }

    /// A Flatten's market order is closing this contract, or waiting to go.
    pub(super) fn flatten_under_way(
        &self,
        account: &AccountId,
        contract: ContractKey,
        now: UtcMillis,
    ) -> bool {
        self.pending_flattens
            .iter()
            .any(|p| &p.account == account && p.contract == contract)
            || self
                .orders
                .values()
                .any(|o| closes(o, now) && &o.account == account && o.contract == contract)
    }

    /// When a waiting Flatten stops waiting: 5 s after it asked and after the last word from a
    /// Flatten order it won't go beside.
    pub(super) fn flatten_wake(&self) -> Option<UtcMillis> {
        self.pending_flattens
            .iter()
            .map(|p| {
                let spoke = self
                    .orders
                    .values()
                    .filter(|o| {
                        closes(o, self.seen_now)
                            && o.account == p.account
                            && o.contract == p.contract
                    })
                    .map(|o| o.updated.0)
                    .max()
                    .unwrap_or(p.since.0);
                UtcMillis(p.since.0.max(spoke) + FLATTEN_ANSWER_MS)
            })
            .min()
    }
}
