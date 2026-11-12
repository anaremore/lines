//! A dropdown's list, GPUI-free (owner-approved controls, 2026-11-11): the rows, an optional
//! group per row (small-capital headers), one pick or several (checkboxes), an optional last
//! row "Custom…" for a typed value, and the keys: ↑/↓/Home/End move, typing jumps to the first
//! row starting with what was typed, Enter picks (or toggles a multi-select's row), Space
//! toggles a multi-select's row, Esc closes. Choosing "Custom…" turns that row into a field:
//! typing fills it, Enter submits it, Esc goes back to the rows. The view (`settings_view`, the studies menu) draws
//! it floating under its trigger (`crate::dropdown`): it never moves or resizes what's around it.

use std::time::{Duration, Instant};

/// Keys typed within this of each other jump together ("10" finds "10m", not "1m" then "0").
pub const TYPE_AHEAD: Duration = Duration::from_millis(700);

/// The most rows a list shows at once (logical px, at 100 %, with its padding): past that it
/// scrolls inside, the lit row kept in view.
pub const MAX_H: f32 = 240.0;

/// One row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    /// Its group's name (a header above the first row of each group).
    pub group: Option<&'static str>,
}

impl Entry {
    pub fn new(label: impl Into<String>) -> Entry {
        Entry {
            label: label.into(),
            group: None,
        }
    }

    pub fn grouped(label: impl Into<String>, group: &'static str) -> Entry {
        Entry {
            label: label.into(),
            group: Some(group),
        }
    }
}

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing for the caller (the lit row may have moved).
    None,
    /// Pick row `k` and close (one pick).
    Pick(usize),
    /// Toggle row `k`, the list stays open (several picks).
    Toggle(usize),
    /// The "Custom…" field's text, Enter pressed (the caller validates it).
    Submit(String),
    /// Close without a change.
    Close,
}

#[derive(Clone, Debug)]
pub struct DropList {
    pub entries: Vec<Entry>,
    pub multi: bool,
    /// A last row "Custom…".
    pub custom: bool,
    /// The lit row (`entries.len()` is "Custom…").
    pub lit: usize,
    /// "Custom…" chosen: what's typed in its field.
    pub typing: Option<String>,
    typed: String,
    typed_at: Option<Instant>,
}

impl DropList {
    /// A list lit at `current` (the value in force; the first row if none).
    pub fn new(entries: Vec<Entry>, multi: bool, custom: bool, current: Option<usize>) -> DropList {
        let n = entries.len() + usize::from(custom);
        DropList {
            entries,
            multi,
            custom,
            lit: current.filter(|&c| c < n).unwrap_or(0),
            typing: None,
            typed: String::new(),
            typed_at: None,
        }
    }

    /// Rows, "Custom…" included.
    pub fn len(&self) -> usize {
        self.entries.len() + usize::from(self.custom)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The outcome of choosing row `k` (a click, or Enter on it). "Custom…" opens its field.
    pub fn choose(&mut self, k: usize) -> Outcome {
        if self.custom && k == self.entries.len() {
            self.lit = k;
            self.typing = Some(String::new());
            Outcome::None
        } else if k >= self.entries.len() {
            Outcome::None
        } else if self.multi {
            Outcome::Toggle(k)
        } else {
            Outcome::Pick(k)
        }
    }

    /// A key (`key` as GPUI names it, `ch` the text it types) at `now`.
    pub fn key(&mut self, key: &str, ch: Option<&str>, now: Instant) -> Outcome {
        if let Some(text) = self.typing.as_mut() {
            match key {
                "escape" => self.typing = None,
                "enter" => return Outcome::Submit(text.trim().to_owned()),
                "backspace" => {
                    text.pop();
                }
                _ => {
                    if let Some(c) = ch.filter(|c| !c.chars().any(char::is_control))
                        && text.len() < 16
                    {
                        text.push_str(c);
                    }
                }
            }
            return Outcome::None;
        }
        let n = self.len();
        if self.is_empty() {
            return if key == "escape" {
                Outcome::Close
            } else {
                Outcome::None
            };
        }
        match key {
            "escape" => return Outcome::Close,
            "down" => self.lit = (self.lit + 1) % n,
            "up" => self.lit = (self.lit + n - 1) % n,
            "home" => self.lit = 0,
            "end" => self.lit = n - 1,
            "pagedown" => self.lit = (self.lit + 8).min(n - 1),
            "pageup" => self.lit = self.lit.saturating_sub(8),
            "enter" => return self.choose(self.lit),
            "space" if self.multi => return self.choose(self.lit),
            _ => {
                let Some(c) = ch.filter(|c| !c.trim().is_empty()) else {
                    return Outcome::None;
                };
                let fresh = self
                    .typed_at
                    .is_none_or(|t| now.saturating_duration_since(t) > TYPE_AHEAD);
                if fresh {
                    self.typed.clear();
                }
                self.typed.push_str(&c.to_lowercase());
                self.typed_at = Some(now);
                if let Some(k) = self
                    .entries
                    .iter()
                    .position(|e| e.label.to_lowercase().starts_with(&self.typed))
                {
                    self.lit = k;
                }
            }
        }
        Outcome::None
    }

    /// The child index of row `k` in a list drawn with a header before each group's first row
    /// and a separator between groups (and before "Custom…"): for keeping it in view.
    pub fn child_index(&self, k: usize) -> usize {
        let mut child = 0;
        let mut prev: Option<&'static str> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if e.group.is_some() && e.group != prev {
                if prev.is_some() {
                    child += 1; // separator
                }
                child += 1; // header
                prev = e.group;
            }
            if i == k {
                return child;
            }
            child += 1;
        }
        // "Custom…" after its separator.
        child + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(multi: bool, custom: bool) -> DropList {
        let e = ["1m", "2m", "10m", "1h"]
            .iter()
            .map(|s| Entry::new(*s))
            .collect();
        DropList::new(e, multi, custom, Some(1))
    }

