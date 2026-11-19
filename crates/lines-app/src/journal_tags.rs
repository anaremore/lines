//! The journal's tag editor (owner-approved mock, 2026-11-19): a small floating panel over the
//! journal for one trade or every selected trade. Chips for the tags the trades share, a text
//! input (Enter adds, Backspace on an empty input removes the last chip), the most recent tags
//! as one-click chips, and the note. Pure: keys and clicks in, journal [`Edit`]s out; the
//! journal saves them as its usual append-only edit records.
//!
//! - **Tags apply at once**: each add or remove sends an edit for every trade the editor is
//!   for (a trade keeps its other tags; several trades share the chips they all hold).
//! - **The note** applies when the editor closes (Esc, Enter in the note, a click outside), and
//!   only if it was typed in: several trades with different notes keep their own otherwise.

use lines_trading::journal::{Edit, Trade};

/// The longest tag, in characters.
pub const MAX_TAG_CHARS: usize = 64;
/// The longest note, in characters.
pub const MAX_NOTE_CHARS: usize = 400;
/// Recent tags offered at most.
pub const MAX_RECENT: usize = 8;

/// Which field has the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Tags,
    Note,
}

/// One trade as the editor holds it: its tags and note as they are now.
#[derive(Clone, Debug, PartialEq)]
struct Held {
    id: String,
    tags: Vec<String>,
    note: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TagEditor {
    /// The panel's title ("Tags · SIM · Sim1 · 13:09", "Tags · 3 trades").
    pub title: String,
    trades: Vec<Held>,
    /// The tags every trade holds, in the first trade's order.
    pub chips: Vec<String>,
    pub input: String,
    pub note: String,
    /// The trades' notes differ (the note shows empty; typing replaces them all).
    pub mixed_note: bool,
    note_typed: bool,
    pub field: Field,
    /// Recent tags not already chips, newest first.
    pub recent: Vec<String>,
}

/// What a key or click did.
#[derive(Clone, Debug, PartialEq)]
pub enum TagKey {
    /// Still editing, nothing to save.
    Typing,
    /// Save these edits; the editor stays open.
    Changed(Vec<Edit>),
    /// Close the editor, saving these edits (none if nothing changed).
    Close(Vec<Edit>),
    /// Not the editor's key.
    Ignored,
}

/// The most recent distinct tags in `trades` (closed order, newest last), newest first.
pub fn recent_tags<'a>(trades: impl DoubleEndedIterator<Item = &'a Trade>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in trades.rev() {
        for tag in &t.tags {
            if !out.contains(tag) {
                out.push(tag.clone());
            }
        }
        if out.len() >= MAX_RECENT * 4 {
            break;
        }
    }
    out
}

