//! Time & sales: a bounded ring buffer of prints (roadmap 5).
//!
//! Fixed capacity, allocated once; pushing overwrites the oldest print and iterating newest
//! first never allocates, so the T&S pane can read it in paint.

use std::fmt;

use crate::price::Price;
use crate::qty::Qty;
use crate::time::UtcMillis;

/// Which side initiated (lifted the offer / hit the bid).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Aggressor {
    /// A buyer took the offer.
    Buy,
    /// A seller hit the bid.
    Sell,
}

/// One trade as shown on the tape.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Print {
    pub time: UtcMillis,
    pub price: Price,
    pub qty: Qty,
    /// `None` when the source doesn't say.
    pub aggressor: Option<Aggressor>,
}

#[derive(Clone, Debug)]
pub struct Tape {
    buf: Vec<Print>,
    /// Index the next print goes to.
    head: usize,
    len: usize,
    /// Prints ever pushed (for "new since last frame" checks).
    pushed: u64,
}

impl Tape {
    pub fn new(capacity: usize) -> Tape {
        let capacity = capacity.max(1);
        Tape {
            buf: Vec::with_capacity(capacity),
            head: 0,
            len: 0,
            pushed: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    pub fn push(&mut self, p: Print) {
        let cap = self.buf.capacity();
        if self.buf.len() < cap {
            self.buf.push(p); // within capacity: no reallocation
        } else {
            self.buf[self.head] = p;
        }
        self.head = (self.head + 1) % cap;
        self.len = (self.len + 1).min(cap);
        self.pushed += 1;
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
        self.len = 0;
    }

    /// The `i`-th newest print (0 = newest).
    pub fn newest(&self, i: usize) -> Option<&Print> {
        if i >= self.len {
            return None;
        }
        let cap = self.buf.capacity();
        Some(&self.buf[(self.head + cap - 1 - i) % cap])
    }

    /// Newest first.
    pub fn iter_newest(&self) -> impl Iterator<Item = &Print> + '_ {
        (0..self.len).filter_map(move |i| self.newest(i))
    }
}

/// `HH:MM:SS.mmm` on `zone`'s clock, the tape's time column.
pub fn write_tape_time<W: fmt::Write>(
    t: UtcMillis,
    zone: &crate::display_tz::DisplayTz,
    out: &mut W,
) -> fmt::Result {
    let (_, _, _, h, m, s) = zone.civil(t);
    write!(out, "{h:02}:{m:02}:{s:02}.{:03}", t.0.rem_euclid(1000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedStr;

    fn p(i: i64) -> Print {
        Print {
            time: UtcMillis(i),
            price: Price(i),
            qty: Qty(1),
            aggressor: None,
        }
    }

    #[test]
    fn ring_keeps_the_newest_and_never_grows() {
        let mut t = Tape::new(3);
        assert!(t.newest(0).is_none());
        for i in 0..5 {
            t.push(p(i));
        }
        assert_eq!(t.len(), 3);
        assert_eq!(t.capacity(), 3);
        let got: Vec<_> = t.iter_newest().map(|x| x.time.0).collect();
        assert_eq!(got, vec![4, 3, 2]);
        assert_eq!(t.pushed(), 5);
        t.clear();
        assert!(t.is_empty());
        t.push(p(9));
        assert_eq!(t.newest(0).unwrap().time.0, 9);
    }

    #[test]
    fn tape_time_has_milliseconds() {
        let mut s = FixedStr::<16>::new();
        write_tape_time(
            UtcMillis::parse_iso("2026-11-06T14:05:31.042Z").unwrap(),
            &crate::display_tz::DisplayTz::utc(),
            &mut s,
        )
        .unwrap();
        assert_eq!(s.as_str(), "14:05:31.042");
        s.clear();
        let et = crate::display_tz::DisplayTz::exchange();
        write_tape_time(
            UtcMillis::parse_iso("2026-11-06T14:05:31.042Z").unwrap(),
            &et,
            &mut s,
        )
        .unwrap();
        assert_eq!(s.as_str(), "09:05:31.042");
    }
}
