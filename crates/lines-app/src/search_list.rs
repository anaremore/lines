//! A searchable list the Settings window opens for a setting with many choices: the time zone
//! (`crate::display_zone`) and the UI font (`crate::fonts`). What's typed filters it, the
//! arrows move, Enter chooses and Esc closes. GPUI-free.
//!
//! The list itself belongs to its setting: a [`Pick`] holds indices into it, and the setting
//! gives the words each choice is searched by.

/// Rows the list shows at once.
pub const ROWS: usize = 8;

/// Indices of `hays` (each choice's lowercase search words, in list order) matching `query`:
/// every word typed is in the choice's words (case and `_`/space ignored). Choices where
/// every typed word starts a word ("eu" in "europe/paris") come first, each group in list
/// order. Empty: all.
pub fn matching<'a>(query: &str, hays: impl IntoIterator<Item = &'a str>) -> Vec<usize> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase().replace('_', " "))
        .collect();
    let mut hits: Vec<(bool, usize)> = hays
        .into_iter()
        .enumerate()
        .filter(|(_, hay)| words.iter().all(|w| hay.contains(w.as_str())))
        .map(|(i, hay)| (!words.iter().all(|w| starts_a_word(hay, w)), i))
        .collect();
    hits.sort();
    hits.into_iter().map(|(_, i)| i).collect()
}

/// `w` begins at the start of `hay` or right after a space, `/` or `(`.
fn starts_a_word(hay: &str, w: &str) -> bool {
    hay.match_indices(w)
        .any(|(at, _)| at == 0 || matches!(hay.as_bytes()[at - 1], b' ' | b'/' | b'('))
}

/// An open list: a search and a selection.
#[derive(Clone, Debug)]
pub struct Pick {
    pub query: String,
    /// The selected match (an index into `hits`).
    pub sel: usize,
    /// Indices into the setting's list, as [`matching`] gives them for `query`.
    pub hits: Vec<usize>,
    /// The setting's search: a query's hits.
    search: fn(&str) -> Vec<usize>,
}

/// What a key did to the list.
#[derive(Clone, Debug, PartialEq)]
pub enum PickKey {
    Typing,
    Ignored,
    Cancel,
    /// Enter on a choice: its index in the setting's list.
    Choose(usize),
}

impl Pick {
    /// Opened on `current` (an index in the setting's list) selected, if it is listed.
    pub fn new(search: fn(&str) -> Vec<usize>, current: Option<usize>) -> Pick {
        let hits = search("");
        let sel = current
            .and_then(|c| hits.iter().position(|&i| i == c))
            .unwrap_or(0);
        Pick {
            query: String::new(),
            sel,
            hits,
            search,
        }
    }

    fn refilter(&mut self) {
        self.hits = (self.search)(&self.query);
        self.sel = 0;
    }

    /// The first row shown (the selection stays in view).
    pub fn top(&self) -> usize {
        self.sel
            .saturating_sub(ROWS / 2)
            .min(self.hits.len().saturating_sub(ROWS))
    }

    /// The selected choice's index in the setting's list, if any matches.
    pub fn selected(&self) -> Option<usize> {
        self.hits.get(self.sel).copied()
    }

    /// Type `text` into the search (screenshots open a list with a search typed).
    #[cfg(any(test, feature = "screenshot"))]
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key("", Some(ch.encode_utf8(&mut [0; 4])));
        }
    }

    pub fn key(&mut self, key: &str, ch: Option<&str>) -> PickKey {
        match key {
            "escape" => PickKey::Cancel,
            "enter" => match self.selected() {
                Some(i) => PickKey::Choose(i),
                None => PickKey::Ignored,
            },
            "up" => {
                self.sel = self.sel.saturating_sub(1);
                PickKey::Typing
            }
            "down" => {
                self.sel = (self.sel + 1).min(self.hits.len().saturating_sub(1));
                PickKey::Typing
            }
            "pageup" => {
                self.sel = self.sel.saturating_sub(ROWS);
                PickKey::Typing
            }
            "pagedown" => {
                self.sel = (self.sel + ROWS).min(self.hits.len().saturating_sub(1));
                PickKey::Typing
            }
            "backspace" => {
                if self.query.pop().is_some() {
                    self.refilter();
                }
                PickKey::Typing
            }
            _ => match ch {
                Some(c) if !c.is_empty() && c.chars().all(|c| !c.is_control()) => {
                    self.query.push_str(c);
                    self.refilter();
                    PickKey::Typing
                }
                _ => PickKey::Ignored,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: [&str; 5] = ["alpha", "beta gamma", "segoe ui", "gamma", "arial black"];

    fn search(q: &str) -> Vec<usize> {
        matching(q, LIST)
    }

    #[test]
    fn every_word_must_match_and_word_starts_rank_first() {
        assert_eq!(search(""), vec![0, 1, 2, 3, 4]);
        assert_eq!(search("GAMMA"), vec![1, 3]);
        assert_eq!(search("ui segoe"), vec![2]);
        // "a" starts "alpha" and "arial black"; it is inside the others.
        assert_eq!(search("a"), vec![0, 4, 1, 3]);
        assert!(search("zzz").is_empty());
    }

    #[test]
    fn typing_filters_moves_and_chooses() {
        let mut p = Pick::new(search, Some(3));
        assert_eq!(p.selected(), Some(3), "opens on the current choice");
        p.type_text("gam");
        assert_eq!(p.hits, vec![1, 3]);
        assert_eq!(p.sel, 0, "a new search selects its first hit");
        assert_eq!(p.key("down", None), PickKey::Typing);
        assert_eq!(p.key("down", None), PickKey::Typing);
        assert_eq!(
            p.key("enter", None),
            PickKey::Choose(3),
            "the last hit holds"
        );
        p.key("backspace", None);
        assert_eq!(p.query, "ga");
        assert_eq!(p.key("escape", None), PickKey::Cancel);
        assert_eq!(p.key("f1", None), PickKey::Ignored);
        let mut p = Pick::new(search, None);
        p.type_text("zzz");
        assert_eq!(p.key("enter", None), PickKey::Ignored, "nothing to choose");
    }
}
