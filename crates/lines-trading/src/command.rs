//! Commands: the single path into the trading core (decision 0018).
//!
//! DOM clicks, chart gestures, hotkeys, the palette, and later alerts and plugins all produce
//! a [`Command`]; the core assigns it a [`CommandId`], timestamps it, and answers with exactly
//! one [`crate::Event::Command`] result. Every command names its account; order commands name
//! their contract (decision 0013) with exact prices and quantities (decision 0002).

use std::time::Instant;

use lines_core::{ContractKey, Price, Qty};
use serde::{Deserialize, Serialize};

use crate::strategy::{StrategyConfig, StrategyId};
use crate::types::{
    AccountId, CommandId, OrderId, OrderKind, Side, contract_serde, opt_price_serde, qty_serde,
};

/// A new order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderSpec {
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub side: Side,
    #[serde(with = "qty_serde")]
    pub qty: Qty,
    pub kind: OrderKind,
}

/// An entry with exits that the broker holds (server-side): the exits activate when the entry
/// fills and cancel each other (OCO).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BracketSpec {
    pub entry: OrderSpec,
    /// Limit exit.
    #[serde(default, with = "opt_price_serde")]
    pub take_profit: Option<Price>,
    /// Stop exit.
    #[serde(default, with = "opt_price_serde")]
    pub stop_loss: Option<Price>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    PlaceOrder(OrderSpec),
    /// An exit for the open position (a stop or target added on the chart): reduce-only at the
    /// broker, so it shrinks with the position and is cancelled when it is flat; refused unless
    /// it is on the closing side and no larger than the position.
    PlaceExit(OrderSpec),
    PlaceBracket(BracketSpec),
    /// Change a working order's quantity and/or price (limit price, or stop trigger).
    ModifyOrder {
        account: AccountId,
        order: OrderId,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_qty")]
        qty: Option<Qty>,
        #[serde(default, with = "opt_price_serde")]
        price: Option<Price>,
    },
    CancelOrder {
        account: AccountId,
        order: OrderId,
    },
    /// Cancel every open order (of one contract, or all). Always allowed (0006).
    CancelAll {
        account: AccountId,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_contract"
        )]
        contract: Option<ContractKey>,
    },
    /// Cancel open orders and close positions at market (of one contract, or all). Always
    /// allowed, even when risk or prop rules block new orders (0006).
    Flatten {
        account: AccountId,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_contract"
        )]
        contract: Option<ContractKey>,
    },
    /// The trader's guardrails for the account (roadmap 8b): tighter rules apply at once,
    /// looser ones with the next trade date (`guardrails::GuardrailTracker::request`). Never
    /// clears a lockout.
    SetGuardrails {
        account: AccountId,
        guardrails: crate::guardrails::Guardrails,
    },
    /// The account's prop rules (`None`: none). As for guardrails: what is tighter applies
    /// at once (the next order is checked against it), what is looser with the next trade
    /// date, during a breach too; open orders keep the rules they were checked against. The
    /// account size can't change. Never from a strategy.
    SetRules {
        account: AccountId,
        rules: Option<crate::rules::RulePreset>,
    },
    /// Arm a trigger on an option, at a price of its underlying (decision 0029). A person's, not
    /// a strategy's yet.
    ArmTrigger(crate::trigger::TriggerSpec),
    /// Move an armed trigger to another price of the underlying, as one step: refused, it stays
    /// where it was; a trigger that has fired or ended is refused, so a move can never arm a new
    /// one. An entry keeps its id and the way it fires. A Stop or Target is armed again, which
    /// replaces it.
    MoveTrigger {
        account: AccountId,
        /// The trigger's option: it has to match, and a refusal's note names it.
        #[serde(with = "crate::types::contract_serde")]
        contract: ContractKey,
        id: crate::trigger::TriggerId,
        #[serde(with = "crate::types::price_serde")]
        level: Price,
    },
    /// Disarm one, or stop a fired Target's chase and cancel its order.
    CancelTrigger {
        account: AccountId,
        /// The trigger's option: it has to match, and a refusal's note names it.
        #[serde(with = "contract_serde")]
        contract: ContractKey,
        id: crate::trigger::TriggerId,
    },
    /// Create or replace a strategy instance (decision 0025). It starts Disarmed; an armed
    /// instance must be stopped or paused first. Not from a strategy.
    AddStrategy(StrategyConfig),
    /// Stop an instance (its entries cancelled) and forget it. Not from a strategy.
    RemoveStrategy {
        id: StrategyId,
    },
    /// Let an instance trade. A person's command only (never a strategy, a rule or the API);
    /// nothing else ever arms.
    ArmStrategy {
        id: StrategyId,
    },
    /// Pause an armed instance until it is armed again by hand: its entry orders are
    /// cancelled, protective exits stay. A strategy may pause only itself.
    PauseStrategy {
        id: StrategyId,
        reason: String,
    },
    /// Stop one instance (`id`) or all of them, at once: never gated by anything. Entry orders
    /// are cancelled; with `flatten` each account and contract they trade is flattened too,
    /// otherwise protective exits stay while a position is open. A strategy may stop only
    /// itself.
    StopStrategies {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<StrategyId>,
        flatten: bool,
    },
    /// How manual trades on one account and contract get their exits: per entry (each entry
    /// its own bracket, the default) or one bracket for the whole position (see
    /// [`BracketMode::Position`]). Switching to `Position` with exits already working is refused
    /// unless `replace` (the person confirmed cancelling them), except when they already are
    /// one stop and one target (adopted, as after a restart).
    SetBracketMode {
        account: AccountId,
        #[serde(with = "contract_serde")]
        contract: ContractKey,
        mode: BracketMode,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
    },
}

