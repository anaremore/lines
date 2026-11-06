//! The DOM ladder (roadmap 5): rows indexed by integer ticks, price grouping, centring.
//!
//! **Rows.** With `row_units = grouping × tick` (catalog price units), row `r` covers prices
//! `[r × row_units, (r + 1) × row_units)`. Grouping only sums: every book level and every traded
//! volume lands in exactly one row, as exact `Qty` sums (decision 0002). Rows run top to bottom
//! from high to low prices.
//!
//! **Work per frame.** [`build_ladder`] visits only the book and volume levels inside the
//! visible price range (three ordered range scans), never the whole book, and writes into a
//! caller-owned buffer of fixed capacity: no allocation, and the work is bounded by
//! `rows × grouping` levels per side whatever the book depth or update rate.
//!
//! **Centring** ([`LadderView`]): the ladder follows the inside market, but only when it
//! leaves the middle half of the rows (so it doesn't jitter with every tick), and never while
//! the pointer is over the ladder (docs/design.md §7: nothing the trader aims at moves).
//! [`LadderView::recentre`] centres on demand (the C key).

use crate::book::{OrderBook, Side};
use crate::price::Price;
use crate::qty::Qty;
use crate::vap::VolumeAtPrice;

/// Grouping presets offered in the UI (ticks per row). Any positive value works.
pub const GROUPING_PRESETS: [u32; 5] = [1, 2, 5, 10, 25];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LadderRow {
    /// Lowest price in the row (the row's label).
    pub price: Price,
    pub bid: Qty,
    pub ask: Qty,
    /// Traded volume in the row.
    pub volume: Qty,
    /// The row holds the best bid / best ask.
    pub best_bid: bool,
    pub best_ask: bool,
    /// The row holds the last trade price.
    pub last: bool,
}

impl LadderRow {
    fn empty(price: Price) -> LadderRow {
        LadderRow {
            price,
            bid: Qty::ZERO,
            ask: Qty::ZERO,
            volume: Qty::ZERO,
            best_bid: false,
            best_ask: false,
            last: false,
        }
    }
}

/// The state of the inside market, shown distinctly by the DOM.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Inside {
    Empty,
    /// Only bids or only asks.
    OneSided,
    Normal,
    /// Best bid == best ask.
    Locked,
    /// Best bid > best ask.
    Crossed,
}

