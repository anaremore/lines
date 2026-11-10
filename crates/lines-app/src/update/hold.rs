//! The trade hold (decision 0031 §2): Restart to update waits while any account has a
//! position, a working order or an armed trigger, or a strategy is running (paper included),
//! and says why. Pure: the counts come from the trading cache and the strategy source the
//! Positions pane and the strategy panel already read.

/// What is going on that a restart would interrupt.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    pub positions: usize,
    pub orders: usize,
    pub triggers: usize,
    pub strategies: usize,
}

impl Activity {
    pub fn quiet(&self) -> bool {
        *self == Activity::default()
    }
}

fn count(n: usize, one: &str, many: &str, tail: &str) -> Option<String> {
    match n {
        0 => None,
        1 => Some(format!("1 {one}{tail}")),
        n => Some(format!("{n} {many}{tail}")),
    }
}

/// Why the update waits ("Update waits: 1 position open, 2 working orders"), or `None`.
pub fn hold_reason(a: Activity) -> Option<String> {
    let parts: Vec<String> = [
        count(a.positions, "position", "positions", " open"),
        count(a.orders, "working order", "working orders", ""),
        count(a.triggers, "armed trigger", "armed triggers", ""),
        count(a.strategies, "strategy", "strategies", " running"),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| format!("Update waits: {}", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_order_trigger_or_strategy_holds_the_restart() {
        assert_eq!(hold_reason(Activity::default()), None);
        let pos = Activity {
            positions: 1,
            ..Activity::default()
        };
        assert_eq!(hold_reason(pos).as_deref(), Some("Update waits: 1 position open"));
        let all = Activity {
            positions: 2,
            orders: 1,
            triggers: 3,
            strategies: 1,
        };
        assert_eq!(
            hold_reason(all).as_deref(),
            Some(
                "Update waits: 2 positions open, 1 working order, 3 armed triggers, 1 strategy running"
            )
        );
        assert!(
            hold_reason(Activity {
                strategies: 2,
                ..Activity::default()
            })
            .is_some(),
            "a paper strategy counts too"
        );
    }
}
