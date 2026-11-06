//! Volume at price: traded volume per exact price, for the DOM's volume column and (later) the
//! chart's volume profile layer.
//!
//! Exact `Qty` sums keyed by integer price units. Updated on the feed thread; lookups and range
//! sums are allocation-free, so views can read it in paint.

use std::collections::BTreeMap;

use crate::price::Price;
use crate::qty::Qty;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeAtPrice {
    levels: BTreeMap<i64, Qty>,
    total: Qty,
}

impl VolumeAtPrice {
    pub fn add(&mut self, price: Price, qty: Qty) {
        let v = self.levels.entry(price.0).or_insert(Qty::ZERO);
        *v = v.saturating_add(qty);
        self.total = self.total.saturating_add(qty);
    }

    pub fn clear(&mut self) {
        self.levels.clear();
        self.total = Qty::ZERO;
    }

    pub fn get(&self, price: Price) -> Qty {
        self.levels.get(&price.0).copied().unwrap_or(Qty::ZERO)
    }

    pub fn total(&self) -> Qty {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// Levels with prices in `[lo, hi)`, ascending.
    pub fn range(&self, lo: Price, hi: Price) -> impl Iterator<Item = (Price, Qty)> + '_ {
        let r = if lo.0 < hi.0 {
            Some(self.levels.range(lo.0..hi.0))
        } else {
            None
        };
        r.into_iter().flatten().map(|(p, q)| (Price(*p), *q))
    }

    /// Every level, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (Price, Qty)> + '_ {
        self.levels.iter().map(|(p, q)| (Price(*p), *q))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_exactly_per_price() {
        let mut v = VolumeAtPrice::default();
        v.add(Price(100), Qty(3));
        v.add(Price(100), Qty(4));
        v.add(Price(125), Qty(1));
        assert_eq!(v.get(Price(100)), Qty(7));
        assert_eq!(v.get(Price(75)), Qty::ZERO);
        assert_eq!(v.total(), Qty(8));
        let r: Vec<_> = v.range(Price(100), Price(125)).collect();
        assert_eq!(r, vec![(Price(100), Qty(7))]);
        assert_eq!(v.range(Price(125), Price(100)).count(), 0);
        v.clear();
        assert!(v.is_empty() && v.total() == Qty::ZERO);
    }
}