/// Exits for manual trades on an account and contract.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BracketMode {
    /// Each entry carries its own bracket (if any).
    #[default]
    PerEntry,
    /// One stop and one target (one-cancels-other, reduce-only) for the whole position: placed
    /// when it opens, at these offsets from the average fill price (rounded onto the tick grid
    /// toward the entry); resized to the position as it grows or shrinks, prices kept (a person
    /// may move them); cancelled when it is flat. Entries carry no exits of their own.
    Position { stop_ticks: u32, target_ticks: u32 },
}

impl Command {
    /// The account the command acts on (`None`: strategy controls that name an instance).
    pub fn account(&self) -> Option<&AccountId> {
        match self {
            Command::PlaceOrder(o) | Command::PlaceExit(o) => Some(&o.account),
            Command::PlaceBracket(b) => Some(&b.entry.account),
            Command::ArmTrigger(t) => Some(&t.account),
            Command::CancelTrigger { account, .. } | Command::MoveTrigger { account, .. } => {
                Some(account)
            }
            Command::ModifyOrder { account, .. }
            | Command::CancelOrder { account, .. }
            | Command::CancelAll { account, .. }
            | Command::Flatten { account, .. }
            | Command::SetGuardrails { account, .. }
            | Command::SetRules { account, .. }
            | Command::SetBracketMode { account, .. } => Some(account),
            Command::AddStrategy(c) => Some(&c.account),
            Command::RemoveStrategy { .. }
            | Command::ArmStrategy { .. }
            | Command::PauseStrategy { .. }
            | Command::StopStrategies { .. } => None,
        }
    }

    /// Commands that only reduce risk: never blocked by risk limits or prop rules.
    pub fn always_allowed(&self) -> bool {
        matches!(
            self,
            Command::CancelAll { .. }
                | Command::Flatten { .. }
                | Command::CancelOrder { .. }
                | Command::CancelTrigger { .. }
                | Command::StopStrategies { .. }
        )
    }

    /// An order action (new, modify, cancel, flatten): a person doing one on a strategy's
    /// account and contract pauses that strategy (decisions 0023, 0025).
    pub fn is_order_action(&self) -> bool {
        matches!(
            self,
            Command::PlaceOrder(_)
                | Command::PlaceExit(_)
                | Command::PlaceBracket(_)
                | Command::ModifyOrder { .. }
                | Command::CancelOrder { .. }
                | Command::CancelAll { .. }
                | Command::Flatten { .. }
        )
    }
}

mod opt_qty {
    use lines_core::Qty;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(q: &Option<Qty>, s: S) -> Result<S::Ok, S::Error> {
        match q {
            Some(q) => s.serialize_some(&q.0),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Qty>, D::Error> {
        Option::<i64>::deserialize(d).map(|o| o.map(Qty))
    }
}

mod opt_contract {
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

/// Where a command came from (for the journal, guardrails and the latency metric).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Dom,
    Chart,
    Hotkey,
    Palette,
    /// Issued by the core itself (e.g. a prop rule's auto-liquidation).
    Rule,
    Api,
    Test,
    /// A strategy instance (decision 0025): automated, held to the strategy gate.
    Strategy(StrategyId),
}

/// A command on its way in. `issued` is the input event's time (the click), so the
/// click → order-sent latency covers the whole pipeline.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub id: CommandId,
    pub source: Source,
    pub issued: Instant,
    pub command: Command,
    /// The pane the command came from (handed to the broker with each request).
    pub pane: Option<crate::broker::PaneContext>,
}

/// Why a command was **denied**: refused by the core (validation, broker capabilities, risk,
/// prop rules) before anything was sent. Never confused with a broker's refusal, which is an
/// order that was sent and came back [`crate::OrderState::Rejected`] with the broker's
/// reason. Shown inline where it happened (docs/design.md §3), labelled "denied".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub code: RejectCode,
    pub message: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    UnknownAccount,
    UnknownOrder,
    UnknownContract,
    /// Not on the tick grid, non-positive quantity, nonsensical bracket, …
    Invalid,
    /// A pre-trade risk limit (decision 0006).
    Risk,
    /// A prop-firm rule (decision 0006).
    PropRule,
    /// The trader's own guardrails (roadmap 8b): a lockout, or a limit they set.
    Guardrail,
    /// The market is closed (sessions calendar).
    MarketClosed,
    /// The broker can't do it (capabilities) or refused it.
    Broker,
    /// The strategy gate (decision 0025): the instance's state, scope or limits, or a
    /// strategy control the source may not issue.
    Strategy,
}

impl Reject {
    pub fn new(code: RejectCode, message: impl Into<String>) -> Reject {
        Reject {
            code,
            message: message.into(),
        }
    }
}
