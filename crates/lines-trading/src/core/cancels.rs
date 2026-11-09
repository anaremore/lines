//! Cancels on their way: a cancel the broker lost or refused is never the end of it.
//!
//! - Every cancel sent is remembered until its order finishes. One unanswered for
//!   [`CANCEL_RESEND_MS`] goes again on its own, [`CANCEL_SENDS`] sends at most, then
//!   [`Event::CancelFailed`] says so once and the order is the person's to deal with.
//! - A person's Cancel, Cancel All or Flatten sends again once the last send is that old, with a
//!   new budget. A rule's or strategy's doesn't add to the budget.
//! - A rebuild keeps them (the venue watcher rebuilds on every revision, so clearing would
//!   stop the resend each time), but the next command sends at once: the broker's list is new.
//! - A broker refuses a cancel by sending `BrokerEvent::Rejected` for an order that is working
//!   or partly filled: the order is live and stays as it is (it can't become Rejected),
//!   [`Event::CancelFailed`] says why, nothing resends on its own and a rule's cancel adds none
//!   (a sweep would ask again forever), and a person's next command goes at once.
//! - While the broker is disconnected nothing is sent and no send is spent.
//! - An order is on its way out only while a cancel may still work: refused or out of sends, it
//!   counts as live again ([`TradingCore::cancel_pending`]).

use std::time::Instant;

use lines_core::UtcMillis;

use super::TradingCore;
use crate::broker::{BrokerRequest, Origin, PaneContext};
use crate::command::{Command, Envelope, Source};
use crate::event::Event;
use crate::order::OrderState;
use crate::types::{CommandId, OrderId};

/// How long a cancel waits for an answer before it goes again.
pub(super) const CANCEL_RESEND_MS: i64 = 2_000;
/// Sends of one cancel before it is reported instead.
pub(super) const CANCEL_SENDS: u32 = 3;

/// A cancel sent and not answered yet.
#[derive(Clone, Debug)]
pub(super) struct SentCancel {
    at: UtcMillis,
    sends: u32,
    /// A rebuild came since: the next command sends at once.
    rebuilt: bool,
    /// Out of sends, and that was said.
    said: bool,
    /// The broker refused it.
    refused: bool,
    /// Who asked, so a resend reaches the broker as the first send did.
    source: Source,
    pane: Option<PaneContext>,
}

impl SentCancel {
    fn due(&self, now: UtcMillis) -> bool {
        now.0.saturating_sub(self.at.0) >= CANCEL_RESEND_MS
    }

    /// Whether anyone but a person may send it again now.
    fn may_send_again(&self, now: UtcMillis) -> bool {
        !self.refused && (self.rebuilt || self.due(now)) && self.sends < CANCEL_SENDS
    }
}

impl TradingCore {
    /// Ask the broker to cancel an order, unless a cancel for it is on its way.
    pub(super) fn send_cancel(&mut self, env: &Envelope, now: UtcMillis, id: OrderId) {
        let person = Origin::of(env.source) == Origin::Manual;
        let sends = match self.cancelling.get(&id) {
            None => 1,
            Some(c) if person && (c.refused || c.rebuilt || c.due(now)) => 1,
            // Refused or out of sends: only a person's command starts again, never a rebuild.
            Some(c) if !person && c.may_send_again(now) => c.sends + 1,
            Some(_) => return,
        };
        self.cancelling.insert(
            id,
            SentCancel {
                at: now,
                sends,
                rebuilt: false,
                said: false,
                refused: false,
                source: env.source,
                pane: env.pane,
            },
        );
        self.send(env, now, BrokerRequest::Cancel { id });
    }

    /// Whether a rule or sweep should leave this order alone for now: a cancel is on its way,
    /// or it was refused or ran out of sends (only a person's command goes then).
    pub(super) fn cancel_on_its_way(&self, now: UtcMillis, id: OrderId) -> bool {
        self.cancelling
            .get(&id)
            .is_some_and(|c| !c.may_send_again(now))
    }

