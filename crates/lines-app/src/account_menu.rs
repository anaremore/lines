//! The account list's layout (GPUI-free): sections, one line per account.
//!
//! - **Paper** first: Lines' own accounts.
//! - Per broker, its tradable accounts: "Sierra · Sim" when they are all the venue's
//!   simulation accounts, else the venue's short name.
//! - Per broker, its read-only accounts: "Sierra · read-only", with "live trading off" on the
//!   header's right when any of them is live (or of a mode the venue didn't say). A row says
//!   its own reason only when it isn't that.
//!
//! Rows within a section are in their shown names' order, and a broker's name is dropped from
//! a row's label when the section header already says it.

use crate::trading::AccountInfo;

/// The header's right-hand words for a read-only section holding live accounts.
pub const LIVE_OFF: &str = "live trading off";

/// One account's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuRow {
    /// Index into the account list.
    pub index: usize,
    /// The name as the row shows it.
    pub name: String,
    /// Dimmed: Lines can't trade it.
    pub read_only: bool,
    /// Dim words on the row's right: a paper account's rules, or a read-only account's reason
    /// when the header doesn't already say it.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSection {
    pub title: String,
    /// Dim words on the header's right.
    pub note: Option<&'static str>,
    pub rows: Vec<MenuRow>,
}

/// The sections for `accounts`; `short(venue)` is a broker's short name.
pub fn sections(
    accounts: &[AccountInfo],
    short: impl Fn(usize) -> Option<String>,
) -> Vec<MenuSection> {
    let mut out: Vec<MenuSection> = Vec::new();
    let paper: Vec<MenuRow> = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.venue.is_none())
        .map(|(i, a)| MenuRow {
            index: i,
            name: a.name.clone(),
            read_only: a.read_only.is_some(),
            detail: Some(match (&a.read_only, &a.rules) {
                (Some(why), _) => format!("read-only: {why}"),
                (None, Some(r)) => format!("rules: {} (approximation)", r.name),
                (None, None) => "no prop rules".into(),
            }),
        })
        .collect();
    if !paper.is_empty() {
        out.push(MenuSection {
            title: "Paper".into(),
            note: None,
            rows: paper,
        });
    }
    let mut venues: Vec<usize> = accounts.iter().filter_map(|a| a.venue).collect();
    venues.sort_unstable();
    venues.dedup();
    for v in venues {
        let name = short(v).unwrap_or_else(|| "Broker".into());
        let strip = |label: &str| {
            label
                .strip_prefix(&format!("{name} "))
                .filter(|rest| !rest.is_empty())
                .unwrap_or(label)
                .to_owned()
        };
        let mine: Vec<(usize, &AccountInfo)> = accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.venue == Some(v))
            .collect();
        let tradable: Vec<&(usize, &AccountInfo)> =
            mine.iter().filter(|(_, a)| a.read_only.is_none()).collect();
        if !tradable.is_empty() {
            let all_sim = tradable.iter().all(|(_, a)| a.badge == "SIM");
            let mut rows: Vec<MenuRow> = tradable
                .iter()
                .map(|(i, a)| MenuRow {
                    index: *i,
                    name: strip(&a.name),
                    read_only: false,
                    detail: None,
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            out.push(MenuSection {
                title: if all_sim {
                    format!("{name} · Sim")
                } else {
                    name.clone()
                },
                note: None,
                rows,
            });
        }
        let read_only: Vec<&(usize, &AccountInfo)> =
            mine.iter().filter(|(_, a)| a.read_only.is_some()).collect();
        if !read_only.is_empty() {
            let live = read_only.iter().any(|(_, a)| a.badge == "LIVE");
            let note = live.then_some(LIVE_OFF);
            let mut rows: Vec<MenuRow> = read_only
                .iter()
                .map(|(i, a)| {
                    let why = a.read_only.clone().unwrap_or_default();
                    MenuRow {
                        index: *i,
                        name: strip(&a.name),
                        read_only: true,
                        detail: (note != Some(why.as_str())).then_some(why),
                    }
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            out.push(MenuSection {
                title: format!("{name} · read-only"),
                note,
                rows,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_trading::AccountId;

    fn acct(id: &str, venue: Option<usize>, badge: &'static str, ro: Option<&str>) -> AccountInfo {
        AccountInfo {
            venue,
            badge,
            read_only: ro.map(str::to_owned),
            paper: venue.is_none(),
            ..AccountInfo::paper(AccountId::new(id), id.to_owned(), None)
        }
    }

    /// The owner's layout: Paper, the broker's Sim accounts, its read-only accounts under
    /// "live trading off"; one row each, sorted by the shown name, the broker's name dropped.
    #[test]
    fn paper_then_sim_then_read_only() {
        let all = vec![
            acct("Paper", None, "PAPER", None),
            acct("Sierra **12", Some(0), "LIVE", Some(LIVE_OFF)),
            acct("Sierra **01", Some(0), "LIVE", Some(LIVE_OFF)),
            acct("Sierra Sim1", Some(0), "SIM", None),
            acct(
                "Sierra **07",
                Some(0),
                "LIVE",
                Some("Sierra doesn't say if it's simulated"),
            ),
        ];
        let s = sections(&all, |_| Some("Sierra".into()));
        let titles: Vec<&str> = s.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, ["Paper", "Sierra · Sim", "Sierra · read-only"]);
        assert_eq!(s[0].rows[0].detail.as_deref(), Some("no prop rules"));
        assert_eq!(s[1].rows[0].name, "Sim1");
        assert!(!s[1].rows[0].read_only);
        let ro = &s[2];
        assert_eq!(ro.note, Some(LIVE_OFF));
        let names: Vec<&str> = ro.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["**01", "**07", "**12"], "the shown names' order");
        assert!(ro.rows.iter().all(|r| r.read_only));
        // The header says "live trading off": rows say only a different reason.
        assert_eq!(ro.rows[0].detail, None);
        assert_eq!(
            ro.rows[1].detail.as_deref(),
            Some("Sierra doesn't say if it's simulated")
        );
        // Every account appears exactly once.
        let mut seen: Vec<usize> = s
            .iter()
            .flat_map(|x| x.rows.iter().map(|r| r.index))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 3, 4]);
    }

    /// A Sim account Lines can't trade yet sits with the read-only ones, with its reason; no
    /// "live trading off" without a live account.
    #[test]
    fn a_read_only_sim_account_says_why() {
        let all = vec![acct(
            "Sierra Sim1",
            Some(0),
            "SIM",
            Some("Sim order entry off"),
        )];
        let s = sections(&all, |_| Some("Sierra".into()));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].title, "Sierra · read-only");
        assert_eq!(s[0].note, None);
        assert_eq!(s[0].rows[0].detail.as_deref(), Some("Sim order entry off"));
    }
}
