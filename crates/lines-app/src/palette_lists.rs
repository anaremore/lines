//! Lists the palette opens in its place (roadmap 8b): every account's guardrails and lockout
//! state. Pure: the workspace gathers the accounts and the core's views, and draws the rows.
//! (The economic calendar is a pane: `crate::econ_view`.)

use lines_core::UtcMillis;
use lines_core::display_tz::DisplayTz;
use lines_trading::{GuardState, Guardrails};

/// One row of a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// An account: name, badge, state word, then lines (text, whether it warns).
    Account {
        id: String,
        name: String,
        badge: &'static str,
        state: &'static str,
        lines: Vec<(String, bool)>,
    },
    /// Nothing to list (a line of text).
    Empty(String),
}

/// What Enter on a row opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Open {
    /// Settings › Accounts on this account.
    Account(String),
}

impl Item {
    pub fn open(&self) -> Option<Open> {
        match self {
            Item::Account { id, .. } => Some(Open::Account(id.clone())),
            _ => None,
        }
    }
}

/// The guardrails in a line: "Max 5 trades · 3 losses → 15 min · Daily loss 500 · …".
pub fn summary(g: &Guardrails) -> String {
    let mut parts = Vec::new();
    if let Some(n) = g.max_trades_per_day {
        parts.push(format!("Max {n} trade{}", if n == 1 { "" } else { "s" }));
    }
    if let Some(l) = g.losing_streak {
        parts.push(format!(
            "{} loss{} → {} min",
            l.losses,
            if l.losses == 1 { "" } else { "es" },
            l.cooldown_min
        ));
    }
    if let Some(n) = g.max_daily_loss {
        parts.push(format!("Daily loss {n}"));
    }
    if let Some(n) = g.max_position {
        parts.push(format!("Position {n}"));
    }
    if let Some(n) = g.no_entries_before_close_min {
        parts.push(format!("Close {n} min"));
    }
    if let Some(r) = g.news {
        let w = if r.before_min == r.after_min {
            format!("±{} min", r.before_min)
        } else {
            format!("−{}/+{} min", r.before_min, r.after_min)
        };
        let medium = if r.include_medium { ", medium too" } else { "" };
        parts.push(format!("News {} {w}{medium}", r.action.as_str()));
    }
    if parts.is_empty() {
        "No guardrails".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// What the list needs of one account.
pub struct AccountIn<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub badge: &'static str,
    /// In force now, and looser ones waiting (`AccountView.limits`); `None`: no view yet.
    pub in_force: Option<&'a Guardrails>,
    pub pending: Option<&'a Guardrails>,
    /// The lockout in force (`Event::Guardrail`): state, and its message now.
    pub lock: Option<(GuardState, String)>,
}

pub fn guardrail_items(accounts: &[AccountIn<'_>]) -> Vec<Item> {
    if accounts.is_empty() {
        return vec![Item::Empty(
            "No account Lines can send orders for".to_owned(),
        )];
    }
    accounts
        .iter()
        .map(|a| {
            let mut lines = Vec::new();
            let state = match &a.lock {
                Some((s, msg)) => {
                    lines.push((msg.clone(), true));
                    match s {
                        GuardState::Cooldown => "Cooldown",
                        _ => "Locked",
                    }
                }
                None => "Active",
            };
            lines.push((
                a.in_force
                    .map_or_else(|| "Waiting for the account".to_owned(), summary),
                false,
            ));
            if let Some(p) = a.pending {
                lines.push((format!("At the next trade date: {}", summary(p)), false));
            }
            Item::Account {
                id: a.id.to_owned(),
                name: a.name.to_owned(),
                badge: a.badge,
                state,
                lines,
            }
        })
        .collect()
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "Tue Nov 24 08:30" in the display zone.
pub fn when(z: &DisplayTz, t: UtcMillis) -> String {
    let (y, m, d, h, min, _) = z.civil(t);
    let days = lines_core::CivilDate::new(y as u16, m as u8, d as u8).map_or(0, |c| c.days());
    format!(
        "{} {} {d} {h:02}:{min:02}",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(m as usize).clamp(1, 12) - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_trading::LosingStreak;
    use lines_trading::news::{NewsAction, NewsBlackout};

    #[test]
    fn a_summary_names_every_rule() {
        assert_eq!(summary(&Guardrails::default()), "No guardrails");
        let g = Guardrails {
            max_trades_per_day: Some(5),
            losing_streak: Some(LosingStreak {
                losses: 3,
                cooldown_min: 15,
            }),
            max_daily_loss: Some(500),
            max_position: Some(4),
            no_entries_before_close_min: Some(10),
            news: Some(NewsBlackout::new(NewsAction::Block)),
        };
        assert_eq!(
            summary(&g),
            "Max 5 trades · 3 losses → 15 min · Daily loss 500 · Position 4 · Close 10 min · News block ±2 min"
        );
        let one = Guardrails {
            max_trades_per_day: Some(1),
            news: Some(NewsBlackout {
                before_min: 5,
                include_medium: true,
                ..NewsBlackout::new(NewsAction::Flatten)
            }),
            ..Default::default()
        };
        assert_eq!(
            summary(&one),
            "Max 1 trade · News flatten −5/+2 min, medium too"
        );
    }

    /// A lockout shows its message first and says so in words; a looser change waiting says
    /// when it applies; an account not yet viewed says so.
    #[test]
    fn account_rows_show_lockouts_and_whats_waiting() {
        let now = Guardrails {
            max_trades_per_day: Some(5),
            ..Default::default()
        };
        let later = Guardrails::default();
        let items = guardrail_items(&[
            AccountIn {
                id: "paper",
                name: "Paper",
                badge: "PAPER",
                in_force: Some(&now),
                pending: Some(&later),
                lock: Some((
                    GuardState::Locked,
                    "Locked: max 5 trades reached, until the next trade date".into(),
                )),
            },
            AccountIn {
                id: "sim",
                name: "Dummy Sim",
                badge: "SIM",
                in_force: None,
                pending: None,
                lock: None,
            },
        ]);
        let Item::Account { state, lines, .. } = &items[0] else {
            panic!()
        };
        assert_eq!(*state, "Locked");
        assert_eq!(
            lines,
            &[
                (
                    "Locked: max 5 trades reached, until the next trade date".to_owned(),
                    true
                ),
                ("Max 5 trades".to_owned(), false),
                ("At the next trade date: No guardrails".to_owned(), false),
            ]
        );
        assert_eq!(items[0].open(), Some(Open::Account("paper".into())));
        let Item::Account { state, lines, .. } = &items[1] else {
            panic!()
        };
        assert_eq!(*state, "Active");
        assert_eq!(lines[0].0, "Waiting for the account");
        assert!(matches!(&guardrail_items(&[])[0], Item::Empty(_)));
    }

    #[test]
    fn refresh_states_read_in_words() {
        let s = lines_feed::econ::bundled();
        let now = UtcMillis::parse_iso("2026-11-09T12:00:00Z").unwrap();
        let f = |t: UtcMillis| when(&DisplayTz::utc(), t);
        let text = |r: crate::econ_ui::Refresh| crate::econ_ui::refresh_text(&r, &s, now, &f).0;
        use crate::econ_ui::Refresh;
        assert_eq!(
            text(Refresh {
                not_this_run: true,
                ..Default::default()
            }),
            "Calendar of 2026-11-07 · not refreshed in this run"
        );
        assert_eq!(
            text(Refresh {
                no_key: true,
                ..Default::default()
            }),
            "Calendar of 2026-11-07 · no refresh: FRED API key not set up"
        );
        assert_eq!(
            text(Refresh {
                running: true,
                ..Default::default()
            }),
            "Calendar of 2026-11-07 · refreshing…"
        );
        assert_eq!(
            text(Refresh {
                updated: Some(now),
                ..Default::default()
            }),
            "Calendar of 2026-11-07 · updated Mon Nov 9 12:00"
        );
    }
}
