//! What the trading core tells its subscribers (UIs, journal, guardrails, tests): one stream
//! of typed events, in order (decision 0018: commands in, events out).

use lines_core::{ContractKey, Price, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::command::{Reject, Source};
use crate::guardrails::GuardState;
use crate::order::Order;
use crate::rules::{Breach, RuleStatus, TradingState};
use crate::strategy::{StrategyId, StrategyState};
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
    /// The account's guardrail state changed (roadmap 8b): a cooldown or lockout began
    /// (`reason`, `until`: its end), or ended (`Active`, both `None`).
    Guardrail {
        account: AccountId,
        state: GuardState,
        reason: Option<String>,
        #[serde(default, with = "crate::types::opt_time_serde")]
        until: Option<UtcMillis>,
    },
    /// A strategy instance was added, removed (`removed`), armed, paused or stopped (decision
    /// 0025). `reason`: why it paused or stopped, in plain words.
    Strategy {
        id: StrategyId,
        /// The account it trades.
        account: AccountId,
        state: StrategyState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        removed: bool,
    },
    /// A trigger on an underlying (decision 0029) after any change; [`crate::trigger`].
    Trigger(crate::trigger::TriggerView),
    /// A cancel didn't happen: the broker refused it, or never answered it. `reason` says which,
    /// and whether the order is still working or its outcome unknown. On a working order
    /// another Cancel, Cancel All or Flatten sends again.
    CancelFailed {
        account: AccountId,
        #[serde(with = "contract_serde")]
        contract: ContractKey,
        order: OrderId,
        reason: String,
        #[serde(with = "time_serde")]
        at: UtcMillis,
    },
    /// The cache was rebuilt from the broker (after a (re)connect or restart).
    Rebuilt {
        accounts: usize,
        open_orders: usize,
    },
    /// A fill for more than the order had left. With [`crate::core::OverfillPolicy::Record`]
    /// it was applied (the broker's position is the truth); with `Reject` it was not. Never
    /// clamped.
    Overfill {
        account: AccountId,
        order: OrderId,
        #[serde(with = "qty_serde")]
        excess: Qty,
        recorded: bool,
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
    /// The broker's stable execution id (`recon-<order>-<filled>` for fills the core learned
    /// of by reconciliation).
    #[serde(default)]
    pub trade_id: String,
    /// Synthesised by reconciliation after a reconnect: the broker had filled more than the
    /// cache knew. Positions and balances come from the broker snapshot, so it carries no P&L
    /// of its own (`realised` and `fee` are zero).
    #[serde(default)]
    pub reconciled: bool,
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
    /// When the price `unrealised` was taken at was seen (`None`: no price yet). A feed that
    /// stopped leaves it old, so a reader can say the P&L is stale rather than show it frozen.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::types::opt_time_serde"
    )]
    pub marked_at: Option<lines_core::UtcMillis>,
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
    /// The breach that restricts new orders, if any (Flatten and Cancel All still work).
    pub blocked: Option<Breach>,
    /// What the account may do: active, reducing only, or halted.
    #[serde(default)]
    pub trading: TradingState,
    /// The prop rules and guardrails in force now, and looser ones waiting for the next
    /// trade date. Optional.
    #[serde(default)]
    pub limits: Box<AccountLimits>,
}

/// An account's limits as the core holds them: in force now, and looser ones asked for that
/// take over with the next trade date.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountLimits {
    /// The account's size (its starting balance, whole dollars): prop rules are for it.
    #[serde(default)]
    pub account_size: i64,
    /// The prop rules in force now (`None`: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_in_force: Option<crate::rules::RulePreset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_pending: Option<crate::rules::PendingRules>,
    /// The trader's guardrails in force now.
    #[serde(
        default,
        skip_serializing_if = "crate::guardrails::Guardrails::is_default"
    )]
    pub guardrails_in_force: crate::guardrails::Guardrails,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardrails_pending: Option<crate::guardrails::Guardrails>,
    /// The largest order the account's risk limits allow (`RiskLimits::max_order_qty`), so the
    /// UI can say so before a click.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_order_qty: Option<i64>,
}
