//! Orders and the one lifecycle every broker follows (decision 0004):
//!
//! ```text
//! submitted ─► acked ─► working ─► partially filled ─► filled
//!     │          │         │              │
//!     └► rejected└► rejected / cancelled ─┴──────────► cancelled
//! ```
//! - **Submitted**: the core sent it to the broker; never shown as working (0006).
//! - **Acked**: the broker accepted it. A bracket's exits stay acked (held server-side) until
//!   their entry fills; a market order may fill straight from acked.
//! - **Working**: live in the market (resting limit, armed stop).
//! - Terminal: filled, cancelled, rejected (with the reason).

use lines_core::{ContractKey, Price, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::types::{AccountId, OrderId, OrderKind, Side, contract_serde, qty_serde, time_serde};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OrderState {
    Submitted,
    Acked,
    Working,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected {
        reason: String,
    },
    /// Sent, but no outcome is known: the app stopped or reconnected before the broker
    /// answered, and the broker's snapshot doesn't list it. Never assumed working (or
    /// cancelled): shown as unresolved until the broker reports it, or the trader checks.
    Unresolved,
}

impl OrderState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected { .. }
        )
    }

    /// Whether the lifecycle allows `self` → `next`.
    pub fn can_become(&self, next: &OrderState) -> bool {
        use OrderState::*;
        matches!(
            (self, next),
            (Submitted, Acked | Rejected { .. })
                | (
                    Acked,
                    Working | PartiallyFilled | Filled | Cancelled | Rejected { .. }
                )
                | (Working, PartiallyFilled | Filled | Cancelled)
                | (PartiallyFilled, PartiallyFilled | Filled | Cancelled)
                | (Submitted, Unresolved)
                | (
                    Unresolved,
                    Acked | Working | PartiallyFilled | Filled | Cancelled | Rejected { .. }
                )
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            OrderState::Submitted => "submitted",
            OrderState::Acked => "acked",
            OrderState::Working => "working",
            OrderState::PartiallyFilled => "partially filled",
            OrderState::Filled => "filled",
            OrderState::Cancelled => "cancelled",
            OrderState::Rejected { .. } => "rejected",
            OrderState::Unresolved => "unresolved",
        }
    }
}

/// An order's part in a bracket (server-side, decision 0004/0018).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Single,
    Entry,
    TakeProfit,
    StopLoss,
    /// Sent by Flatten.
    Flatten,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub side: Side,
    #[serde(with = "qty_serde")]
    pub qty: Qty,
    pub kind: OrderKind,
    pub state: OrderState,
    #[serde(with = "qty_serde")]
    pub filled: Qty,
    /// Σ fill price × fill qty (exact), for the average fill price.
    pub fill_value: i128,
    pub role: Role,
    /// The bracket's entry (for exits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<OrderId>,
    /// The other exit of a one-cancels-other pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oco: Option<OrderId>,
    /// A stop that has triggered (now behaves as its market/limit form).
    #[serde(default)]
    pub triggered: bool,
    #[serde(with = "time_serde")]
    pub created: UtcMillis,
    #[serde(with = "time_serde")]
    pub updated: UtcMillis,
    /// Only ever closes the position (Flatten's orders; any order placed while the account is
    /// reducing only). The broker shrinks it as the position shrinks and cancels it when the
    /// position is flat; it never opens, adds or flips.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduce_only: bool,
    /// Filled beyond the order's quantity (an overfill the core recorded; normally 0).
    #[serde(default, skip_serializing_if = "is_zero", with = "qty_serde")]
    pub overfill: Qty,
}

fn is_zero(q: &Qty) -> bool {
    q.0 == 0
}

impl Order {
    pub fn remaining(&self) -> Qty {
        Qty(self.qty.0 - self.filled.0)
    }

    /// Average fill price in price units (display; exact value is `fill_value / filled`).
    pub fn avg_fill(&self) -> Option<f64> {
        (self.filled.0 > 0).then(|| self.fill_value as f64 / self.filled.0 as f64)
    }

    /// At the broker or on its way (submitted, acked, working, partially filled). Not
    /// terminal and not unresolved.
    pub fn is_open(&self) -> bool {
        !self.state.is_terminal() && self.state != OrderState::Unresolved
    }

    /// Record a fill and move to partially filled / filled.
    pub fn apply_fill(&mut self, price: Price, qty: Qty, at: UtcMillis) {
        self.filled = Qty(self.filled.0 + qty.0);
        self.fill_value += price.0 as i128 * qty.0 as i128;
        self.overfill = Qty((self.filled.0 - self.qty.0).max(0));
        self.state = if self.filled.0 >= self.qty.0 {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };
        self.updated = at;
    }
}

#[cfg(test)]
mod tests {
    use super::OrderState::*;

    #[test]
    fn lifecycle_transitions() {
        let rej = || Rejected { reason: "x".into() };
        // Allowed.
        for (a, b) in [
            (Submitted, Acked),
            (Submitted, rej()),
            (Acked, Working),
            (Acked, Filled),
            (Acked, PartiallyFilled),
            (Acked, Cancelled),
            (Acked, rej()),
            (Working, PartiallyFilled),
            (Working, Filled),
            (Working, Cancelled),
            (PartiallyFilled, PartiallyFilled),
            (PartiallyFilled, Filled),
            (PartiallyFilled, Cancelled),
        ] {
            assert!(a.can_become(&b), "{a:?} -> {b:?}");
        }
        // Not allowed: skipping the ack, leaving a terminal state, un-filling.
        for (a, b) in [
            (Submitted, Working),
            (Submitted, Filled),
            (Working, Acked),
            (Working, rej()),
            (Filled, Cancelled),
            (Cancelled, Working),
            (rej(), Acked),
            (PartiallyFilled, Working),
        ] {
            assert!(!a.can_become(&b), "{a:?} -> {b:?}");
        }
        assert!(Filled.is_terminal() && rej().is_terminal() && !Working.is_terminal());
    }
}