    /// Whether this order is on its way out: a cancel was sent and may still work. Refused or
    /// out of sends, the order is as live as any other.
    pub(super) fn cancel_pending(&self, id: OrderId) -> bool {
        self.cancelling
            .get(&id)
            .is_some_and(|c| !c.refused && !c.said)
    }

    /// Cancels unanswered for too long: sent again, or reported once they are out of sends.
    pub(super) fn cancels_resend(&mut self, now: UtcMillis) {
        self.cancels_forget_finished();
        if !self.broker.connected() {
            return;
        }
        let due: Vec<OrderId> = self
            .cancelling
            .iter()
            .filter(|(_, c)| !c.said && !c.refused && c.due(now))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Some(account) = self.orders.get(&id).map(|o| o.account.clone()) else {
                continue;
            };
            let Some(c) = self.cancelling.get_mut(&id) else {
                continue;
            };
            if c.sends >= CANCEL_SENDS {
                c.said = true;
                let reason = format!("no answer to {} cancels", c.sends);
                self.cancel_failed(id, reason, now);
                continue;
            }
            c.sends += 1;
            c.at = now;
            let (source, pane) = (c.source, c.pane);
            let command = CommandId(self.next_internal);
            self.next_internal += 1;
            let env = Envelope {
                id: command,
                source,
                issued: Instant::now(),
                command: Command::CancelOrder { account, order: id },
                pane,
            };
            self.send(&env, now, BrokerRequest::Cancel { id });
            self.events.push(Event::Command {
                id: command,
                source,
                at: now,
                result: Ok(vec![id]),
            });
        }
    }

    /// When a cancel next goes again or is reported.
    pub(super) fn cancels_wake(&self) -> Option<UtcMillis> {
        let next = self
            .cancelling
            .values()
            .filter(|c| !c.said && !c.refused)
            .map(|c| UtcMillis(c.at.0.saturating_add(CANCEL_RESEND_MS)))
            .min()?;
        // Disconnected, what is due stays due: look again later, not in a loop.
        Some(if self.broker.connected() {
            next
        } else {
            UtcMillis(self.seen_now.0.saturating_add(CANCEL_RESEND_MS))
        })
    }

    /// Finished (a rebuild's don't pass through `finish`): nothing left to cancel. An unresolved
    /// order isn't finished: it may still be at the broker.
    fn cancels_forget_finished(&mut self) {
        let orders = &self.orders;
        self.cancelling
            .retain(|id, _| orders.get(id).is_some_and(|o| !o.state.is_terminal()));
    }

    /// After a rebuild: what the broker no longer lists needs no cancel; the next command for
    /// what it does list sends at once.
    pub(super) fn cancels_rebuilt(&mut self) {
        self.cancels_forget_finished();
        for c in self.cancelling.values_mut() {
            c.rebuilt = true;
        }
    }

    /// The broker's `Rejected` for a live order with a cancel on its way: the cancel was
    /// refused, not the order. Returns whether that is what this was.
    pub(super) fn cancel_refused(&mut self, id: OrderId, reason: &str, at: UtcMillis) -> bool {
        let live = self
            .orders
            .get(&id)
            .is_some_and(|o| matches!(o.state, OrderState::Working | OrderState::PartiallyFilled));
        if !live {
            return false;
        }
        let Some(c) = self.cancelling.get_mut(&id).filter(|c| !c.refused) else {
            return false;
        };
        c.refused = true;
        self.cancel_failed(id, format!("cancel refused: {reason}"), at);
        true
    }

    fn cancel_failed(&mut self, id: OrderId, reason: String, at: UtcMillis) {
        let Some(o) = self.orders.get(&id) else {
            return;
        };
        let reason = if o.is_open() {
            format!("still working: {reason}")
        } else {
            format!("outcome unknown: {reason}")
        };
        tracing::warn!(target: "lines::trading", order = id.0, "cancel failed: {reason}");
        self.events.push(Event::CancelFailed {
            account: o.account.clone(),
            contract: o.contract,
            order: id,
            reason,
            at,
        });
    }
}
