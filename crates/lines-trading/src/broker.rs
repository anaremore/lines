//! The broker trait (decision 0004): paper, replay and live brokers sit behind it, and the
//! trading core can't tell them apart.
//!
//! Like provider sessions (decision 0011) a broker is I/O-free from the core's point of view:
//! the core hands it requests and market data with the current time, and polls it for events
//! (acks, fills, cancels). A live broker does its I/O behind this interface; the paper broker
//! is a deterministic simulation. Brokers **declare capabilities** (decision 0018); the UI
//! enables controls from them, never from the broker's identity.
//!
//! Brokers are the source of truth for orders, positions and balances (decision 0012):
//! [`Broker::snapshot`] is what the core rebuilds from after a (re)connect.

use lines_core::{ContractKey, Price, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::market::MarketInput;
use crate::order::Order;
use crate::position::Position;
use crate::types::{AccountId, Money, OrderId, contract_serde, price_serde, qty_serde, time_serde};

/// What a broker can do (decision 0018). A missing capability is a disabled control with a
/// reason, never a silent fallback.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerCaps {
    pub market_orders: bool,
    pub stop_orders: bool,
    pub modify: bool,
    /// Brackets held by the broker (they survive the app closing).
    pub server_brackets: bool,
    pub oco: bool,
    pub fractional_qty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerRequest {
    Place(Order),
    /// An entry and its exits (one-cancels-other), held by the broker.
    Bracket {
        entry: Order,
        exits: Vec<Order>,
    },
    Modify {
        id: OrderId,
        qty: Option<Qty>,
        price: Option<Price>,
    },
    Cancel {
        id: OrderId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BrokerEvent {
    Acked {
        id: OrderId,
        #[serde(with = "time_serde")]
        at: UtcMillis,
    },
    /// Live in the market (a resting limit, an armed stop, an activated bracket exit).
    Working {
        id: OrderId,
        #[serde(with = "time_serde")]
        at: UtcMillis,
    },
    Rejected {
        id: OrderId,
        reason: String,
        #[serde(with = "time_serde")]
        at: UtcMillis,
    },
    Fill {
        id: OrderId,
        #[serde(with = "price_serde")]
        price: Price,
        #[serde(with = "qty_serde")]
        qty: Qty,
        fee: Money,
        #[serde(with = "time_serde")]
        at: UtcMillis,
    },
    Cancelled {
        id: OrderId,
        reason: String,
        #[serde(with = "time_serde")]
        at: UtcMillis,
    },
    /// The order after a modify (quantity or price), or an exit resized by its entry's fills.
    Modified { order: Order },
}

/// One account as the broker holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerAccount {
    pub id: AccountId,
    /// Realised P&L and fees included.
    pub balance: Money,
    pub positions: Vec<BrokerPosition>,
    /// Open orders (bracket exits included).
    pub orders: Vec<Order>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerPosition {
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub position: Position,
}

/// The broker's authoritative state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerSnapshot {
    pub accounts: Vec<BrokerAccount>,
}

pub trait Broker: Send {
    fn name(&self) -> &str;
    fn capabilities(&self) -> BrokerCaps;
    fn request(&mut self, now: UtcMillis, req: BrokerRequest);
    /// Market data for a contract (paper and replay brokers simulate fills from it; live
    /// brokers ignore it).
    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput);
    /// Events due by `now`.
    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>);
    /// When the broker next has something due (paper ack latency), if anything.
    fn next_wake(&self) -> Option<UtcMillis>;
    fn snapshot(&self) -> BrokerSnapshot;
    /// Whether the broker is reachable (paper: always).
    fn connected(&self) -> bool {
        true
    }
    /// State to save locally (paper: its accounts and open orders; live brokers hold their
    /// own and return `None`).
    fn saved_state(&self) -> Option<serde_json::Value> {
        None
    }
}
