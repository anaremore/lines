//! The command palette's order (the owner's palette item; the idea, not the code, from Zed's
//! palette): commands the trader has run from the palette come first, by how often, then fuzzy
//! matches by score (the symbol picker's scorer, `picker::Searcher`). ↑ in an empty palette
//! recalls recent queries. GPUI-free.
//!
//! - **Saved:** [`PaletteHistory`] on the settings file (`palette`, optional, no migration): a
//!   use count per command (keyed by [`key`]: the command's name, a setting's by its stable
//!   settings key and value, so a renamed or reordered command starts again at 0, never takes
//!   another's count) and the last [`QUERIES`] queries, newest first.
//! - **Commands that trade, or can't be taken back, are never boosted** ([`never_boosted`]:
//!   every `Command::Trade`, the strategies' Start, Stop and Stop all, the chain's One-click,
//!   the update's restart and roll-back, Close pane): they aren't remembered at all (no count,
//!   no query), so running one often never puts it at the top of an empty palette, and ↑ never
//!   recalls the query that ran it, where a habitual Enter would run it again.
//! - **Stable:** the order depends only on the query and the history snapshot taken when the
//!   palette opened (a run is counted after it closes), with every tie broken by the palette's
//!   own order: the same query always gives the same list, so nothing moves under the pointer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::commands::{self, Command};

/// Recent queries kept.
pub const QUERIES: usize = 10;

/// What the palette remembers (the settings file's `palette`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteHistory {
    /// Runs from the palette per command, by [`key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uses: BTreeMap<String, u32>,
    /// The last queries a command was run from, newest first, at most [`QUERIES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<String>,
}

impl PaletteHistory {
    pub fn is_empty(&self) -> bool {
        self.uses.is_empty() && self.queries.is_empty()
    }

    /// Its use count for ranking: 0 for a command that is [`never_boosted`].
    fn uses_of(&self, c: Command) -> u32 {
        if never_boosted(c) {
            return 0;
        }
        self.uses.get(&key(c)).copied().unwrap_or(0)
    }

    /// `cmd` was run from the palette with `query` typed: one more use; a non-empty query
    /// becomes the newest (once).
    pub fn record(&mut self, cmd: Command, query: &str) {
        // Neither counted nor its query kept: ↑ then Enter must never re-run one.
        if never_boosted(cmd) {
            return;
        }
        let n = self.uses.entry(key(cmd)).or_insert(0);
        *n = n.saturating_add(1);
        let q = query.trim();
        if !q.is_empty() {
            self.queries.retain(|x| x != q);
            self.queries.insert(0, q.to_owned());
            self.queries.truncate(QUERIES);
        }
    }
}

/// Commands that send or cancel orders, arm trading, start or stop strategies (Stop flattens),
/// restart the app, or close a pane (no way back): never remembered, so never ranked up by use
/// or recalled with ↑, however often they ran.
pub fn never_boosted(c: Command) -> bool {
    use crate::commands::StrategyCmd as S;
    matches!(
        c,
        Command::Trade(_)
            | Command::Strategy(S::StopAll | S::StopAllKeep | S::Start | S::Stop)
            | Command::ChainOneClick
            | Command::ClosePane
            | Command::RestartToUpdate
            | Command::RollBackUpdate
    )
}

/// A command's saved name: its `{:?}` (variant names and the values they carry, all stable),
/// except a setting's, whose `{:?}` holds its position in the settings table: by the setting's
/// key and the value's text instead (`Setting:appearance.theme=Light`).
fn key(c: Command) -> String {
    match c {
        Command::Setting(s) => match (s.setting(), s.value()) {
            (Some(st), Some(v)) => format!("Setting:{}={}", st.key, st.value_text(&v)),
            (Some(st), None) => format!("Setting:{}", st.key),
            (None, _) => "Setting".to_owned(),
        },
        c => format!("{c:?}"),
    }
}

/// The palette's list for `query`: the matching commands (every one for an empty query), those
/// run before first by use count, then the rest by fuzzy score; ties keep `items`' order.
pub fn rank(
    items: &[Command],
    query: &str,
    history: &PaletteHistory,
    searcher: &mut crate::picker::Searcher,
) -> Vec<Command> {
    let labels: Vec<String> = items.iter().map(|c| commands::label(*c)).collect();
    let scores = searcher.score_texts(query, labels.iter().map(String::as_str));
    let mut hits: Vec<(usize, u32, u32)> = scores
        .into_iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|s| (i, history.uses_of(items[i]), s)))
        .collect();
    // Used first (more uses first), then by score; ties in the palette's order.
    hits.sort_by(|a, b| {
        (b.1 > 0)
            .cmp(&(a.1 > 0))
            .then(b.1.cmp(&a.1))
            .then(b.2.cmp(&a.2))
            .then(a.0.cmp(&b.0))
    });
    hits.into_iter().map(|(i, _, _)| items[i]).collect()
}

/// What ↑ does in the palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Up {
    /// Show this older query.
    Recall(String),
    /// On the oldest recalled query: nothing (never wrap to the bottom row).
    Stay,
    /// Move the selection up, as usual.
    Move,
}

