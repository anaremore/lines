//! News blackout (roadmap 8b; decision 0006: a prop restriction as data): no new entries from
//! `before_min` minutes before a scheduled release until `after_min` minutes after it, on the
//! products the release moves.
//!
//! The rule lives on the trader's guardrails ([`crate::guardrails::Guardrails::news`]) and on
//! a prop preset ([`crate::rules::RulePreset::news`]); when both are set the tighter of each
//! part applies ([`NewsBlackout::tightest`]). Its action:
//! - **Warn**: nothing is refused; the UI shows the window.
//! - **Block**: entries are refused inside the window.
//! - **Flatten**: as Block, and at the window's start the core flattens each position the
//!   release affects (a `Source::Rule` Flatten, which also cancels that contract's orders).
//!
//! **Exits are never blocked** (as every guardrail): Flatten, Cancel All, cancels and orders
//! that only close the position. The schedule is read on the core's clock (the market clock in
//! a replay). A schedule that can't answer for the date (missing, or stale) is never taken as
//! clear: Block and Flatten refuse entries on the products it can't answer for.

use lines_core::UtcMillis;
use lines_core::econ::{Coverage, EconSchedule, Importance};
use serde::{Deserialize, Serialize};

const MINUTE: i64 = 60_000;

/// What a blackout does.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsAction {
    /// Show it; refuse nothing.
    Warn,
    /// Refuse new entries.
    Block,
    /// Refuse new entries and flatten affected positions when the window opens.
    Flatten,
}

impl NewsAction {
    pub fn as_str(self) -> &'static str {
        match self {
            NewsAction::Warn => "warn",
            NewsAction::Block => "block",
            NewsAction::Flatten => "flatten",
        }
    }
}

fn two() -> u32 {
    2
}

/// A news blackout rule. Defaults: two minutes either side of high-importance releases.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NewsBlackout {
    pub action: NewsAction,
    /// Minutes before the release the window opens.
    #[serde(default = "two")]
    pub before_min: u32,
    /// Minutes after the release the window closes.
    #[serde(default = "two")]
    pub after_min: u32,
    /// Also medium-importance releases (PPI, GDP, PCE); otherwise high only (CPI, jobs, FOMC).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_medium: bool,
}

impl NewsBlackout {
    pub fn new(action: NewsAction) -> NewsBlackout {
        NewsBlackout {
            action,
            before_min: 2,
            after_min: 2,
            include_medium: false,
        }
    }

    pub fn min_importance(&self) -> Importance {
        if self.include_medium {
            Importance::Medium
        } else {
            Importance::High
        }
    }

    pub fn before_ms(&self) -> i64 {
        self.before_min as i64 * MINUTE
    }

    pub fn after_ms(&self) -> i64 {
        self.after_min as i64 * MINUTE
    }

    /// Part by part, the tighter rule: the stronger action, the wider window, more releases.
    pub fn tightest(&self, o: &NewsBlackout) -> NewsBlackout {
        NewsBlackout {
            action: self.action.max(o.action),
            before_min: self.before_min.max(o.before_min),
            after_min: self.after_min.max(o.after_min),
            include_medium: self.include_medium || o.include_medium,
        }
    }

    /// The tighter of two optional rules (a rule is tighter than none).
    pub fn tighter(a: Option<NewsBlackout>, b: Option<NewsBlackout>) -> Option<NewsBlackout> {
        match (a, b) {
            (Some(x), Some(y)) => Some(x.tightest(&y)),
            (x, y) => x.or(y),
        }
    }
}

/// Where a product stands under a rule at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewsStatus {
    /// No window now, and the schedule can answer.
    Clear,
    /// Inside the window of a release at `at`; the window ends at `until`.
    Blackout {
        /// "CPI, October 2026".
        title: String,
        at: UtcMillis,
        until: UtcMillis,
    },
    /// The schedule can't answer (none loaded, missing for the date, or stale): never clear.
    Unknown { reason: String },
}

/// "CPI, October 2026".
pub fn title(s: &EconSchedule, e: &lines_core::econ::EconEvent) -> String {
    format!("{}, {}", s.release(e).name, e.label)
}

/// Why the schedule can't answer for `symbol` under `rule`, if it can't (short, US English).
pub fn unknown_reason(
    schedule: Option<&EconSchedule>,
    rule: &NewsBlackout,
    symbol: &str,
    now: UtcMillis,
) -> Option<String> {
    let Some(s) = schedule else {
        return Some("no economic calendar loaded".to_owned());
    };
    match s.coverage(
        symbol,
        now,
        rule.after_ms(),
        rule.before_ms(),
        rule.min_importance(),
    ) {
        Coverage::Current => None,
        Coverage::Missing { release, date } => Some(format!(
            "economic calendar missing {} for {date}",
            s.releases[release].name
        )),
        Coverage::Stale { checked } => Some(format!("economic calendar stale (checked {checked})")),
    }
}

/// Where `symbol` stands under `rule` at `now`. A window in force is reported even when the
/// schedule is also stale (the known event is the more useful answer).
pub fn status(
    schedule: Option<&EconSchedule>,
    rule: &NewsBlackout,
    symbol: &str,
    now: UtcMillis,
) -> NewsStatus {
    if let Some(s) = schedule
        && let Some(e) = s.window_at(
            Some(symbol),
            now,
            rule.before_ms(),
            rule.after_ms(),
            rule.min_importance(),
        )
    {
        return NewsStatus::Blackout {
            title: title(s, e),
            at: e.at,
            until: UtcMillis(e.at.0 + rule.after_ms()),
        };
    }
    match unknown_reason(schedule, rule, symbol, now) {
        Some(reason) => NewsStatus::Unknown { reason },
        None => NewsStatus::Clear,
    }
}

/// May a new entry on `symbol` go out at `now`? The error is the denial's message. Exits are
/// the caller's to let through before asking (as for every guardrail).
pub fn check(
    schedule: Option<&EconSchedule>,
    rule: &NewsBlackout,
    symbol: &str,
    now: UtcMillis,
) -> Result<(), String> {
    if rule.action == NewsAction::Warn {
        return Ok(());
    }
    match status(schedule, rule, symbol, now) {
        NewsStatus::Clear => Ok(()),
        NewsStatus::Blackout { title, until, .. } => Err(format!(
            "News blackout: {title}, {} min left",
            EconSchedule::minutes_left(now, until)
        )),
        NewsStatus::Unknown { reason } => Err(format!(
            "News blackout: {reason}; can't confirm no release, so entries are refused"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tightest_takes_each_part_on_its_own() {
        let a = NewsBlackout {
            action: NewsAction::Warn,
            before_min: 5,
            after_min: 1,
            include_medium: true,
        };
        let b = NewsBlackout::new(NewsAction::Flatten);
        assert_eq!(
            a.tightest(&b),
            NewsBlackout {
                action: NewsAction::Flatten,
                before_min: 5,
                after_min: 2,
                include_medium: true,
            }
        );
        assert_eq!(NewsBlackout::tighter(None, Some(b)), Some(b));
        assert_eq!(NewsBlackout::tighter(None, None), None);
    }

    #[test]
    fn the_saved_form_is_pinned() {
        let r = NewsBlackout::new(NewsAction::Block);
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, r#"{"action":"block","before_min":2,"after_min":2}"#);
        let back: NewsBlackout = serde_json::from_str(r#"{"action":"flatten"}"#).unwrap();
        assert_eq!(back, NewsBlackout::new(NewsAction::Flatten));
    }
}
