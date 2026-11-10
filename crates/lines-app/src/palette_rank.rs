//! The command palette's order (the owner's palette item; the idea, not the code, from Zed's
//! palette): commands the trader has run from the palette come first, by how often, then fuzzy
//! matches by score (the symbol picker's scorer, `picker::Searcher`). ↑ in an empty palette
//! recalls recent queries. GPUI-free.
//!
//! - **Saved:** [`PaletteHistory`] on the settings file (`palette`, optional, no migration): a
//!   use count per command (keyed by the command's name, `{:?}`: a renamed command just starts
//!   again at 0) and the last [`QUERIES`] queries, newest first.
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
    /// Runs from the palette per command, by its name (`{:?}`).
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

    fn uses_of(&self, c: Command) -> u32 {
        self.uses.get(&key(c)).copied().unwrap_or(0)
    }

    /// `cmd` was run from the palette with `query` typed: one more use; a non-empty query
    /// becomes the newest (once).
    pub fn record(&mut self, cmd: Command, query: &str) {
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

/// A command's saved name.
fn key(c: Command) -> String {
    format!("{c:?}")
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

/// Recalling recent queries with ↑ (and back with ↓) in the palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recall {
    /// The recalled query's index in [`PaletteHistory::queries`] (`None`: not recalling).
    pub at: Option<usize>,
}

impl Recall {
    /// ↑ with `query` typed and row `selected` chosen: the next older query to show, if this ↑
    /// recalls (an empty palette, or a recalled query unchanged with its first row chosen).
    pub fn up(&mut self, query: &str, selected: usize, history: &PaletteHistory) -> Option<String> {
        let next = match self.at {
            None if query.is_empty() => 0,
            Some(i) if selected == 0 && history.queries.get(i).is_some_and(|q| q == query) => i + 1,
            _ => return None,
        };
        let q = history.queries.get(next)?;
        self.at = Some(next);
        Some(q.clone())
    }

    /// ↓ while a recalled query is shown unchanged with its first row chosen: the next newer
    /// query, or the empty palette after the newest. `None`: ↓ moves the selection as usual.
    pub fn down(
        &mut self,
        query: &str,
        selected: usize,
        history: &PaletteHistory,
    ) -> Option<String> {
        let i = self.at?;
        if selected != 0 || history.queries.get(i).is_none_or(|q| q != query) {
            return None;
        }
        if i == 0 {
            self.at = None;
            return Some(String::new());
        }
        self.at = Some(i - 1);
        history.queries.get(i - 1).cloned()
    }

    /// The query was edited: no longer recalling.
    pub fn edited(&mut self) {
        self.at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Command as C;

    fn ranked(query: &str, h: &PaletteHistory) -> Vec<Command> {
        rank(
            &commands::palette(),
            query,
            h,
            &mut crate::picker::Searcher::default(),
        )
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
        assert!(
            q.contains(&C::NewTab),
            "{:?}",
            q.iter().map(|c| commands::label(*c)).collect::<Vec<_>>()
        );
        // A command that doesn't match never shows, however often it ran.
        let mut h2 = PaletteHistory::default();
        h2.record(C::NewTab, "");
        assert!(!ranked("zzzzqqq", &h2).contains(&C::NewTab));
        // Among matches, the used one first even when another scores higher.
        let base = ranked("tab", &PaletteHistory::default());
        let last = *base.last().unwrap();
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

    /// ↑ in an empty palette recalls the newest, ↑ again older; ↓ goes back, past the newest
    /// to empty; typing or a moved selection ends recalling (↑↓ move the selection again).
    #[test]
    fn up_recalls_recent_queries() {
        let mut h = PaletteHistory::default();
        h.record(C::NewTab, "old");
        h.record(C::NewTab, "new");
        let mut r = Recall::default();
        assert_eq!(r.up("", 0, &h).as_deref(), Some("new"));
        assert_eq!(r.up("new", 0, &h).as_deref(), Some("old"));
        assert_eq!(
            r.up("old", 0, &h),
            None,
            "no older one: the selection moves"
        );
        assert_eq!(r.down("old", 0, &h).as_deref(), Some("new"));
        assert_eq!(r.down("new", 0, &h).as_deref(), Some(""));
        assert_eq!(r.at, None);
        // A typed query: ↑ moves the selection.
        assert_eq!(Recall::default().up("ta", 0, &h), None);
        // A recalled query whose selection moved: ↑↓ move the selection.
        let mut r = Recall::default();
        r.up("", 0, &h);
        assert_eq!(r.up("new", 2, &h), None);
        assert_eq!(r.down("new", 2, &h), None);
        // Edited: done.
        r.edited();
        assert_eq!(r.up("newx", 0, &h), None);
        // No history: nothing to recall.
        assert_eq!(
            Recall::default().up("", 0, &PaletteHistory::default()),
            None
        );
    }

    /// The saved field: absent when empty; old files without it read as empty.
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
