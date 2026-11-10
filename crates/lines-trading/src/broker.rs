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
    /// Honors `Order::oco` on `Place` (one fill cancels or shrinks the other) and
    /// `Order::reduce_only` (never closes more than the position; cancelled when it is flat).
    /// A Flatten's market order (`Role::Flatten`) must be able to close the position whatever
    /// reduce-only exits are working: a broker budgeting them oldest-first would refuse it.
    /// Chart exits (`Command::PlaceExit`) are refused without it.
    pub oco: bool,
    pub fractional_qty: bool,
    /// The broker reports executions of orders Lines didn't place as
    /// [`BrokerEvent::ExternalFill`] (a venue observed, e.g. manual trading there). The core
    /// then never reconciles fills for orders it adopted from a snapshot: those executions
    /// already reached the fill stream once.
    ///
    /// The contract: an account is listed (`AccountSource`) only once its positions are known,
    /// and from then on every execution on it is reported once, including those made while
    /// disconnected (after the reconnect); none from before it was listed. The app's journal
    /// follows such a broker's accounts from these fills alone (after seeding what was open
    /// when each account joined), so a missed execution would be missing there.
    #[serde(default)]
    pub external_fills: bool,
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
    /// The broker refused a request. For an order it hasn't answered yet, it ends the order. For
    /// an acked, working, partly filled or unresolved order with a cancel sent, it is the cancel
    /// that was refused: the order stays as it is (`core/cancels.rs`). Otherwise it ends an
    /// acked or unresolved order; on a working one (a refused modify) it is only logged. A refused modify arriving while a cancel is on its way reads as a
    /// refused cancel: a person's next command sends again.
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
    /// An execution on an order Lines didn't place (traded at the venue itself, e.g. by hand in
    /// its own platform). Recorded as a fill (events, journal) with no order; the account's
    /// positions and balance stay the broker's snapshot's ([`Broker::snapshot`], `rebuild`),
    /// which already includes it, so nothing is counted twice. Repeats (same `trade_id`) are
    /// dropped like any fill.
    ExternalFill {
        account: AccountId,
        #[serde(with = "contract_serde")]
        contract: ContractKey,
        side: crate::types::Side,
        #[serde(with = "price_serde")]
        price: Price,
        #[serde(with = "qty_serde")]
        qty: Qty,
        fee: Money,
        #[serde(with = "time_serde")]
        at: UtcMillis,
        trade_id: String,
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
    /// `Rule` (prop-rule liquidation), `Api` and strategies are automated; everything else is
    /// a person.
    pub fn of(source: Source) -> Origin {
        match source {
            Source::Rule | Source::Api | Source::Strategy(_) => Origin::Automated,
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
    /// A simulation in this process (the paper broker): no order reaches a venue. Strategy
    /// orders go only to simulated brokers until Live automation is enabled (decision 0025).
    /// Live brokers keep the default, `false`; so does a venue's own Sim account.
    fn simulated(&self) -> bool {
        false
    }
    /// Settle an expired option position at `price` (its intrinsic value, per contract unit):
    /// `order` closes it at once, without a fee, cancelling what else works on the contract.
    /// Only simulated brokers are asked (a venue settles its own); the default refuses.
    fn settle(&mut self, now: UtcMillis, order: Order, price: Price) {
        let _ = (now, order, price);
    }
    /// A replay jumped back (the replay bar): the clock went back. Requests waiting on the old,
    /// later clock (a cancel sent just before the jump) take effect at once, before any of the
    /// replayed data. Only simulated brokers wait on the market clock; the default does nothing.
    fn rewound(&mut self) {}
    /// Reset a simulated account (`Command::ResetPaper`): `balance`, no positions. Asked only
    /// once the account has nothing open. Only simulated brokers can; the default refuses.
    fn reset_account(&mut self, now: UtcMillis, account: &AccountId, balance: Money) -> bool {
        let _ = (now, account, balance);
        false
    }
    /// State to save locally (paper: its accounts and open orders; live brokers hold their
    /// own and return `None`).
    fn saved_state(&self) -> Option<serde_json::Value> {
        None
    }
}
