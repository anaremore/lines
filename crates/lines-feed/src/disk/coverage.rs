//! Which time ranges are known complete for a contract (decision 0019, gaps).
//!
//! Two kinds, both inclusive millisecond ranges, kept sorted and merged:
//! - **bars**: every trade in the range is reflected in the stored bars (from a provider's
//!   closed history snapshot, or from a gap-free live stream).
//! - **trades**: every trade in the range is in the segment files (live stream only: history
//!   arrives as bars, not trades).
//!
//! A range is claimed only after the feed says history is complete for the connection, and is
//! closed at any reported gap or disconnect. Nothing outside a claimed range is assumed:
//! missing stays missing. Stored as a small text file, replaced atomically.

use std::io;
use std::path::Path;

/// Sorted, disjoint, inclusive `[from, to]` ranges in ms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranges(Vec<(i64, i64)>);

impl Ranges {
    pub fn add(&mut self, from: i64, to: i64) {
        if from > to {
            return;
        }
        let (mut lo, mut hi) = (from, to);
        // Merge with every overlapping or adjacent range.
        self.0.retain(|&(a, b)| {
            let touches = a <= hi.saturating_add(1) && b.saturating_add(1) >= lo;
            if touches {
                lo = lo.min(a);
                hi = hi.max(b);
            }
            !touches
        });
        let at = self.0.partition_point(|&(a, _)| a < lo);
        self.0.insert(at, (lo, hi));
    }

    pub fn remove(&mut self, from: i64, to: i64) {
        if from > to {
            return;
        }
        let mut out = Vec::with_capacity(self.0.len() + 1);
        for &(a, b) in &self.0 {
            if b < from || a > to {
                out.push((a, b));
                continue;
            }
            if a < from {
                out.push((a, from - 1));
            }
            if b > to {
                out.push((to + 1, b));
            }
        }
        self.0 = out;
    }

    pub fn contains(&self, t: i64) -> bool {
        self.0.iter().any(|&(a, b)| a <= t && t <= b)
    }

    /// The parts of `[from, to]` not covered.
    pub fn holes(&self, from: i64, to: i64) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        let mut at = from;
        for &(a, b) in &self.0 {
            if b < at {
                continue;
            }
            if a > to {
                break;
            }
            if a > at {
                out.push((at, a - 1));
            }
            at = at.max(b.saturating_add(1));
        }
        if at <= to {
            out.push((at, to));
        }
        out
    }

    pub fn ranges(&self) -> &[(i64, i64)] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub bars: Ranges,
    pub trades: Ranges,
}

impl Coverage {
    /// Missing or unreadable → empty (nothing is assumed complete).
    pub fn load(path: &Path) -> Coverage {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Coverage::default();
        };
        let mut c = Coverage::default();
        for (n, line) in text.lines().enumerate() {
            let mut it = line.split_whitespace();
            let parsed = (|| {
                let kind = it.next()?;
                let a: i64 = it.next()?.parse().ok()?;
                let b: i64 = it.next()?.parse().ok()?;
                Some((kind, a, b))
            })();
            match parsed {
                Some(("bars", a, b)) => c.bars.add(a, b),
                Some(("trades", a, b)) => c.trades.add(a, b),
                _ if line.trim().is_empty() || line.starts_with('#') => {}
                _ => {
                    tracing::warn!("{}:{}: ignoring {line:?}", path.display(), n + 1);
                }
            }
        }
        c
    }

    /// Write atomically (temporary file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut s = String::from("# lines store coverage v1: kind from_ms to_ms (inclusive)\n");
        for (kind, r) in [("bars", &self.bars), ("trades", &self.trades)] {
            for &(a, b) in r.ranges() {
                s.push_str(&format!("{kind} {a} {b}\n"));
            }
        }
        super::write_atomic(path, s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_split_and_report_holes() {
        let mut r = Ranges::default();
        r.add(10, 20);
        r.add(30, 40);
        r.add(21, 25); // adjacent: merges
        assert_eq!(r.ranges(), &[(10, 25), (30, 40)]);
        assert_eq!(r.holes(0, 50), vec![(0, 9), (26, 29), (41, 50)]);
        assert_eq!(r.holes(12, 24), vec![]);
        r.add(5, 35);
        assert_eq!(r.ranges(), &[(5, 40)]);
        r.remove(20, 22);
        assert_eq!(r.ranges(), &[(5, 19), (23, 40)]);
        assert!(r.contains(19) && !r.contains(20) && !r.contains(41));
        r.add(9, 3); // empty: ignored
        assert_eq!(r.ranges().len(), 2);
    }

    #[test]
    fn persists_and_tolerates_junk() {
        let dir = std::env::temp_dir().join(format!("lines-cov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("coverage");
        let mut c = Coverage::default();
        c.bars.add(1, 100);
        c.trades.add(50, 100);
        c.save(&p).unwrap();
        assert_eq!(Coverage::load(&p), c);
        std::fs::write(&p, "bars 1 2\nnonsense\ntrades x y\n").unwrap();
        let c = Coverage::load(&p);
        assert_eq!(c.bars.ranges(), &[(1, 2)]);
        assert!(c.trades.is_empty());
        assert_eq!(Coverage::load(&dir.join("absent")), Coverage::default());
    }
}
