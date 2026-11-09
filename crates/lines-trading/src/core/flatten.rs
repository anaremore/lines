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
use crate::order::{Order, Role};
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

/// A plain stop closing the position (from a DOM): protection, not a reduce-only one, so it would
/// open a position if left once this one is gone.
fn plain_stop_closes(o: &Order, position: i64) -> bool {
    !o.reduce_only
        && matches!(o.kind, OrderKind::Stop { .. } | OrderKind::StopLimit { .. })
        && position.signum() == -o.side.sign()
        && o.remaining().0 <= position.abs()
}

/// An order protecting a position: it can only close it, so it is safe to leave working. A
/// Flatten's own order isn't one: when it's left working at all it's because it's still `closing`.
fn protects(o: &Order) -> bool {
    o.role != Role::Flatten
        && (o.reduce_only || matches!(o.role, Role::TakeProfit | Role::StopLoss))
}

impl TradingCore {
    /// Which orders a Flatten of these positions leaves working on a broker honoring reduce-only:
    /// what protects them, until they're flat.
    pub(super) fn flatten_keeps(&self, o: &Order, positions: &[(ContractKey, i64)]) -> bool {
        self.broker.capabilities().oco
            && positions
                .iter()
                .find(|(k, _)| *k == o.contract)
                .is_some_and(|(_, q)| protects(o) || plain_stop_closes(o, *q))
    }

    /// Plain stops a Flatten kept: nothing else ends them when the position is flat.
    pub(super) fn flatten_note_kept(
        &mut self,
        account: &AccountId,
        positions: &[(ContractKey, i64)],
    ) {
        if !self.broker.capabilities().oco {
            return;
        }
        for o in self
            .orders
            .values()
            .filter(|o| o.is_open() && &o.account == account)
        {
            let keep = positions
                .iter()
                .find(|(k, _)| *k == o.contract)
                .is_some_and(|(_, q)| plain_stop_closes(o, *q));
            if keep && !self.flatten_kept.contains(&o.id) {
                self.flatten_kept.push(o.id);
            }
        }
    }

    /// A kept plain stop ends once its position is flat or on the stop's side: it would open one.
    fn flatten_clean_up(&mut self, now: UtcMillis) {
        if self.flatten_kept.is_empty() {
            return;
        }
        let kept = std::mem::take(&mut self.flatten_kept);
        let mut ending = Vec::new();
        for id in kept {
            let Some(o) = self.orders.get(&id).filter(|o| o.is_open()) else {
                continue;
            };
            let position = self.position(&o.account, o.contract);
            if position == 0 || position.signum() == o.side.sign() {
                ending.push((o.account.clone(), id));
            } else {
                self.flatten_kept.push(id);
            }
        }
        for (account, order) in ending {
            self.submit_internal(
                now,
                crate::command::Source::Rule,
                crate::command::Command::CancelOrder { account, order },
            );
        }
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
        let spec = OrderSpec {
            account: account.clone(),
            contract,
            side: if qty > 0 { Side::Sell } else { Side::Buy },
            qty: Qty(qty.abs()),
            kind: OrderKind::Market,
        };
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

    /// A cancel this Flatten sent is still unanswered on the contract.
    fn flatten_waits(&self, account: &AccountId, contract: ContractKey) -> bool {
        self.orders.values().any(|o| {
            o.is_open()
                && &o.account == account
                && o.contract == contract
                && o.role != Role::Flatten
                && self.cancelling.contains(&o.id)
        })
    }

    /// A Flatten waiting for its cancels goes once they're answered, or the venue has had long
    /// enough. It closes what the position is then: an exit may have filled meanwhile.
    pub(super) fn flatten_pending(&mut self, now: UtcMillis) {
        self.flatten_clean_up(now);
        let mut i = 0;
        while i < self.pending_flattens.len() {
            let p = &self.pending_flattens[i];
            let (account, contract, since) = (p.account.clone(), p.contract, p.since);
            let qty = self.position(&account, contract);
            let late = now.0.saturating_sub(since.0) >= FLATTEN_ANSWER_MS;
            if qty != 0 && self.flatten_waits(&account, contract) && !late {
                i += 1;
                continue;
            }
            let mut p = self.pending_flattens.remove(i);
            if qty != 0 {
                // Click to sent counts from here: the wait is the broker's answers, not the core.
                p.env.issued = std::time::Instant::now();
                // Anything placed while it waited goes too: it could fill as well.
                self.cancel_scoped(&p.env, now, &account, Some(contract), None);
                self.flatten_send(&p.env, now, &account, contract, qty);
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

    /// When a waiting Flatten stops waiting for an answer.
    pub(super) fn flatten_wake(&self) -> Option<UtcMillis> {
        self.pending_flattens
            .iter()
            .map(|p| UtcMillis(p.since.0 + FLATTEN_ANSWER_MS))
            .min()
    }
}
