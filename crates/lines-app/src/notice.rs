//! Inline notices in the window footer (docs/design.md §3: errors where they happened, never
//! modal). Pure, so the rules are tested without a window.
//!
//! - The same message again replaces the old one (moved to the end, newest last) and counts
//!   the repeats (`… ×3`), so pressing a refused key three times never stacks three copies.
//! - A notice that only explains a refused action expires after [`NOTICE_TTL`]. One the trader
//!   must act on (a file that couldn't be read or saved) stays until dismissed.
//! - At most [`MAX_NOTICES`] are kept; the oldest goes first.

use std::time::{Duration, Instant};

/// How long a notice that needs no action stays.
pub const NOTICE_TTL: Duration = Duration::from_secs(6);
/// Notices shown at once.
pub const MAX_NOTICES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    text: String,
    count: u32,
    /// `None`: stays until dismissed (needs action).
    expires: Option<Instant>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notices(Vec<Notice>);

impl Notices {
    /// Add `text`. `needs_action`: it stays until dismissed; else it expires at
    /// `now + NOTICE_TTL`. A repeat keeps the stricter lifetime of the two.
    pub fn push(&mut self, text: String, needs_action: bool, now: Instant) {
        let expires = (!needs_action).then(|| now + NOTICE_TTL);
        let (count, expires) = match self.0.iter().position(|n| n.text == text) {
            Some(i) => {
                let old = self.0.remove(i);
                let kept = match (old.expires, expires) {
                    (None, _) | (_, None) => None,
                    (Some(_), Some(e)) => Some(e),
                };
                (old.count.saturating_add(1), kept)
            }
            None => (1, expires),
        };
        if self.0.len() >= MAX_NOTICES {
            self.0.remove(0);
        }
        self.0.push(Notice {
            text,
            count,
            expires,
        });
    }

    /// Drop expired notices; `true` if any went (redraw the footer).
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.0.len();
        self.0.retain(|n| n.expires.is_none_or(|e| e > now));
        self.0.len() != before
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The footer text: notices oldest first, repeats counted.
    pub fn text(&self) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|n| {
                if n.count > 1 {
                    format!("{} ×{}", n.text, n.count)
                } else {
                    n.text.clone()
                }
            })
            .collect();
        Some(parts.join("  ·  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_replace_and_count() {
        let t0 = Instant::now();
        let mut n = Notices::default();
        for _ in 0..3 {
            n.push("no linked DOM".into(), false, t0);
        }
        assert_eq!(n.text().as_deref(), Some("no linked DOM ×3"));
        n.push("other".into(), false, t0);
        n.push("no linked DOM".into(), false, t0);
        // The repeat moves to the end (newest last), still one copy.
        assert_eq!(n.text().as_deref(), Some("other  ·  no linked DOM ×4"));
    }

    #[test]
    fn refusals_expire_and_action_notices_stay() {
        let t0 = Instant::now();
        let mut n = Notices::default();
        n.push("couldn't save workspace".into(), true, t0);
        n.push("the last visible pane stays".into(), false, t0);
        assert!(!n.expire(t0 + NOTICE_TTL - Duration::from_millis(1)));
        assert!(n.expire(t0 + NOTICE_TTL));
        assert_eq!(n.text().as_deref(), Some("couldn't save workspace"));
        assert!(!n.expire(t0 + NOTICE_TTL * 100), "needs action: stays");
        n.clear();
        assert!(n.is_empty() && n.text().is_none());
    }

    #[test]
    fn a_repeat_refreshes_the_timeout_and_keeps_needs_action() {
        let t0 = Instant::now();
        let mut n = Notices::default();
        n.push("a".into(), false, t0);
        n.push("a".into(), false, t0 + Duration::from_secs(5));
        assert!(!n.expire(t0 + NOTICE_TTL), "the repeat restarted the timer");
        n.push("a".into(), true, t0 + Duration::from_secs(6));
        n.push("a".into(), false, t0 + Duration::from_secs(7));
        assert!(
            !n.expire(t0 + NOTICE_TTL * 100),
            "once it needs action it stays"
        );
    }

    #[test]
    fn bounded() {
        let t0 = Instant::now();
        let mut n = Notices::default();
        for i in 0..10 {
            n.push(format!("n{i}"), false, t0);
        }
        assert_eq!(n.text().as_deref(), Some("n6  ·  n7  ·  n8  ·  n9"));
    }
}