/// Recalling recent queries with ↑ in the palette (↓ always moves the selection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recall {
    /// The recalled query's index in [`PaletteHistory::queries`] (`None`: not recalling).
    pub at: Option<usize>,
}

impl Recall {
    /// ↑ with `query` typed and row `selected` chosen. It recalls only with the first row
    /// chosen, in an empty palette or on a recalled query unchanged: the next older query, or
    /// [`Up::Stay`] past the oldest. Anywhere else (the selection moved, a typed or edited
    /// query, no history) [`Up::Move`].
    pub fn up(&mut self, query: &str, selected: usize, history: &PaletteHistory) -> Up {
        if selected != 0 {
            return Up::Move;
        }
        let next = match self.at {
            None if query.is_empty() => 0,
            Some(i) if history.queries.get(i).is_some_and(|q| q == query) => i + 1,
            _ => return Up::Move,
        };
        match history.queries.get(next) {
            Some(q) => {
                self.at = Some(next);
                Up::Recall(q.clone())
            }
            None if self.at.is_some() => Up::Stay,
            None => Up::Move,
        }
    }

    /// The query was edited: no longer recalling.
    pub fn edited(&mut self) {
        self.at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Command as C, TradeCmd};

    fn ranked(query: &str, h: &PaletteHistory) -> Vec<Command> {
        rank(
            &commands::palette(),
            query,
            h,
            &mut crate::picker::Searcher::default(),
        )
    }

    fn labels(cs: &[Command]) -> Vec<String> {
        cs.iter().take(6).map(|c| commands::label(*c)).collect()
    }

    /// Commands run before come first, more uses first; then the rest, fuzzy by score.
    #[test]
    fn used_commands_come_first_then_fuzzy_matches() {
        let items = commands::palette();
        let mut h = PaletteHistory::default();
        assert_eq!(
            ranked("", &h),
            items,
            "nothing run yet: the palette's own order"
        );
        let (a, b) = (items[5], items[9]);
        h.record(b, "");
        h.record(a, "");
        h.record(a, "");
        let all = ranked("", &h);
        assert_eq!(&all[..2], &[a, b], "by use count");
        assert_eq!(
            all.len(),
            items.len(),
            "an empty query still lists every command"
        );
        // Fuzzy: letters in order, not a substring ("nwtb" finds "New tab").
        let q = ranked("nwtb", &PaletteHistory::default());
        assert!(q.contains(&C::NewTab), "{:?}", labels(&q));
        // A command that doesn't match never shows, however often it ran.
        let mut h2 = PaletteHistory::default();
        h2.record(C::NewTab, "");
        assert!(!ranked("zzzzqqq", &h2).contains(&C::NewTab));
        // Among matches, the used one first even when another scores higher.
        let base = ranked("tab", &PaletteHistory::default());
        let last = *base.iter().rev().find(|c| !never_boosted(**c)).unwrap();
        let mut h3 = PaletteHistory::default();
        h3.record(last, "tab");
        assert_eq!(ranked("tab", &h3)[0], last);
    }

    /// The same query and history always give the same list (nothing moves under the
    /// pointer), whatever was typed before.
    #[test]
    fn the_order_is_stable_for_a_query() {
        let mut h = PaletteHistory::default();
        h.record(C::NewTab, "tab");
        let first = ranked("ta", &h);
        let _ = ranked("t", &h);
        let _ = ranked("tab", &h);
        assert_eq!(ranked("ta", &h), first);
        // Equal counts and scores: the palette's order breaks the tie, never chance.
        let items = commands::palette();
        let mut h2 = PaletteHistory::default();
        h2.record(items[3], "");
        h2.record(items[1], "");
        assert_eq!(&ranked("", &h2)[..2], &[items[1], items[3]]);
    }

    /// The last ten queries, newest first, each once; an empty query isn't one.
    #[test]
    fn queries_are_remembered_newest_first() {
        let mut h = PaletteHistory::default();
        for i in 0..12 {
            h.record(C::NewTab, &format!("q{i}"));
        }
        h.record(C::NewTab, "  ");
        h.record(C::NewTab, "q5");
        assert_eq!(h.queries.len(), QUERIES);
        assert_eq!(h.queries[0], "q5");
        assert_eq!(h.queries.iter().filter(|q| *q == "q5").count(), 1);
        assert_eq!(h.queries[1], "q11");
        assert_eq!(h.uses_of(C::NewTab), 14);
    }