    #[test]
    fn arrows_wrap_and_enter_picks() {
        let t = Instant::now();
        let mut l = list(false, false);
        assert_eq!(l.lit, 1, "lit at the value in force");
        assert_eq!(l.key("down", None, t), Outcome::None);
        assert_eq!(l.lit, 2);
        l.key("end", None, t);
        l.key("down", None, t);
        assert_eq!(l.lit, 0, "wraps");
        l.key("up", None, t);
        assert_eq!(l.lit, 3);
        assert_eq!(l.key("enter", None, t), Outcome::Pick(3));
        assert_eq!(l.key("escape", None, t), Outcome::Close);
    }

    #[test]
    fn typing_jumps_to_the_first_match_and_keys_typed_together_jump_together() {
        let t = Instant::now();
        let mut l = list(false, false);
        l.key("1", Some("1"), t);
        assert_eq!(l.lit, 0, "1 → 1m");
        l.key("0", Some("0"), t + Duration::from_millis(100));
        assert_eq!(l.lit, 2, "10 → 10m");
        // A pause starts over.
        l.key("1", Some("1"), t + Duration::from_secs(2));
        l.key("h", Some("h"), t + Duration::from_millis(2100));
        assert_eq!(l.lit, 3, "1h");
        // No match: the lit row stays.
        l.key("z", Some("z"), t + Duration::from_secs(5));
        assert_eq!(l.lit, 3);
    }

    #[test]
    fn a_multi_select_toggles_and_stays_open() {
        let t = Instant::now();
        let mut l = list(true, false);
        assert_eq!(l.key("space", Some(" "), t), Outcome::Toggle(1));
        assert_eq!(l.key("enter", None, t), Outcome::Toggle(1));
        // Space in a one-pick list types nothing.
        let mut one = list(false, false);
        assert_eq!(one.key("space", Some(" "), t), Outcome::None);
    }

    #[test]
    fn custom_is_the_last_row() {
        let t = Instant::now();
        let mut l = list(false, true);
        assert_eq!(l.len(), 5);
        l.key("end", None, t);
        assert_eq!(l.key("enter", None, t), Outcome::None);
        assert_eq!(l.typing.as_deref(), Some(""), "its field opens");
        for c in ["7", "m"] {
            l.key(c, Some(c), t);
        }
        l.key("x", Some("x"), t);
        l.key("backspace", None, t);
        assert_eq!(l.key("enter", None, t), Outcome::Submit("7m".into()));
        // Esc leaves the field for the rows; a second Esc closes.
        assert_eq!(l.key("escape", None, t), Outcome::None);
        assert_eq!(l.typing, None);
        assert_eq!(l.key("escape", None, t), Outcome::Close);
        assert_eq!(l.choose(0), Outcome::Pick(0));
    }

    #[test]
    fn child_indexes_count_headers_and_separators() {
        let e = vec![
            Entry::grouped("1m", "Minutes"),
            Entry::grouped("5m", "Minutes"),
            Entry::grouped("1h", "Hours"),
        ];
        let l = DropList::new(e, false, true, None);
        // Header, 1m, 5m, separator, header, 1h, separator, Custom….
        assert_eq!(l.child_index(0), 1);
        assert_eq!(l.child_index(1), 2);
        assert_eq!(l.child_index(2), 5);
        assert_eq!(l.child_index(3), 7, "Custom…");
        let flat = list(false, false);
        assert_eq!(flat.child_index(2), 2);
    }
}
