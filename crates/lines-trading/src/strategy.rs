//! Strategies as the trading core sees them (decision 0025, pending acceptance).
//!
//! A strategy runs outside the core (`lines-strategy`) and acts only through commands with
//! [`Source::Strategy`](crate::Source::Strategy). The **core** enforces the rules, so a buggy
//! or hung strategy host can't get around them:
//!
//! - **State** ([`StrategyState`]): Disarmed, Armed, Paused or Stopped. Only Armed may enter.
//!   The others may only reduce: cancel, flatten its own contract, modify without adding
//!   quantity, and place orders that only close part of the position.
//! - **Scope**: an instance trades only its own account and contract, and modifies or cancels
//!   only orders it placed.
//! - **Limits** ([`StrategyLimits`], all required): max position (worst case with open
//!   orders), max daily loss on its own fills, orders per minute, allowed hours.
//! - **Paper only**: strategy orders reach only a simulated broker ([`Broker::simulated`]).
//! - Then everything a click passes: prop rules, risk, guardrails.
//!
//! Pauses come from the core itself (manual order activity on the instance's account and
//! contract, an account lockout or breach, a disconnected broker, a silent host, the daily
//! loss) or from the host and the strategy (`Command::PauseStrategy`). Pausing and stopping
//! cancel the instance's entry orders; protective exits stay while a position is open. Nothing
//! resumes automatically: only a manual `Command::ArmStrategy` arms.
//!
//! [`Broker::simulated`]: crate::Broker::simulated

use std::collections::{BTreeSet, VecDeque};

use lines_core::sessions::LocalTime;
use lines_core::{CivilDate, ContractKey, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::position::Position;
use crate::types::{AccountId, Money, OrderId, contract_serde};

/// A strategy instance, assigned when it is created and saved with its configuration (like
/// order ids, stable across runs).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StrategyId(pub u32);

impl std::fmt::Display for StrategyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "strategy {}", self.0)
    }
}

/// One allowed window, in local time of [`TradingHours::tz`]: `start` inclusive, `end`
/// exclusive. A window whose end is not after its start runs past midnight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoursWindow {
    /// `HH:MM`.
    pub start: String,
    /// `HH:MM`.
    pub end: String,
}

/// When an instance may enter (exits are never limited by time, size, loss or rate).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradingHours {
    /// IANA time zone (bundled database: identical on every machine), e.g. `America/New_York`.
    pub tz: String,
    pub windows: Vec<HoursWindow>,
}

/// An instance's own limits, on top of the account's risk, prop rules and guardrails (the
/// tightest wins). All required: there is no "unlimited".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyLimits {
    /// Worst case with its open orders, in quantity units (> 0).
    pub max_position: i64,
    /// Realised (fees included) + unrealised on its own fills per trade date, whole currency
    /// units (> 0). Reaching it pauses the instance.
    pub max_daily_loss: i64,
    /// Entries in any 60 s (> 0), counted when the core accepts them. Exits (cancels, Flatten,
    /// modifies that add nothing, closing orders) are never counted or limited.
    pub max_orders_per_min: u32,
    pub hours: TradingHours,
}

/// An instance as the core knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyConfig {
    pub id: StrategyId,
    /// Shown in events and denials, e.g. `level-touch #2`.
    pub label: String,
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub limits: StrategyLimits,
}

/// What an instance may do.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyState {
    /// Created, or after a restart (armed state is never saved). Never trades.
    #[default]
    Disarmed,
    /// May enter, within its limits.
    Armed,
    /// Stopped by a rule or a request until re-armed by hand; may only reduce.
    Paused,
    /// Stopped by the trader; may only reduce.
    Stopped,
}

impl StrategyState {
    pub fn label(self) -> &'static str {
        match self {
            StrategyState::Disarmed => "disarmed",
            StrategyState::Armed => "armed",
            StrategyState::Paused => "paused",
            StrategyState::Stopped => "stopped",
        }
    }
}

/// Wall-clock time without a heartbeat from the strategy host before the trading service
/// pauses every armed instance (host liveness is a wall-clock matter: a fast replay moves the
/// market clock minutes in a moment).
pub const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// The order-rate window.
const RATE_WINDOW_MS: i64 = 60_000;

impl StrategyLimits {
    /// Check the limits are usable (all positive, hours parse, time zone known).
    pub fn validate(&self) -> Result<(), String> {
        if self.max_position <= 0 {
            return Err("max position must be above 0".into());
        }
        if self.max_daily_loss <= 0 {
            return Err("max daily loss must be above 0".into());
        }
        if self.max_orders_per_min == 0 {
            return Err("max orders per minute must be above 0".into());
        }
        self.hours.check()
    }
}

impl TradingHours {
    fn check(&self) -> Result<(), String> {
        if self.windows.is_empty() {
            return Err("allowed hours need at least one window".into());
        }
        self.zone()?;
        for w in &self.windows {
            w.parse()?;
        }
        Ok(())
    }

    fn zone(&self) -> Result<jiff::tz::TimeZone, String> {
        jiff::tz::TimeZone::get(&self.tz).map_err(|_| format!("unknown time zone {:?}", self.tz))
    }

    /// Whether `t` falls in a window (false when the hours don't parse).
    pub fn allows(&self, t: UtcMillis) -> bool {
        let Ok(tz) = self.zone() else {
            return false;
        };
        let Ok(ts) = jiff::Timestamp::from_millisecond(t.0) else {
            return false;
        };
        let local = ts.to_zoned(tz);
        let m = local.hour() as u16 * 60 + local.minute() as u16;
        self.windows.iter().any(|w| match w.parse() {
            Ok((s, e)) if s < e => m >= s && m < e,
            Ok((s, e)) => m >= s || m < e,
            Err(_) => false,
        })
    }
}

