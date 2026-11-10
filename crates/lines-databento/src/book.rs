//! mbp-10 snapshots → `BookClear` / `BookLevel` batches.
//!
//! Every mbp-10 record carries the top 10 levels per side *after* its event. The book is
//! consistent only at the end of an event (`F_LAST`), so a batch is emitted only there. Each
//! batch is the exact difference between the levels last emitted and the new top 10: changed
//! levels with their new total, and levels that left the top 10 with quantity 0. So the
//! replayed book always equals Databento's top 10, never a stale deeper level.
//!
//! The first batch, and the first after a book clear (action `R`), is a `BookClear` followed by
//! the whole snapshot.

use std::collections::BTreeMap;

use dbn::{BidAskPair, UNDEF_PRICE};
use lines_core::{Price, Qty, Side};
use lines_feed::native::NativeEvent;

use crate::price::{PriceConv, PriceError};

/// (quantity, order count) per price.
type Levels = BTreeMap<Price, (Qty, u32)>;

#[derive(Default, Debug, Clone)]
pub struct BookDiff {
    bids: Levels,
    asks: Levels,
    /// The next batch starts with `BookClear`.
    needs_clear: bool,
    started: bool,
}

/// Counters the importer reports.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookStats {
    pub batches: u64,
    pub levels_emitted: u64,
    pub clears: u64,
    /// Levels refused because their price isn't exact or on the tick grid.
    pub off_grid_levels: u64,
    /// Two levels at one price in one snapshot (the first is kept).
    pub duplicate_levels: u64,
    /// Batches whose best bid >= best ask.
    pub crossed_batches: u64,
}

impl BookDiff {
    pub fn new() -> Self {
        BookDiff {
            needs_clear: true,
            ..Default::default()
        }
    }

    /// The venue cleared the book (action `R`): the next batch starts from scratch.
    pub fn clear(&mut self) {
        self.needs_clear = true;
    }

    fn snapshot(
        levels: &[BidAskPair; 10],
        conv: &PriceConv,
        stats: &mut BookStats,
    ) -> (Levels, Levels) {
        let (mut bids, mut asks) = (Levels::new(), Levels::new());
        for l in levels {
            for (px, sz, ct, book) in [
                (l.bid_px, l.bid_sz, l.bid_ct, &mut bids),
                (l.ask_px, l.ask_sz, l.ask_ct, &mut asks),
            ] {
                if px == UNDEF_PRICE || sz == 0 {
                    continue;
                }
                match conv.price(px) {
                    Ok(p) => {
                        if let std::collections::btree_map::Entry::Vacant(e) = book.entry(p) {
                            e.insert((Qty(sz as i64), ct));
                        } else {
                            stats.duplicate_levels += 1;
                        }
                    }
                    Err(PriceError::Undefined) => {}
                    Err(_) => stats.off_grid_levels += 1,
                }
            }
        }
        (bids, asks)
    }

    fn diff(
        side: Side,
        old: &Levels,
        new: &Levels,
        time_us: i64,
        cleared: bool,
        out: &mut Vec<NativeEvent>,
    ) {
        if !cleared {
            for p in old.keys() {
                if !new.contains_key(p) {
                    out.push(NativeEvent::BookLevel {
                        time_us,
                        side,
                        price: *p,
                        qty: Qty(0),
                        orders: 0,
                    });
                }
            }
        }
        for (p, (q, c)) in new {
            if cleared || old.get(p) != Some(&(*q, *c)) {
                out.push(NativeEvent::BookLevel {
                    time_us,
                    side,
                    price: *p,
                    qty: *q,
                    orders: *c,
                });
            }
        }
    }

