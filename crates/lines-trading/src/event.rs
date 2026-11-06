//! What the trading core tells its subscribers (UIs, journal, guardrails, tests): one stream
//! of typed events, in order (decision 0018: commands in, events out).

use lines_core::{ContractKey, Price, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::command::{Reject, Source};
use crate::order::Order;
use crate::rules::{Breach, RuleStatus};
use crate::types::{
    AccountId, CommandId, Money, OrderId, Side, contract_serde, price_serde, qty_serde, time_serde,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The answer to every command: the orders it created or touched, or why it was refused.
    Command {
        id: CommandId,
        source: Source,
        #[serde(with = "time_serde")]
        at: UtcMillis,
        result: Result<Vec<OrderId>, Reject>,
    },
    /// An order after any change (state, fill, modify). Never shown as working before acked.
    Order(Order),
    Fill(FillEvent),
    Position(PositionView),
    Account(AccountView),
    /// A prop rule was crossed (decision 0006: surfaced clearly).
    Breach {
        account: AccountId,
        breach: Breach,
    },
    /// The cache was rebuilt from the broker (after a (re)connect or restart).
    Rebuilt {
        accounts: usize,
        open_orders: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillEvent {
    pub account: AccountId,
    pub order: OrderId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub side: Side,
    #[serde(with = "price_serde")]
    pub price: Price,
    #[serde(with = "qty_serde")]
    pub qty: Qty,
    pub fee: Money,
    /// P&L this fill realised (before the fee).
    pub realised: Money,
    #[serde(with = "time_serde")]
    pub at: UtcMillis,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PositionView {
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    /// Signed quantity units.
    pub qty: i64,
    /// Average open price, price units (display).
    pub avg_price: Option<f64>,
    pub unrealised: Money,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountView {
    pub account: AccountId,
    /// Realised P&L and fees included.
    pub balance: Money,
    pub unrealised: Money,
    pub equity: Money,
    /// Today's P&L (trade date of the account's day calendar), fees included.
    pub day_pnl: Money,
    pub rules: Option<RuleStatus>,
    /// New orders are blocked by a rule (Flatten and Cancel All still work).
    pub blocked: Option<Breach>,
}
