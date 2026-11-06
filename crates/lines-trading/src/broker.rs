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

use crate::command::Source;
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
    /// An execution. `trade_id` is the broker's stable id for it (the core drops a repeat:
    /// brokers re-send executions after a reconnect).
    Fill {
        id: OrderId,
        #[serde(default)]
        trade_id: String,
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

/// Who issued an order: a person (DOM, chart, hotkey, palette) or automation (rules, API).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Manual,
    Automated,
}

impl Origin {
    /// `Rule` (prop-rule liquidation) and `Api` are automated; everything else is a person.
    pub fn of(source: Source) -> Origin {
        match source {
            Source::Rule | Source::Api => Origin::Automated,
            Source::Dom | Source::Chart | Source::Hotkey | Source::Palette | Source::Test => {
                Origin::Manual
            }
        }
    }
}

/// How the issuing pane chose its contract (decision 0013).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractSelection {
    /// The pane follows a product to its front month.
    FollowsProduct,
    /// The pane is pinned to one contract.
    Pinned,
}

/// The pane (DOM or chart) an order was issued from, as it was at that moment. Live brokers
/// check it against their venue (decision 0024); the paper broker ignores it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneContext {
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub selection: ContractSelection,
    /// Lines' front-month resolver's answer for the pane's product at issue time (`None`: no
    /// answer, e.g. a spot product or no calendar).
    #[serde(default, with = "opt_contract_serde")]
    pub front_month: Option<ContractKey>,
}

/// Per-request context the core hands a broker with every [`BrokerRequest`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestContext {
    pub source: Source,
    pub origin: Origin,
    /// `None` when the command didn't come from a pane (API, rules, a palette with no pane).
    pub pane: Option<PaneContext>,
}

impl RequestContext {
    pub fn new(source: Source, pane: Option<PaneContext>) -> RequestContext {
        RequestContext {
            source,
            origin: Origin::of(source),
            pane,
        }
    }
}

mod opt_contract_serde {
    use lines_core::ContractKey;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::types::ContractDoc;
    pub fn serialize<S: Serializer>(k: &Option<ContractKey>, s: S) -> Result<S::Ok, S::Error> {
        k.map(ContractDoc::from_key).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ContractKey>, D::Error> {
        match Option::<ContractDoc>::deserialize(d)? {
            Some(c) => c
                .to_key()
                .map(Some)
                .ok_or_else(|| serde::de::Error::custom("bad contract")),
            None => Ok(None),
        }
    }
}

pub trait Broker: Send {
    fn name(&self) -> &str;
    fn capabilities(&self) -> BrokerCaps;
    fn request(&mut self, now: UtcMillis, req: BrokerRequest);
    /// A request with its context (issuing pane, front month, manual vs automated). The core
    /// always calls this; the default ignores the context (the paper broker), so implementing
    /// only [`Broker::request`] stays enough.
    fn request_in_context(&mut self, now: UtcMillis, req: BrokerRequest, ctx: &RequestContext) {
        let _ = ctx;
        self.request(now, req);
    }
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
