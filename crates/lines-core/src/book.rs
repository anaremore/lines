//! Price-level order book (aggregated depth), indexed by exact integer price units.
//!
//! Lives on the feed thread; the UI will read snapshots of the top levels (roadmap 5).

use std::collections::BTreeMap;

use crate::price::Price;
use crate::qty::Qty;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Level {
    pub qty: Qty,
    /// Number of orders at the level, if the source reports it (0 = unknown).
    pub orders: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrderBook {
    bids: BTreeMap<i64, Level>,
    asks: BTreeMap<i64, Level>,
}

impl OrderBook {
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    /// Set a level's size; a zero (or negative) quantity removes the level.
    pub fn set(&mut self, side: Side, price: Price, qty: Qty, orders: u32) {
        let map = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        if qty.0 <= 0 {
            map.remove(&price.0);
        } else {
            map.insert(price.0, Level { qty, orders });
        }
    }

    pub fn remove(&mut self, side: Side, price: Price) {
        self.set(side, price, Qty::ZERO, 0);
    }

    pub fn best_bid(&self) -> Option<(Price, Level)> {
        self.bids.iter().next_back().map(|(p, l)| (Price(*p), *l))
    }

    pub fn best_ask(&self) -> Option<(Price, Level)> {
        self.asks.iter().next().map(|(p, l)| (Price(*p), *l))
    }

    /// Bid at or above ask. Only meaningful at batch boundaries.
    pub fn is_crossed(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some((b, _)), Some((a, _))) if b >= a)
    }

    pub fn len(&self, side: Side) -> usize {
        match side {
            Side::Bid => self.bids.len(),
            Side::Ask => self.asks.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Levels from the inside out (best first).
    pub fn levels(&self, side: Side) -> Box<dyn Iterator<Item = (Price, Level)> + '_> {
        match side {
            Side::Bid => Box::new(self.bids.iter().rev().map(|(p, l)| (Price(*p), *l))),
            Side::Ask => Box::new(self.asks.iter().map(|(p, l)| (Price(*p), *l))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_best_and_crossing() {
        let mut b = OrderBook::default();
        assert!(b.best_bid().is_none() && !b.is_crossed());
        b.set(Side::Bid, Price(100), Qty(5), 2);
        b.set(Side::Bid, Price(99), Qty(7), 0);
        b.set(Side::Ask, Price(101), Qty(3), 1);
        b.set(Side::Ask, Price(102), Qty(1), 1);
        assert_eq!(b.best_bid().unwrap().0, Price(100));
        assert_eq!(
            b.best_ask().unwrap(),
            (
                Price(101),
                Level {
                    qty: Qty(3),
                    orders: 1
                }
            )
        );
        assert!(!b.is_crossed());
        let bids: Vec<_> = b.levels(Side::Bid).map(|(p, _)| p.0).collect();
        assert_eq!(bids, vec![100, 99]);
        b.set(Side::Bid, Price(101), Qty(1), 0);
        assert!(b.is_crossed());
        b.remove(Side::Bid, Price(101));
        b.set(Side::Ask, Price(101), Qty(0), 0); // zero qty removes
        assert_eq!(b.best_ask().unwrap().0, Price(102));
        b.clear();
        assert!(b.is_empty());
    }
}
