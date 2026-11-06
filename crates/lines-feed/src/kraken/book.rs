//! The session's own copy of Kraken's book, in exact units. Kraken expects clients to keep the
//! book truncated to the subscribed depth (levels pushed out are not deleted explicitly), and
//! the checksum is computed over this truncated book, so the session must mirror it exactly;
//! the store's book receives the same changes as `BookLevel` events.

use std::collections::BTreeMap;

use lines_core::{Price, Qty, Side};

use super::num::book_checksum;
use crate::provider::MarketEvent;

#[derive(Debug, Default)]
pub struct Book {
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
    /// Levels kept per side; `None` = unbounded (Kraken Futures sends the full book).
    depth: Option<usize>,
}

fn level(side: Side, price: i64, qty: i64) -> MarketEvent {
    MarketEvent::BookLevel {
        side,
        price: Price(price),
        qty: Qty(qty),
        orders: 0,
    }
}

impl Book {
    pub fn new(depth: Option<usize>) -> Self {
        Book {
            depth,
            ..Default::default()
        }
    }

    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Set one level (`qty == 0` removes it) and emit the change.
    pub fn set(&mut self, side: Side, price: i64, qty: i64, events: &mut Vec<MarketEvent>) {
        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        if qty == 0 {
            if book.remove(&price).is_none() {
                return;
            }
        } else {
            book.insert(price, qty);
        }
        events.push(level(side, price, qty));
    }

    /// Drop levels beyond the subscribed depth (worst prices first), emitting their removal.
    pub fn truncate(&mut self, events: &mut Vec<MarketEvent>) {
        let Some(depth) = self.depth else { return };
        while self.bids.len() > depth {
            let (&p, _) = self.bids.first_key_value().expect("non-empty");
            self.bids.remove(&p);
            events.push(level(Side::Bid, p, 0));
        }
        while self.asks.len() > depth {
            let (&p, _) = self.asks.last_key_value().expect("non-empty");
            self.asks.remove(&p);
            events.push(level(Side::Ask, p, 0));
        }
    }

    /// Kraken's CRC-32 over the 10 best asks then the 10 best bids.
    pub fn checksum(&self) -> u32 {
        book_checksum(
            self.asks.iter().map(|(&p, &q)| (p, q)),
            self.bids.iter().rev().map(|(&p, &q)| (p, q)),
        )
    }

    pub fn levels(&self, side: Side) -> Vec<(i64, i64)> {
        match side {
            Side::Bid => self.bids.iter().rev().map(|(&p, &q)| (p, q)).collect(),
            Side::Ask => self.asks.iter().map(|(&p, &q)| (p, q)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_worst_levels_and_reports_them() {
        let mut b = Book::new(Some(2));
        let mut ev = Vec::new();
        for p in [100, 99, 98] {
            b.set(Side::Bid, p, 1, &mut ev);
        }
        for p in [101, 102, 103] {
            b.set(Side::Ask, p, 1, &mut ev);
        }
        ev.clear();
        b.truncate(&mut ev);
        assert_eq!(b.levels(Side::Bid), [(100, 1), (99, 1)]);
        assert_eq!(b.levels(Side::Ask), [(101, 1), (102, 1)]);
        assert_eq!(ev, [level(Side::Bid, 98, 0), level(Side::Ask, 103, 0)]);
        // Removing an absent level emits nothing.
        ev.clear();
        b.set(Side::Bid, 50, 0, &mut ev);
        assert!(ev.is_empty());
    }
}