impl HoursWindow {
    pub fn new(start: &str, end: &str) -> HoursWindow {
        HoursWindow {
            start: start.into(),
            end: end.into(),
        }
    }

    fn parse(&self) -> Result<(u16, u16), String> {
        let p = |s: &str| {
            LocalTime::parse(s)
                .map(LocalTime::minutes)
                .ok_or_else(|| format!("bad time {s:?} (use HH:MM)"))
        };
        let (s, e) = (p(&self.start)?, p(&self.end)?);
        if s == e {
            return Err(format!("empty window {}-{}", self.start, self.end));
        }
        Ok((s, e))
    }
}

/// An instance's runtime state in the core.
#[derive(Clone, Debug)]
pub(crate) struct Instance {
    pub cfg: StrategyConfig,
    pub state: StrategyState,
    pub reason: Option<String>,
    /// Orders it placed (bracket exits included), open or not; pruned as they finish.
    pub orders: BTreeSet<OrderId>,
    /// Its own fills (FIFO), for its own P&L.
    pub position: Position,
    /// Its realised P&L, fees included, on `day`.
    pub day_realised: Money,
    pub day: Option<CivilDate>,
    /// When its recent new orders and modifies passed the gate.
    pub recent: VecDeque<UtcMillis>,
    /// The command in flight passed the gate as an entry: count it if the core accepts it.
    pub counting: bool,
}

impl Instance {
    pub fn new(cfg: StrategyConfig) -> Instance {
        Instance {
            cfg,
            state: StrategyState::Disarmed,
            reason: None,
            orders: BTreeSet::new(),
            position: Position::default(),
            day_realised: Money::ZERO,
            day: None,
            recent: VecDeque::new(),
            counting: false,
        }
    }

    /// Orders in the last minute (dropping older ones).
    pub fn rate(&mut self, now: UtcMillis) -> u32 {
        while self
            .recent
            .front()
            .is_some_and(|t| now.0 - t.0 >= RATE_WINDOW_MS)
        {
            self.recent.pop_front();
        }
        self.recent.len() as u32
    }

    /// A new trade date starts its day P&L over.
    pub fn roll_day(&mut self, date: Option<CivilDate>) {
        if date.is_some() && date != self.day {
            self.day = date;
            self.day_realised = Money::ZERO;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hours(tz: &str, w: &[(&str, &str)]) -> TradingHours {
        TradingHours {
            tz: tz.into(),
            windows: w.iter().map(|(s, e)| HoursWindow::new(s, e)).collect(),
        }
    }

    fn t(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    #[test]
    fn hours_in_named_zone_across_dst() {
        let h = hours("America/New_York", &[("09:30", "16:00")]);
        // EDT (UTC-4) in July, EST (UTC-5) in November.
        assert!(h.allows(t("2026-07-14T13:30:00Z")));
        assert!(!h.allows(t("2026-07-14T13:29:59Z")));
        assert!(!h.allows(t("2026-07-14T20:00:00Z")));
        assert!(h.allows(t("2026-11-10T14:30:00Z")));
        assert!(!h.allows(t("2026-11-10T13:30:00Z")));
    }

    #[test]
    fn window_past_midnight() {
        let h = hours("America/Chicago", &[("17:00", "08:30")]);
        assert!(h.allows(t("2026-11-10T23:30:00Z"))); // 17:30 CST
        assert!(h.allows(t("2026-11-10T14:00:00Z"))); // 08:00 CST
        assert!(!h.allows(t("2026-11-10T15:00:00Z"))); // 09:00 CST
    }

    #[test]
    fn limits_must_be_set() {
        let ok = StrategyLimits {
            max_position: 2,
            max_daily_loss: 500,
            max_orders_per_min: 10,
            hours: hours("America/New_York", &[("09:30", "16:00")]),
        };
        assert_eq!(ok.validate(), Ok(()));
        for bad in [
            StrategyLimits {
                max_position: 0,
                ..ok.clone()
            },
            StrategyLimits {
                max_daily_loss: 0,
                ..ok.clone()
            },
            StrategyLimits {
                max_orders_per_min: 0,
                ..ok.clone()
            },
            StrategyLimits {
                hours: hours("America/New_York", &[]),
                ..ok.clone()
            },
            StrategyLimits {
                hours: hours("Mars/Olympus", &[("09:30", "16:00")]),
                ..ok.clone()
            },
            StrategyLimits {
                hours: hours("America/New_York", &[("9h", "16:00")]),
                ..ok.clone()
            },
            StrategyLimits {
                hours: hours("America/New_York", &[("10:00", "10:00")]),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn saved_form_is_stable() {
        let cfg = StrategyConfig {
            id: StrategyId(3),
            label: "example #3".into(),
            account: AccountId::new("paper-1"),
            contract: ContractKey::monthly(
                lines_feed::catalog::ES,
                2026,
                lines_core::Month::new(12).unwrap(),
            ),
            limits: StrategyLimits {
                max_position: 2,
                max_daily_loss: 500,
                max_orders_per_min: 10,
                hours: hours("America/New_York", &[("09:30", "16:00")]),
            },
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: StrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
        assert!(json.starts_with(r#"{"id":3,"label":"example #3","account":"paper-1""#));
    }
}
