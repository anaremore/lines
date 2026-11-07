//! Footprint chart (roadmap 10e): per bar, traded volume by price row split by aggressor,
//! with the bar's point of control and diagonal imbalances. Pure and allocation-free within
//! the buffers' capacity (the chart builds it every frame; `tests/no_alloc.rs`).
//!
//! **Data.** The store keeps a footprint per 1-minute bar ([`Footprint`]: exact `Qty` sums
//! by price and aggressor, from counted trades only). A chart bar on any timeframe covers a
//! bucket `[start, end)` of 1-minute bars ([`BarsView::end`]), so its footprint is the
//! **merge** of the 1-minute footprints in the bucket: exact integer sums, the same whatever
//! the timeframe (a 5-minute bar's cells equal the sum of its five 1-minute bars' cells).
//!
//! **Rows** group prices as the DOM and the volume profile do
//! (`row = floor(price / (grouping × tick))`, [`crate::ladder::row_of`]); [`fit_grouping`]
//! coarsens the grouping until a row is tall enough to read.
//!
//! **Point of control:** the bar's largest row; ties as the volume profile
//! ([`ProfileRows::build`]): nearest the middle of the bar's rows, then the lower.
//!
//! **Imbalance** (diagonal, as traders read a footprint): buying at row `r` against selling
//! one row **below** (`r − 1`), selling at row `r` against buying one row **above**
//! (`r + 1`). A buy imbalance at `r` when `buy[r] ≥ min` and
//! `100 × buy[r] ≥ ratio% × sell[r − 1]`; a sell imbalance at `r` when `sell[r] ≥ min` and
//! `100 × sell[r] ≥ ratio% × buy[r + 1]`. An empty opposite row counts as 0 (any volume of
//! at least `min` against nothing is an imbalance). Integer arithmetic.
//!
//! **No trade data:** a bar whose bucket has no counted trades (history only) gets no cells;
//! the chart draws its candle instead.

use std::ops::Range;

use crate::ladder::{GROUPING_PRESETS, row_of};
use crate::profile::{Footprint, ProfileRow, ProfileRows};
use crate::qty::Qty;
use crate::timeframe::BarsView;

/// Imbalance thresholds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ImbalanceRule {
    /// Percent: 300 = three times the diagonal opposite.
    pub ratio_pct: u32,
    /// The imbalanced side must trade at least this much.
    pub min_volume: Qty,
}

impl Default for ImbalanceRule {
    fn default() -> Self {
        ImbalanceRule {
            ratio_pct: 300,
            min_volume: Qty(5),
        }
    }
}

/// A row's imbalances (both can hold at once).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Imbalance {
    pub buy: bool,
    pub sell: bool,
}

/// One cell: a price row of one bar.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// Row index (`floor(price / row_units)`).
    pub row: i64,
    pub buy: Qty,
    pub sell: Qty,
    /// Side not reported by the source.
    pub unknown: Qty,
    pub total: Qty,
    pub imbalance: Imbalance,
}

impl Cell {
    /// Buying minus selling (unknown-side volume counts in neither).
    pub fn delta(&self) -> i64 {
        self.buy.0.saturating_sub(self.sell.0)
    }
}

/// One bar's cells: `cells[first..first + len]`, rows ascending.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BarCells {
    /// Index of the bar in the view.
    pub bar: usize,
    pub first: usize,
    pub len: usize,
    /// Index into the bar's cells.
    pub poc: Option<usize>,
    pub total: Qty,
    /// Bar delta (buy − sell).
    pub delta: i64,
}

/// The visible bars' footprints, rebuilt each frame into fixed buffers.
#[derive(Clone, Debug)]
pub struct FootprintFrame {
    pub cells: Vec<Cell>,
    pub bars: Vec<BarCells>,
    /// Price units per row.
    pub row_units: i64,
    /// Largest cell total on screen (scales cell shading).
    pub max_cell: Qty,
    /// Some bar had more rows or cells than the buffers hold: it shows as a candle.
    pub truncated: bool,
    /// Bars in the range that had trade data.
    pub with_data: usize,
    scratch: ProfileRows,
}

impl FootprintFrame {
    /// Buffers for `bars` bars, `cells` cells in all, and `rows` rows in one bar.
    pub fn with_capacity(bars: usize, cells: usize, rows: usize) -> FootprintFrame {
        FootprintFrame {
            cells: Vec::with_capacity(cells),
            bars: Vec::with_capacity(bars),
            row_units: 1,
            max_cell: Qty::ZERO,
            truncated: false,
            with_data: 0,
            scratch: ProfileRows::with_capacity(rows),
        }
    }

