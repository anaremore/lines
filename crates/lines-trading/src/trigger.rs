//! Triggers on an option's underlying (decision 0029): what they are and what they report. The
//! core holds and runs them (`core/trigger.rs`), so a busy UI can't delay one.
//!
//! A trigger watches the underlying's last trade and fires only in the option's regular hours.
//!
//! - **Entry:** a buy. It fires when the underlying reaches its level from the side it was on
//!   when armed, and sends a limit at the ask, capped at the trader's max. It doesn't chase: the
//!   ask above the cap, or any new-order check failing at that moment, ends it with its reason.
//! - **Stop and Target:** at most one of each per position, linked: when one fires, the other
//!   ends. They close whatever the position is when they fire, so they shrink with it, and they
//!   end if it goes flat or flips. Which way they fire follows the position's direction: a long
//!   call's Stop fires as the underlying falls, a long put's as it rises.
//! - **A fired Stop** closes at once with a market order (decision 0029, an exception to 0028's
//!   refusal of market orders). **A fired Target** limits at the bid and re-prices about every
//!   [`crate::risk::OptionGuard::target_chase_ms`] until filled. Either one with no usable bid
//!   keeps waiting and says so ([`TriggerView::note`]).
//! - **A stale underlying never fires one** ([`TriggerView::stale`]). A print past a level after a
//!   gap (stale, or the market opening past it) fires an exit at once and ends an entry.
//! - **After a restart** entries don't return. Stop and Target on a position still open return
//!   armed ([`TriggerView::restored`]), saved in the trading file from v4 on ([`SavedTrigger`]).

use lines_core::{ContractKey, Price, Qty, Right};
use serde::{Deserialize, Serialize};

use crate::types::{AccountId, CommandId, Side, contract_serde, price_serde, qty_serde};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TriggerId(pub u64);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerKind {
    /// Buy `qty`, at the ask, for no more than `max_price`.
    Entry {
        #[serde(with = "qty_serde")]
        qty: Qty,
        #[serde(with = "price_serde")]
        max_price: Price,
    },
    Stop,
    Target,
}

impl TriggerKind {
    pub fn is_exit(self) -> bool {
        !matches!(self, TriggerKind::Entry { .. })
    }

    pub fn word(self) -> &'static str {
        match self {
            TriggerKind::Entry { .. } => "Entry",
            TriggerKind::Stop => "Stop",
            TriggerKind::Target => "Target",
        }
    }
}

/// What to arm: on an option, at a price of its underlying.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerSpec {
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    #[serde(flatten)]
    pub kind: TriggerKind,
    /// The underlying's price.
    #[serde(with = "price_serde")]
    pub level: Price,
}

/// Which way the underlying has to go to reach the level.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    AtOrAbove,
    AtOrBelow,
}

impl Direction {
    /// A price is at or past `level`.
    pub fn reached(self, level: Price, price: Price) -> bool {
        match self {
            Direction::AtOrAbove => price >= level,
            Direction::AtOrBelow => price <= level,
        }
    }

    /// Which way an exit fires: a position that gains as the underlying rises (long call, short
    /// put) stops below and targets above, and the other kind the reverse.
    pub fn of_exit(kind: TriggerKind, right: Right, position: i64) -> Direction {
        let gains_rising = (position > 0) == (right == Right::Call);
        if (kind == TriggerKind::Stop) == gains_rising {
            Direction::AtOrBelow
        } else {
            Direction::AtOrAbove
        }
    }
}

/// The side that closes a position.
pub fn closing_side(position: i64) -> Side {
    if position > 0 { Side::Sell } else { Side::Buy }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerState {
    /// Watching the underlying.
    Armed,
    /// An exit fired and is closing the position.
    Working,
    /// Over ([`TriggerView::reason`] says why); this is its last event.
    Ended,
}

/// A trigger as the core reports it ([`crate::Event::Trigger`]), after any change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerView {
    pub id: TriggerId,
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    #[serde(flatten)]
    pub kind: TriggerKind,
    #[serde(with = "price_serde")]
    pub level: Price,
    pub direction: Direction,
    pub state: TriggerState,
    /// The command that armed it (`None`: restored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_by: Option<CommandId>,
    /// The underlying's feed is stale in regular hours: it can't fire. Report it loudly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// Back armed after a restart. Report it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restored: bool,
    /// While [`TriggerState::Working`]: why it isn't closing yet (no usable bid, market closed,
    /// a refusal). Report it loudly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Why it ended, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A Stop or Target as saved in the trading file (v4). Entries are never saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTrigger {
    pub id: TriggerId,
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    #[serde(flatten)]
    pub kind: TriggerKind,
    #[serde(with = "price_serde")]
    pub level: Price,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long put's direction is flipped, and a short option's flips again.
    #[test]
    fn exits_fire_in_the_positions_direction() {
        use Direction::{AtOrAbove as Up, AtOrBelow as Down};
        use TriggerKind::{Stop, Target};
        let of = Direction::of_exit;
        assert_eq!(of(Stop, Right::Call, 1), Down, "long call");
        assert_eq!(of(Target, Right::Call, 1), Up);
        assert_eq!(of(Stop, Right::Put, 1), Up, "long put");
        assert_eq!(of(Target, Right::Put, 1), Down);
        assert_eq!(of(Stop, Right::Call, -1), Up, "short call");
        assert_eq!(of(Target, Right::Call, -1), Down);
        assert_eq!(of(Stop, Right::Put, -1), Down, "short put");
        assert_eq!(of(Target, Right::Put, -1), Up);
    }

    #[test]
    fn reaching_includes_the_level() {
        let l = Price(100);
        assert!(Direction::AtOrAbove.reached(l, Price(100)));
        assert!(Direction::AtOrAbove.reached(l, Price(101)));
        assert!(!Direction::AtOrAbove.reached(l, Price(99)));
        assert!(Direction::AtOrBelow.reached(l, Price(100)));
        assert!(Direction::AtOrBelow.reached(l, Price(99)));
        assert!(!Direction::AtOrBelow.reached(l, Price(101)));
    }

    #[test]
    fn the_saved_form_round_trips() {
        let t = SavedTrigger {
            id: TriggerId(7),
            account: AccountId::new("paper"),
            contract: ContractKey::spot(lines_core::ProductId(2001)),
            kind: TriggerKind::Stop,
            level: Price(68_000),
        };
        let v = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<SavedTrigger>(&v).unwrap(), t);
    }
}