    /// The batch for an event-ending snapshot (empty if nothing changed).
    pub fn apply(
        &mut self,
        time_us: i64,
        levels: &[BidAskPair; 10],
        conv: &PriceConv,
        stats: &mut BookStats,
        out: &mut Vec<NativeEvent>,
    ) {
        let (bids, asks) = Self::snapshot(levels, conv, stats);
        let cleared = self.needs_clear || !self.started;
        let start = out.len();
        if cleared {
            out.push(NativeEvent::BookClear { time_us });
            stats.clears += 1;
        }
        Self::diff(Side::Bid, &self.bids, &bids, time_us, cleared, out);
        Self::diff(Side::Ask, &self.asks, &asks, time_us, cleared, out);
        let emitted = out.len() - start;
        if emitted > 0 {
            stats.batches += 1;
            stats.levels_emitted += (emitted - cleared as usize) as u64;
            if let (Some(b), Some(a)) = (bids.keys().next_back(), asks.keys().next())
                && b >= a
            {
                stats.crossed_batches += 1;
            }
        }
        self.bids = bids;
        self.asks = asks;
        self.needs_clear = false;
        self.started = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::catalog;

    fn lvl(bid: (i64, u32), ask: (i64, u32)) -> BidAskPair {
        BidAskPair {
            bid_px: bid.0,
            ask_px: ask.0,
            bid_sz: bid.1,
            ask_sz: ask.1,
            bid_ct: bid.1.min(9),
            ask_ct: ask.1.min(9),
        }
    }

    fn empty() -> [BidAskPair; 10] {
        std::array::from_fn(|_| BidAskPair {
            bid_px: UNDEF_PRICE,
            ask_px: UNDEF_PRICE,
            ..Default::default()
        })
    }

    const P: i64 = 6_000_000_000_000; // 6000.00
    const T: i64 = 250_000_000; // one tick

    #[test]
    fn first_batch_clears_then_diffs_are_exact() {
        let conv = PriceConv::new(&catalog::by_symbol("ES").unwrap());
        let mut b = BookDiff::new();
        let mut st = BookStats::default();
        let mut snap = empty();
        snap[0] = lvl((P, 5), (P + T, 7));
        snap[1] = lvl((P - T, 3), (P + 2 * T, 2));
        let mut out = Vec::new();
        b.apply(1, &snap, &conv, &mut st, &mut out);
        assert!(matches!(out[0], NativeEvent::BookClear { .. }));
        assert_eq!(out.len(), 5);

        // Bid at P goes to 6, the level at P-T leaves the top 10 (moves past level 10).
        snap[0] = lvl((P, 6), (P + T, 7));
        snap[1] = lvl((UNDEF_PRICE, 0), (P + 2 * T, 2));
        out.clear();
        b.apply(2, &snap, &conv, &mut st, &mut out);
        assert_eq!(
            out,
            vec![
                NativeEvent::BookLevel {
                    time_us: 2,
                    side: Side::Bid,
                    price: Price(599975),
                    qty: Qty(0),
                    orders: 0
                },
                NativeEvent::BookLevel {
                    time_us: 2,
                    side: Side::Bid,
                    price: Price(600000),
                    qty: Qty(6),
                    orders: 6
                },
            ]
        );
        // Unchanged: no batch.
        out.clear();
        b.apply(3, &snap, &conv, &mut st, &mut out);
        assert!(out.is_empty());
        assert_eq!((st.batches, st.clears), (2, 1));
    }

    #[test]
    fn a_clear_restarts_from_a_full_snapshot() {
        let conv = PriceConv::new(&catalog::by_symbol("ES").unwrap());
        let mut b = BookDiff::new();
        let mut st = BookStats::default();
        let mut snap = empty();
        snap[0] = lvl((P, 5), (P + T, 7));
        let mut out = Vec::new();
        b.apply(1, &snap, &conv, &mut st, &mut out);
        b.clear();
        out.clear();
        b.apply(2, &snap, &conv, &mut st, &mut out);
        assert!(matches!(out[0], NativeEvent::BookClear { .. }));
        assert_eq!(out.len(), 3, "clear + both sides again");
    }

    #[test]
    fn off_grid_levels_are_counted_not_rounded_and_crossing_is_counted() {
        let conv = PriceConv::new(&catalog::by_symbol("ES").unwrap());
        let mut b = BookDiff::new();
        let mut st = BookStats::default();
        let mut snap = empty();
        snap[0] = lvl((P + 100_000_000, 5), (P, 7)); // bid off tick; would cross
        snap[1] = lvl((P + T, 1), (P + 2 * T, 1)); // bid above the ask: crossed
        let mut out = Vec::new();
        b.apply(1, &snap, &conv, &mut st, &mut out);
        assert_eq!(st.off_grid_levels, 1);
        assert_eq!(st.crossed_batches, 1);
        assert!(
            !out.iter()
                .any(|e| matches!(e, NativeEvent::BookLevel { price, .. } if price.0 % 25 != 0))
        );
    }
}