    /// Cells of `b`.
    pub fn cells_of(&self, b: &BarCells) -> &[Cell] {
        &self.cells[b.first..b.first + b.len]
    }

    /// Build the footprints of bars `range` of `view` from the store's per-period
    /// footprint `fp`, in rows of `row_units` price units. Never allocates: a bar that would
    /// not fit the buffers is skipped (`truncated`) and shows as a candle.
    pub fn build(
        &mut self,
        fp: &Footprint,
        view: BarsView<'_>,
        range: Range<usize>,
        row_units: i64,
        rule: ImbalanceRule,
    ) {
        let u = row_units.max(1);
        self.cells.clear();
        self.bars.clear();
        self.row_units = u;
        self.max_cell = Qty::ZERO;
        self.truncated = false;
        self.with_data = 0;
        let row_cap = self.scratch.rows.capacity();
        for i in range.start..range.end.min(view.len()) {
            if self.bars.len() == self.bars.capacity() {
                self.truncated = true;
                break;
            }
            let bar = view.bars[i];
            let (from, to) = (bar.time, view.end(i));
            // The bar's rows (from its range) must fit the scratch buffer: checked first, so
            // `ProfileRows::build` never grows it.
            let rows = row_of(bar.high, u) - row_of(bar.low, u) + 1;
            if fp.range(from, to).next().is_none() {
                continue; // no counted trades: a candle
            }
            self.with_data += 1;
            if rows > row_cap as i64 || rows <= 0 {
                self.truncated = true;
                continue;
            }
            self.scratch.build(u, 70, || fp.range(from, to));
            let n = self.scratch.rows.len();
            if n > row_cap || self.cells.len() + n > self.cells.capacity() {
                // A trade outside the bar's range (never, for counted trades) or out of cells.
                self.truncated = true;
                continue;
            }
            let first = self.cells.len();
            let rows = &self.scratch.rows;
            let (mut buy, mut sell) = (0i64, 0i64);
            for (k, r) in rows.iter().enumerate() {
                let imbalance = imbalance_at(rows, k, rule);
                buy = buy.saturating_add(r.volume.buy.0);
                sell = sell.saturating_add(r.volume.sell.0);
                self.max_cell = self.max_cell.max(r.total);
                self.cells.push(Cell {
                    row: self.scratch.base_row + k as i64,
                    buy: r.volume.buy,
                    sell: r.volume.sell,
                    unknown: r.volume.unknown,
                    total: r.total,
                    imbalance,
                });
            }
            self.bars.push(BarCells {
                bar: i,
                first,
                len: n,
                poc: self.scratch.poc,
                total: self.scratch.total,
                delta: buy.saturating_sub(sell),
            });
        }
    }
}

/// The diagonal imbalances of row `k` of `rows` (ascending, consecutive rows).
pub fn imbalance_at(rows: &[ProfileRow], k: usize, rule: ImbalanceRule) -> Imbalance {
    let ratio = i128::from(rule.ratio_pct);
    let beats = |own: Qty, other: Qty| {
        own.0 > 0
            && own >= rule.min_volume
            && 100 * i128::from(own.0) >= ratio * i128::from(other.0)
    };
    let r = rows[k].volume;
    let below = k.checked_sub(1).map_or(Qty::ZERO, |b| rows[b].volume.sell);
    let above = rows.get(k + 1).map_or(Qty::ZERO, |a| a.volume.buy);
    Imbalance {
        buy: beats(r.buy, below),
        sell: beats(r.sell, above),
    }
}

/// The smallest grouping, at least `wanted`, whose rows are at least `min_px` tall at
/// `px_per_unit` (price units to pixels): `wanted`, else the DOM presets, then 50, 100, 250
/// and their multiples of 10. No allocation.
pub fn fit_grouping(wanted: u32, tick: i64, px_per_unit: f64, min_px: f64) -> u32 {
    let fits = |g: u32| (g as f64) * (tick.max(1) as f64) * px_per_unit >= min_px;
    let wanted = wanted.max(1);
    if fits(wanted) {
        return wanted;
    }
    if let Some(g) = GROUPING_PRESETS
        .iter()
        .copied()
        .find(|&g| g > wanted && fits(g))
    {
        return g;
    }
    // 50, 100, 250, 500, 1000, 2500, …
    let mut pow = 1u32;
    loop {
        for m in [50u32, 100, 250] {
            let g = m.saturating_mul(pow);
            if fits(g) || g == u32::MAX {
                return g;
            }
        }
        pow = pow.saturating_mul(10);
    }
}

#[cfg(test)]
mod tests;
