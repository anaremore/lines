//! How a Flatten closes without giving up protection when the close can't happen.
//!
//! A broker that honors reduce-only (`BrokerCaps::oco`) gets the market order at once, and the
//! position's exits stay until the position is flat (`cancel_stranded_exits`): they can't reverse
//! it there, and if the market order is refused or finds nothing to fill on, they still protect
//! it. A broker that doesn't honor it could fill both, so its orders are cancelled first and the
//! market order waits for those answers (at most [`super::FLATTEN_ANSWER_MS`], for a venue that
//! never answers). That protection is gone if the market order then fails: `docs/known-gaps.md`.

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

/// An order protecting a position: it can only close it, so it is safe to leave working.
fn protects(o: &Order) -> bool {
    o.reduce_only || matches!(o.role, Role::TakeProfit | Role::StopLoss)
}

impl TradingCore {
    /// Which orders a Flatten of these contracts leaves working on a broker honoring reduce-only.
    pub(super) fn flatten_keeps(&self, o: &Order, contracts: &[ContractKey]) -> bool {
        self.broker.capabilities().oco && protects(o) && contracts.contains(&o.contract)
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
            return Some(self.flatten_send(env, now, account, contract, qty));
        }
        let waiting = self
            .pending_flattens
            .iter()
            .any(|p| &p.account == account && p.contract == contract);
        if !waiting {
            self.pending_flattens.push(PendingFlatten {
                account: account.clone(),
                contract,
                env: env.clone(),
                since: now,
            });
        }
        None
    }

    /// Something other than a Flatten's own order still works on the contract.
    fn flatten_waits(&self, account: &AccountId, contract: ContractKey) -> bool {
        self.orders.values().any(|o| {
            o.is_open()
                && &o.account == account
                && o.contract == contract
                && o.role != Role::Flatten
        })
    }

    /// A Flatten waiting for its cancels goes once they're answered, or the venue has had long
    /// enough. It closes what the position is then: an exit may have filled meanwhile.
    pub(super) fn flatten_pending(&mut self, now: UtcMillis) {
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
            let p = self.pending_flattens.remove(i);
            if qty != 0 {
                self.flatten_send(&p.env, now, &account, contract, qty);
            }
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
