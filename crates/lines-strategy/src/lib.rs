//! Strategies (decision 0025).
//!
//! A [`Strategy`] is a **pure, deterministic state machine**, like a provider session
//! (decision 0011): no I/O, no clock reads, no randomness, no threads. Every input comes with
//! [`Ctx::now`]. It answers each [`Input`] with [`Decision`]s: a rule id, a reason in plain US
//! English, the levels it used, and at most one [`Command`]. The host sends the command with
//! `Source::Strategy(id)` and logs the decision with the command's result.
//!
//! The trading core enforces the strategy rules (state, scope, limits, paper only; see
//! `lines_trading::strategy`), so nothing here is trusted for safety.
//!
//! - [`extension`]: [`StrategyExtension`], how private strategies plug in (decision 0020).
//! - [`backtest`]: one instance over a recording (or stored session) plus recorded levels,
//!   with the paper broker on the market clock: a deterministic [`backtest::Report`].
//! - [`host`]: runs instances in the app (live sources and replays) on their own thread.
//! - [`example`]: a trivial, synthetic level-touch strategy with no edge (tests, docs).

pub mod backtest;
pub mod example;
pub mod extension;
pub mod host;

use std::sync::Arc;

use lines_core::{ContractKey, Price, Product, UtcMillis};
use lines_feed::levels::LevelMap;
use lines_feed::levels::runtime::LevelStatus;
use lines_feed::provider::MarketEvent;
use lines_trading::{AccountId, Command, Event, StrategyId, StrategyState};
use serde::Serialize;

pub use extension::StrategyExtension;
pub use lines_feed::levels::LevelValue;

/// What the strategy is told about each call.
#[derive(Copy, Clone, Debug)]
pub struct Ctx<'a> {
    /// Market time (live: the wall clock; replays and backtests: the recorded time).
    pub now: UtcMillis,
    pub id: StrategyId,
    pub account: &'a AccountId,
    pub contract: ContractKey,
    pub product: &'a Product,
    /// As the trading core last reported it (only Armed may enter).
    pub state: StrategyState,
}

/// A level source's current map, as the strategy sees it.
#[derive(Clone, Debug)]
pub struct LevelView {
    pub source: &'static str,
    pub status: LevelStatus,
    pub map: Option<Arc<LevelMap>>,
    /// When the map was last received or confirmed.
    pub received: Option<UtcMillis>,
}

/// One input.
#[derive(Copy, Clone, Debug)]
pub enum Input<'a> {
    /// One feed batch for the instance's contract.
    Market(&'a [MarketEvent]),
    /// A required level source's map or freshness changed.
    Levels(&'a LevelView),
    /// A trading-core event for the instance (its account and contract, its commands, its
    /// state).
    Trading(&'a Event),
    /// Time passed (so time rules fire in a quiet market).
    Timer,
}

/// A level a decision used (for the audit log and the strategy panel).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LevelRef {
    pub source: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(with = "lines_trading::types::price_serde")]
    pub price: Price,
}

/// One decision: why, from which levels, and optionally one command.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    /// Stable rule id, e.g. `touch-long`.
    pub rule: String,
    /// Plain US English, e.g. "Buy limit at Support 6012.50: price touched it from above".
    pub reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<LevelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Command>,
}

impl Decision {
    /// A decision with no command (a skip, a note).
    pub fn note(rule: &str, reason: impl Into<String>) -> Decision {
        Decision {
            rule: rule.into(),
            reason: reason.into(),
            levels: Vec::new(),
            command: None,
        }
    }
}

pub trait Strategy: Send {
    /// Handle one input; push decisions to `out` (in the order the commands should go).
    fn on_input(&mut self, ctx: &Ctx<'_>, input: Input<'_>, out: &mut Vec<Decision>);

    /// Short lines describing its state (strategy panel, reports).
    fn status(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Whether a trading-core event is for an instance: its own commands' results and state, and
/// what happens on its account and contract. The one definition for the host and backtests.
pub fn concerns(cfg: &lines_trading::StrategyConfig, e: &Event) -> bool {
    let (a, k) = (&cfg.account, cfg.contract);
    match e {
        Event::Command { source, .. } => *source == lines_trading::Source::Strategy(cfg.id),
        Event::Order(o) => &o.account == a && o.contract == k,
        Event::Fill(f) => &f.account == a && f.contract == k,
        Event::Position(p) => &p.account == a && p.contract == k,
        Event::Account(v) => &v.account == a,
        Event::Breach { account, .. }
        | Event::Guardrail { account, .. }
        | Event::ResetWaiting { account }
        | Event::AccountReset { account, .. } => account == a,
        Event::Overfill { account, .. } => account == a,
        Event::CancelFailed {
            account, contract, ..
        } => account == a && *contract == k,
        Event::Trigger(t) => &t.account == a && t.contract == k,
        Event::Strategy { id, .. } => *id == cfg.id,
        Event::Rebuilt { .. } => true,
    }
}

/// How to bring a level value onto the tick grid (decision 0002: never silently).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// The grid price at or below the value.
    Down,
    /// The grid price at or above the value.
    Up,
}

/// A level value as an order price. `Tick` values are exact; `Raw` values are rounded as
/// asked; VWAP-relative values have no fixed price (`None`).
pub fn to_tick(value: &LevelValue, product: &Product, rounding: Rounding) -> Option<Price> {
    let tick = product.price.tick;
    match *value {
        LevelValue::Tick(p) => Some(p),
        LevelValue::Raw(v) if v.is_finite() && tick > 0 => {
            let steps = v / tick as f64;
            let steps = match rounding {
                Rounding::Down => steps.floor(),
                Rounding::Up => steps.ceil(),
            };
            (steps.abs() < (i64::MAX / tick) as f64).then(|| Price(steps as i64 * tick))
        }
        LevelValue::Raw(_) | LevelValue::VwapOffset(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_tick_rounds_only_as_asked() {
        let es = lines_feed::catalog::get(lines_feed::catalog::ES).unwrap();
        assert_eq!(
            to_tick(&LevelValue::Tick(Price(600_025)), &es, Rounding::Up),
            Some(Price(600_025))
        );
        assert_eq!(
            to_tick(&LevelValue::Raw(600_030.0), &es, Rounding::Down),
            Some(Price(600_025))
        );
        assert_eq!(
            to_tick(&LevelValue::Raw(600_030.0), &es, Rounding::Up),
            Some(Price(600_050))
        );
        assert_eq!(
            to_tick(&LevelValue::Raw(-30.0), &es, Rounding::Down),
            Some(Price(-50))
        );
        assert_eq!(to_tick(&LevelValue::Raw(f64::NAN), &es, Rounding::Up), None);
        assert_eq!(
            to_tick(&LevelValue::VwapOffset(5.0), &es, Rounding::Up),
            None
        );
    }
}