impl TagEditor {
    /// An editor for `trades` (one or more), offering `recent` (newest first).
    pub fn new(title: String, trades: &[&Trade], recent: &[String]) -> TagEditor {
        let held: Vec<Held> = trades
            .iter()
            .map(|t| Held {
                id: t.id.clone(),
                tags: t.tags.clone(),
                note: t.note.clone(),
            })
            .collect();
        let chips: Vec<String> = held
            .first()
            .map(|f| {
                f.tags
                    .iter()
                    .filter(|tag| held.iter().all(|h| h.tags.contains(tag)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let first_note = held.first().map(|h| h.note.clone()).unwrap_or_default();
        let mixed_note = held.iter().any(|h| h.note != first_note);
        let mut e = TagEditor {
            title,
            trades: held,
            chips,
            input: String::new(),
            note: if mixed_note {
                String::new()
            } else {
                first_note
            },
            mixed_note,
            note_typed: false,
            field: Field::Tags,
            recent: recent.to_vec(),
        };
        e.trim_recent();
        e
    }

    /// The ids of the trades it edits.
    #[cfg(test)]
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.trades.iter().map(|h| h.id.as_str())
    }

    pub fn count(&self) -> usize {
        self.trades.len()
    }

    fn trim_recent(&mut self) {
        let chips = &self.chips;
        self.recent.retain(|r| !chips.contains(r));
        self.recent.truncate(MAX_RECENT);
    }

    /// Every trade's edit as it stands (its tags; the typed note, else its own).
    fn edits(&self) -> Vec<Edit> {
        self.trades
            .iter()
            .map(|h| Edit {
                id: h.id.clone(),
                tags: h.tags.clone(),
                note: if self.note_typed {
                    self.note.clone()
                } else {
                    h.note.clone()
                },
                ..Default::default()
            })
            .collect()
    }

    /// Add `tag` (trimmed) to every trade. `None` if it was empty or already a chip.
    pub fn add(&mut self, tag: &str) -> Option<Vec<Edit>> {
        let tag: String = tag.trim().chars().take(MAX_TAG_CHARS).collect();
        if tag.is_empty() || tag.chars().any(char::is_control) || self.chips.contains(&tag) {
            return None;
        }
        for h in &mut self.trades {
            if !h.tags.contains(&tag) {
                h.tags.push(tag.clone());
            }
        }
        self.chips.push(tag);
        self.trim_recent();
        Some(self.edits())
    }

    /// Remove `tag` from every trade. `None` if it isn't a chip.
    pub fn remove(&mut self, tag: &str) -> Option<Vec<Edit>> {
        let i = self.chips.iter().position(|c| c == tag)?;
        let tag = self.chips.remove(i);
        for h in &mut self.trades {
            h.tags.retain(|t| *t != tag);
        }
        Some(self.edits())
    }

    /// Close: the note's edits if it was typed in, else none.
    pub fn close(&self) -> Vec<Edit> {
        if self.note_typed {
            self.edits()
        } else {
            Vec::new()
        }
    }

    /// A key (`key` as GPUI names it; `typed`: the character it types, if any).
    pub fn key(&mut self, key: &str, typed: Option<&str>) -> TagKey {
        match (key, self.field) {
            ("escape", _) => TagKey::Close(self.close()),
            ("tab", _) => {
                self.field = match self.field {
                    Field::Tags => Field::Note,
                    Field::Note => Field::Tags,
                };
                TagKey::Typing
            }
            ("enter", Field::Tags) => {
                let typed = std::mem::take(&mut self.input);
                let mut out = None;
                // "a, b" adds both.
                for part in typed.split(',') {
                    if let Some(e) = self.add(part) {
                        out = Some(e);
                    }
                }
                out.map_or(TagKey::Typing, TagKey::Changed)
            }
            ("enter", Field::Note) => TagKey::Close(self.close()),
            ("backspace", Field::Tags) => {
                if self.input.pop().is_some() {
                    return TagKey::Typing;
                }
                match self.chips.last().cloned() {
                    Some(last) => self.remove(&last).map_or(TagKey::Typing, TagKey::Changed),
                    None => TagKey::Typing,
                }
            }
            ("backspace", Field::Note) => {
                self.note.pop();
                self.note_typed = true;
                TagKey::Typing
            }
            (_, field) => match typed.filter(|c| !c.chars().any(char::is_control)) {
                Some(c) => {
                    let (text, max) = match field {
                        Field::Tags => (&mut self.input, MAX_TAG_CHARS),
                        Field::Note => {
                            self.note_typed = true;
                            (&mut self.note, MAX_NOTE_CHARS)
                        }
                    };
                    if text.chars().count() < max {
                        text.push_str(c);
                    }
                    TagKey::Typing
                }
                None => TagKey::Ignored,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(id: &str, tags: &[&str], note: &str) -> Trade {
        use lines_core::{ContractKey, Month, Price, UtcMillis};
        Trade {
            id: id.into(),
            account: lines_trading::AccountId::new("paper"),
            contract: ContractKey::monthly(lines_feed::catalog::ES, 2026, Month::new(12).unwrap()),
            side: lines_trading::Side::Buy,
            entry_time: UtcMillis(0),
            exit_time: UtcMillis(0),
            trade_date: lines_core::CivilDate::new(2026, 11, 5).unwrap(),
            max_qty: 1,
            entry_qty: 1,
            entry_value: 0,
            exit_qty: 1,
            exit_value: 0,
            realized: lines_trading::Money::ZERO,
            fees: lines_trading::Money::ZERO,
            mae: lines_trading::Money::ZERO,
            mfe: lines_trading::Money::ZERO,
            low: Price(0),
            high: Price(0),
            fills: Vec::new(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            note: note.into(),
            screenshots: Vec::new(),
            incomplete: false,
            excursion_unavailable: false,
            cover_checked: false,
        }
    }

    fn typed(e: &mut TagEditor, s: &str) {
        for c in s.chars() {
            let c = c.to_string();
            e.key(&c, Some(&c));
        }
    }

    /// One trade: Enter adds (commas split), Backspace on an empty input removes the last chip,
    /// each change saves at once with the note untouched; Esc saves a typed note.
    #[test]
    fn one_trade_adds_removes_and_saves_its_note_on_close() {
        let t = trade("a", &["chase"], "late entry");
        let recent = vec!["RI bounce".to_owned(), "chase".to_owned()];
        let mut e = TagEditor::new("Tags".into(), &[&t], &recent);
        assert_eq!(e.chips, ["chase"]);
        assert_eq!(e.recent, ["RI bounce"], "a chip isn't offered again");
        typed(&mut e, "A+, late");
        let TagKey::Changed(edits) = e.key("enter", None) else {
            panic!("saved")
        };
        assert_eq!(edits[0].tags, ["chase", "A+", "late"]);
        assert_eq!(edits[0].note, "late entry");
        assert_eq!(e.input, "");
        // Backspace: the input first, then the last chip.
        typed(&mut e, "x");
        assert_eq!(e.key("backspace", None), TagKey::Typing);
        let TagKey::Changed(edits) = e.key("backspace", None) else {
            panic!("removed")
        };
        assert_eq!(edits[0].tags, ["chase", "A+"]);
        // Enter on an empty input or a repeat saves nothing.
        assert_eq!(e.key("enter", None), TagKey::Typing);
        typed(&mut e, "chase");
        assert_eq!(e.key("enter", None), TagKey::Typing);
        // Esc with the note untouched: nothing more to save.
        assert_eq!(e.key("escape", None), TagKey::Close(Vec::new()));
        // The note: Tab to it, type, Esc saves it with the tags as they are.
        assert_eq!(e.key("tab", None), TagKey::Typing);
        typed(&mut e, "!");
        let TagKey::Close(edits) = e.key("escape", None) else {
            panic!("closed")
        };
        assert_eq!(edits[0].note, "late entry!");
        assert_eq!(edits[0].tags, ["chase", "A+"]);
        assert_eq!(e.key("f5", None), TagKey::Ignored);
    }

    /// Several trades: chips are the tags all hold; adding and removing touch each trade and
    /// keep its other tags; mixed notes stay each trade's own unless typed.
    #[test]
    fn several_trades_share_chips_and_keep_their_own() {
        let a = trade("a", &["chase", "late"], "one");
        let b = trade("b", &["late", "news"], "two");
        let mut e = TagEditor::new("Tags · 2 trades".into(), &[&a, &b], &[]);
        assert_eq!(e.chips, ["late"]);
        assert!(e.mixed_note && e.note.is_empty());
        let edits = e.add("A+").unwrap();
        assert_eq!(edits[0].tags, ["chase", "late", "A+"]);
        assert_eq!(edits[1].tags, ["late", "news", "A+"]);
        assert_eq!(
            (edits[0].note.as_str(), edits[1].note.as_str()),
            ("one", "two")
        );
        let edits = e.remove("late").unwrap();
        assert_eq!(edits[0].tags, ["chase", "A+"]);
        assert_eq!(edits[1].tags, ["news", "A+"]);
        assert_eq!(e.remove("news"), None, "not shared: not a chip");
        e.field = Field::Note;
        typed(&mut e, "same");
        let edits = e.close();
        assert!(edits.iter().all(|x| x.note == "same"));
        assert_eq!(e.ids().collect::<Vec<_>>(), ["a", "b"]);
    }

    /// Recent tags: newest trades first, each once, a tag shown as a chip left out.
    #[test]
    fn recent_tags_are_newest_first() {
        let old = trade("o", &["fade", "A+"], "");
        let new = trade("n", &["chase", "fade"], "");
        assert_eq!(
            recent_tags([&old, &new].into_iter()),
            ["chase", "fade", "A+"]
        );
        let e = TagEditor::new(
            "Tags".into(),
            &[&new],
            &recent_tags([&old, &new].into_iter()),
        );
        assert_eq!(e.recent, ["A+"]);
        // Tags are cleaned: trimmed, cut, never empty or with control characters.
        let mut e = TagEditor::new("Tags".into(), &[&old], &[]);
        assert_eq!(e.add("   "), None);
        assert_eq!(e.add("a\tb"), None);
        let long = "x".repeat(100);
        let edits = e.add(&long).unwrap();
        assert_eq!(edits[0].tags.last().unwrap().chars().count(), MAX_TAG_CHARS);
    }
}