pub fn inside(book: &OrderBook) -> Inside {
    match (book.best_bid(), book.best_ask()) {
        (None, None) => Inside::Empty,
        (Some(_), None) | (None, Some(_)) => Inside::OneSided,
        (Some((b, _)), Some((a, _))) if b == a => Inside::Locked,
        (Some((b, _)), Some((a, _))) if b > a => Inside::Crossed,
        _ => Inside::Normal,
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LadderStats {
    /// Book and volume levels visited (the per-frame work metric).
    pub levels_visited: usize,
    /// Largest bid/ask size among the rows (scales the depth bars).
    pub max_size: Qty,
    pub max_volume: Qty,
}

/// Row index holding `price`.
pub fn row_of(price: Price, row_units: i64) -> i64 {
    price.0.div_euclid(row_units.max(1))
}

/// Fill `out` with `rows` rows (at most `out.capacity()`, so this never allocates), the top
/// row being `top_row`.
pub fn build_ladder(
    book: &OrderBook,
    vap: &VolumeAtPrice,
    last: Option<Price>,
    row_units: i64,
    top_row: i64,
    rows: usize,
    out: &mut Vec<LadderRow>,
) -> LadderStats {
    let u = row_units.max(1);
    let rows = rows.min(out.capacity());
    out.clear();
    let mut stats = LadderStats::default();
    if rows == 0 {
        return stats;
    }
    for k in 0..rows as i64 {
        out.push(LadderRow::empty(Price((top_row - k).saturating_mul(u))));
    }
    let bottom_row = top_row - (rows as i64 - 1);
    let lo = Price(bottom_row.saturating_mul(u));
    let hi = Price((top_row + 1).saturating_mul(u));
    let slot = |p: i64| (top_row - p.div_euclid(u)) as usize;
    for (p, l) in book.range(Side::Bid, lo, hi) {
        let r = &mut out[slot(*p)];
        r.bid = r.bid.saturating_add(l.qty);
        stats.levels_visited += 1;
    }
    for (p, l) in book.range(Side::Ask, lo, hi) {
        let r = &mut out[slot(*p)];
        r.ask = r.ask.saturating_add(l.qty);
        stats.levels_visited += 1;
    }
    for (p, q) in vap.range(lo, hi) {
        let r = &mut out[slot(p.0)];
        r.volume = r.volume.saturating_add(q);
        stats.levels_visited += 1;
    }
    let in_view = |p: Price| p >= lo && p < hi;
    if let Some((b, _)) = book.best_bid().filter(|(b, _)| in_view(*b)) {
        out[slot(b.0)].best_bid = true;
    }
    if let Some((a, _)) = book.best_ask().filter(|(a, _)| in_view(*a)) {
        out[slot(a.0)].best_ask = true;
    }
    if let Some(p) = last.filter(|p| in_view(*p)) {
        out[slot(p.0)].last = true;
    }
    for r in out.iter() {
        stats.max_size = stats.max_size.max(r.bid).max(r.ask);
        stats.max_volume = stats.max_volume.max(r.volume);
    }
    stats
}

/// Which row the ladder centres on: midway between the best bid's and best ask's rows, else
/// whichever side exists, else the last trade.
pub fn inside_row(book: &OrderBook, last: Option<Price>, row_units: i64) -> Option<i64> {
    let row = |p| row_of(p, row_units);
    match (book.best_bid(), book.best_ask()) {
        (Some((b, _)), Some((a, _))) => Some((row(b) + row(a)).div_euclid(2)),
        (Some((p, _)), None) | (None, Some((p, _))) => Some(row(p)),
        (None, None) => last.map(row),
    }
}

/// Per-pane ladder position.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LadderView {
    /// Row shown in the middle; `None` until there is something to centre on.
    centre: Option<i64>,
}

impl LadderView {
    pub fn centre(&self) -> Option<i64> {
        self.centre
    }

    /// Top row for a ladder of `rows` rows.
    pub fn top_row(&self, rows: usize) -> Option<i64> {
        self.centre.map(|c| c + (rows as i64 - 1) / 2)
    }

    /// Follow the inside market: re-centre when it leaves the middle half of the rows. Never
    /// while hovered. Returns whether the ladder moved.
    pub fn follow(&mut self, inside: Option<i64>, rows: usize, hovered: bool) -> bool {
        let Some(i) = inside else { return false };
        if hovered && self.centre.is_some() {
            return false;
        }
        let band = (rows as i64 / 4).max(1);
        match self.centre {
            Some(c) if (i - c).abs() <= band => false,
            _ => {
                self.centre = Some(i);
                true
            }
        }
    }

    /// Centre on the inside market now (the C key).
    pub fn recentre(&mut self, inside: Option<i64>) {
        if inside.is_some() {
            self.centre = inside;
        }
    }

    /// Move by `rows` rows; positive shows higher prices.
    pub fn scroll(&mut self, rows: i64) {
        if let Some(c) = &mut self.centre {
            *c += rows;
        }
    }

    /// The grouping changed: keep the same price in the middle.
    pub fn regroup(&mut self, old_units: i64, new_units: i64) {
        if let Some(c) = &mut self.centre {
            *c = (*c * old_units.max(1)).div_euclid(new_units.max(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: i64 = 25;

    fn book() -> OrderBook {
        let mut b = OrderBook::default();
        // Bids 99.75 .. 98.00, asks 100.00 .. 101.75 (prices in 1/100, tick 25).
        for k in 0..8 {
            b.set(Side::Bid, Price(9_975 - k * TICK), Qty(10 + k), 0);
            b.set(Side::Ask, Price(10_000 + k * TICK), Qty(20 + k), 0);
        }
        b
    }

    #[test]
    fn rows_are_ticks_with_best_and_last_marked() {
        let b = book();
        let mut vap = VolumeAtPrice::default();
        vap.add(Price(10_000), Qty(7));
        let mut out = Vec::with_capacity(16);
        let top = row_of(Price(10_050), TICK);
        let st = build_ladder(&b, &vap, Some(Price(10_000)), TICK, top, 5, &mut out);
        let prices: Vec<_> = out.iter().map(|r| r.price.0).collect();
        assert_eq!(prices, vec![10_050, 10_025, 10_000, 9_975, 9_950]);
        assert_eq!(out[2].ask, Qty(20));
        assert!(out[2].best_ask && out[2].last && !out[2].best_bid);
        assert_eq!(out[2].volume, Qty(7));
        assert!(out[3].best_bid);
        assert_eq!(out[3].bid, Qty(10));
        assert_eq!(st.levels_visited, 2 + 3 + 1);
        assert_eq!(st.max_size, Qty(22));
    }

    #[test]
    fn grouping_sums_exactly_and_loses_nothing() {
        let b = book();
        let vap = VolumeAtPrice::default();
        for g in [1, 2, 5, 10, 25, 3] {
            let u = g * TICK;
            let mut out = Vec::with_capacity(64);
            let top = row_of(Price(10_400), u);
            build_ladder(&b, &vap, None, u, top, 64, &mut out);
            let bids: i64 = out.iter().map(|r| r.bid.0).sum();
            let asks: i64 = out.iter().map(|r| r.ask.0).sum();
            assert_eq!(bids, (10..18).sum::<i64>(), "grouping {g}");
            assert_eq!(asks, (20..28).sum::<i64>(), "grouping {g}");
            for r in &out {
                assert_eq!(r.price.0 % u, 0, "row labels on the group grid");
            }
        }
        // 4 ticks per row: 100.00..100.75 is one row holding asks 20+21+22+23.
        let mut out = Vec::with_capacity(8);
        build_ladder(&b, &vap, None, 100, row_of(Price(10_000), 100), 2, &mut out);
        assert_eq!((out[0].price, out[0].ask), (Price(10_000), Qty(86)));
        assert_eq!(
            (out[1].price, out[1].bid),
            (Price(9_900), Qty(10 + 11 + 12 + 13))
        );
    }

    #[test]
    fn never_allocates_beyond_capacity_and_ignores_far_levels() {
        let mut b = book();
        for k in 0..100_000 {
            b.set(Side::Bid, Price(-k * TICK), Qty(1), 0);
        }
        let mut out = Vec::with_capacity(10);
        let st = build_ladder(
            &b,
            &VolumeAtPrice::default(),
            None,
            TICK,
            row_of(Price(10_100), TICK),
            50,
            &mut out,
        );
        assert_eq!(out.len(), 10, "clamped to capacity");
        assert_eq!(out.capacity(), 10);
        assert!(
            st.levels_visited <= 2 * 10,
            "only the visible range is visited"
        );
    }

    #[test]
    fn inside_states() {
        let mut b = OrderBook::default();
        assert_eq!(inside(&b), Inside::Empty);
        b.set(Side::Bid, Price(100), Qty(1), 0);
        assert_eq!(inside(&b), Inside::OneSided);
        b.set(Side::Ask, Price(125), Qty(1), 0);
        assert_eq!(inside(&b), Inside::Normal);
        b.set(Side::Bid, Price(125), Qty(1), 0);
        assert_eq!(inside(&b), Inside::Locked);
        b.set(Side::Bid, Price(150), Qty(1), 0);
        assert_eq!(inside(&b), Inside::Crossed);
    }

    #[test]
    fn centring_follows_but_never_while_hovered() {
        let mut v = LadderView::default();
        assert!(!v.follow(None, 20, false));
        assert!(
            v.follow(Some(100), 20, true),
            "first centre even if hovered"
        );
        assert_eq!(v.top_row(20), Some(109));
        assert!(
            !v.follow(Some(104), 20, false),
            "within the middle half: stays"
        );
        assert!(!v.follow(Some(200), 20, true), "hovered: never moves");
        assert_eq!(v.centre(), Some(100));
        assert!(v.follow(Some(200), 20, false));
        assert_eq!(v.centre(), Some(200));
        v.scroll(-3);
        assert_eq!(v.centre(), Some(197));
        v.recentre(Some(200));
        assert_eq!(v.centre(), Some(200));
        v.regroup(25, 250);
        assert_eq!(v.centre(), Some(20));
    }

    #[test]
    fn inside_row_is_between_best_bid_and_ask() {
        let b = book();
        assert_eq!(inside_row(&b, None, TICK), Some(399)); // rows 399 (bid) and 400 (ask)
        assert_eq!(
            inside_row(&OrderBook::default(), Some(Price(500)), TICK),
            Some(20)
        );
        assert_eq!(inside_row(&OrderBook::default(), None, TICK), None);
    }
}