    /// ↑ on the first row of an empty palette recalls the newest query, ↑ again older ones;
    /// anywhere else ↑ moves the selection (a moved selection, a typed or edited query, no
    /// older query, no history).
    #[test]
    fn up_recalls_recent_queries() {
        let mut h = PaletteHistory::default();
        h.record(C::NewTab, "old");
        h.record(C::NewTab, "new");
        let mut r = Recall::default();
        assert_eq!(r.up("", 0, &h), Up::Recall("new".into()));
        assert_eq!(r.up("new", 0, &h), Up::Recall("old".into()));
        assert_eq!(
            r.up("old", 0, &h),
            Up::Stay,
            "past the oldest: stays, never wraps"
        );
        // The selection moved off the first row (↓, the wheel): ↑ moves it back, never
        // recalls (a reflexive Enter must run the row the trader went to).
        assert_eq!(Recall::default().up("", 1, &h), Up::Move);
        let mut r = Recall::default();
        r.up("", 0, &h);
        assert_eq!(r.up("new", 2, &h), Up::Move);
        // A typed query: ↑ moves the selection.
        assert_eq!(Recall::default().up("ta", 0, &h), Up::Move);
        // Edited: done.
        r.edited();
        assert_eq!(r.up("newx", 0, &h), Up::Move);
        // No history: nothing to recall.
        assert_eq!(
            Recall::default().up("", 0, &PaletteHistory::default()),
            Up::Move
        );
    }

    /// A command that trades (or restarts the app) run often never moves to the top: an
    /// empty palette's Enter never sends an order, stops a strategy or restarts because of the
    /// history. Every such command in the palette, run 50 times each.
    #[test]
    fn trading_commands_are_never_boosted() {
        use crate::commands::StrategyCmd as S;
        let risky = [
            C::Trade(TradeCmd::Flatten),
            C::Strategy(S::StopAll),
            C::Strategy(S::StopAllKeep),
            C::Strategy(S::Start),
            C::Strategy(S::Stop),
            C::ChainOneClick,
            C::RestartToUpdate,
            C::RollBackUpdate,
        ];
        let mut h = PaletteHistory::default();
        for c in commands::palette()
            .into_iter()
            .filter(|c| never_boosted(*c))
            .chain(risky)
        {
            for _ in 0..50 {
                h.record(c, "");
            }
        }
        assert!(risky.iter().all(|c| never_boosted(*c)));
        assert!(
            h.is_empty(),
            "never remembered: no count, no query to recall"
        );
        let mut h2 = PaletteHistory::default();
        h2.record(C::Trade(TradeCmd::Flatten), "flatten");
        assert!(h2.queries.is_empty());
        assert!(never_boosted(C::ClosePane));
        assert_eq!(ranked("", &h), ranked("", &PaletteHistory::default()));
        assert_eq!(ranked("", &h)[0], commands::palette()[0]);
        assert!(!never_boosted(C::NewTab) && !never_boosted(C::Strategy(S::Open)));
    }

    /// The shipped order keeps the rule the substring filter's tests pinned: "flatten" lists
    /// Flatten first.
    #[test]
    fn flatten_comes_first_for_its_name() {
        let q = ranked("flatten", &PaletteHistory::default());
        assert_eq!(q[0], C::Trade(TradeCmd::Flatten), "{:?}", labels(&q));
    }

    /// A setting command's saved name is its settings key and value text, never its table
    /// position: exactly `Setting:<key>=<value>`, distinct per value.
    #[test]
    fn setting_keys_name_the_setting() {
        let cmds = crate::settings_schema::palette();
        let s = cmds.iter().copied().find(|s| s.value().is_some()).unwrap();
        let st = s.setting().unwrap();
        let v = s.value().unwrap();
        assert_eq!(
            key(C::Setting(s)),
            format!("Setting:{}={}", st.key, st.value_text(&v))
        );
        // Every palette setting command's key is its own.
        let keys: std::collections::BTreeSet<String> =
            cmds.iter().map(|s| key(C::Setting(*s))).collect();
        assert_eq!(keys.len(), cmds.len());
    }

    /// Characters the fuzzy pattern would treat specially are literal: as a negation, "!tab"
    /// would list nearly every command; literal, it lists few or none.
    #[test]
    fn special_characters_are_literal() {
        let not = ranked("!tab", &PaletteHistory::default());
        assert!(not.len() < commands::palette().len() / 2, "{}", not.len());
    }

    /// On the settings file: absent when empty; a file without it reads as empty; one with it
    /// round-trips.
    #[test]
    fn the_settings_file_keeps_it() {
        let s: crate::picker::Settings = serde_json::from_str("{}").unwrap();
        assert!(s.palette.is_empty());
        let mut s = crate::picker::Settings::default();
        assert!(!serde_json::to_string(&s).unwrap().contains("palette"));
        s.palette.record(C::NewTab, "tab");
        let json = serde_json::to_string(&s).unwrap();
        let back: crate::picker::Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.palette, s.palette);
    }

    /// The saved field alone: absent when empty; old files without it read as empty.
    #[test]
    fn the_saved_field_is_optional() {
        let h = PaletteHistory::default();
        assert_eq!(serde_json::to_string(&h).unwrap(), "{}");
        let back: PaletteHistory = serde_json::from_str("{}").unwrap();
        assert!(back.is_empty());
        let mut h = PaletteHistory::default();
        h.record(C::NewTab, "tab");
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, r#"{"uses":{"NewTab":1},"queries":["tab"]}"#);
        assert_eq!(serde_json::from_str::<PaletteHistory>(&json).unwrap(), h);
    }
}
