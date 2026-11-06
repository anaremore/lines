//! The DOM price trail: where price just traded, with volume as intensity (a "motion blur"
//! beside the price column). Pure and allocation-free after construction.
//!
//! Per price row of the DOM's grid (`row = floor(price / (grouping × tick))`, as
//! `crate::ladder`) it keeps two decaying accumulators: volume bought by aggressors and volume
//! sold by them. A trade without an aggressor counts by tick direction (up from the previous
//! price: buy; down: sell; unchanged: as the previous trade; the very first: half each).
//!
//! - **Decay** is exponential on the *market clock* (times passed in by the caller, from the
//!   feed), never the wall clock, so replays and screenshots are deterministic. The half-life
//!   is a setting ([`MIN_HALF_LIFE_MS`]..=[`MAX_HALF_LIFE_MS`], default
//!   [`DEFAULT_HALF_LIFE_MS`]). Decay is applied lazily: values are stored as of `at` and
//!   scaled when the clock moves.
//! - **Storage** is a fixed ring of [`ROWS`] rows around the newest trade's row; a trade
//!   outside it recenters the window on that row, clearing only the rows that left. No
//!   allocation after [`Trail::new`].
//! - **Intensity** is decayed volume ÷ a *stable* scale: a slow-moving estimate of a high
//!   percentile of the active rows' volume (it rises in about a second, falls over tens of
//!   seconds, and holds still while nothing trades), never the instantaneous maximum, so one big print doesn't make every other row
//!   flicker dark. Opacity is `sqrt(volume / scale)`, capped at [`MAX_ALPHA`].
//! - **Animation** is needed only while some row is above [`VISIBLE`] opacity: after about
//!   ten half-lives without trades nothing is, and the DOM stops asking for frames.

use crate::price::Price;
use crate::qty::Qty;
use crate::tape::Aggressor;
use crate::time::UtcMillis;

/// Rows kept around the newest trade (a power of two: a slot is `row mod ROWS`).
pub const ROWS: usize = 512;
pub const MIN_HALF_LIFE_MS: u32 = 500;
pub const MAX_HALF_LIFE_MS: u32 = 5_000;
pub const DEFAULT_HALF_LIFE_MS: u32 = 1_500;
/// Half-life presets the DOM command cycles through.
pub const HALF_LIFE_PRESETS_MS: [u32; 5] = [500, 1_000, 1_500, 3_000, 5_000];
/// The strongest a trail cell gets (opacity).
pub const MAX_ALPHA: f32 = 0.6;
/// Below this opacity a cell counts as gone (no more frames for it): a row at the scale gets
/// there in under ten half-lives.
pub const VISIBLE: f32 = 0.03;

/// How fast the intensity scale follows its target, in ms (time constants): up, down.
const SCALE_RISE_MS: f64 = 1_000.0;
const SCALE_FALL_MS: f64 = 30_000.0;
/// The percentile of active rows' volume the scale tracks.
const SCALE_PERCENTILE: f64 = 0.9;
/// Rows below this share of the scale don't count as active for the percentile.
const ACTIVE_SHARE: f64 = 1e-3;

/// Which way the last price moved (for the chevron on the newest row).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
}

/// One row as painted: opacity (0 = nothing) and the aggressor balance in `-1..=1`
/// (`1` all bought, `-1` all sold).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Cell {
    pub alpha: f32,
    pub net: f32,
}

#[derive(Clone, Debug)]
pub struct Trail {
    half_life_ms: u32,
    /// Ticks × grouping per row, in price units.
    units: i64,
    buy: Box<[f64; ROWS]>,
    sell: Box<[f64; ROWS]>,
    /// Scratch for the percentile (allocated once).
    scratch: Box<[f64; ROWS]>,
    /// The window holds rows `center - ROWS/2 .. center + ROWS/2`; `None` before any trade.
    center: Option<i64>,
    /// The market time the stored values are decayed to.
    at: Option<i64>,
    /// Intensity scale (quantity units, as `f64`).
    scale: f64,
    last_price: Option<Price>,
    /// The previous trade's aggressor side by tick rule (for unchanged prices).
    last_side: Option<Aggressor>,
    last_move: Option<Move>,
    /// Trades arrived since the scale was last re-targeted (with no trades it holds still,
    /// so a fading row fades at exactly the half-life).
    rescale: bool,
}

impl Trail {
    /// An empty trail for rows `units` price units tall (grouping × tick).
    pub fn new(units: i64, half_life_ms: u32) -> Trail {
        Trail {
            half_life_ms: clamp_half_life(half_life_ms),
            units: units.max(1),
            buy: Box::new([0.0; ROWS]),
            sell: Box::new([0.0; ROWS]),
            scratch: Box::new([0.0; ROWS]),
            center: None,
            at: None,
            scale: 0.0,
            last_price: None,
            last_side: None,
            last_move: None,
            rescale: false,
        }
    }

    pub fn half_life_ms(&self) -> u32 {
        self.half_life_ms
    }

    pub fn set_half_life(&mut self, ms: u32) {
        self.half_life_ms = clamp_half_life(ms);
    }

    pub fn units(&self) -> i64 {
        self.units
    }

    /// Forget everything (a new market or grouping). Keeps the half-life. No allocation.
    pub fn clear(&mut self, units: i64) {
        self.units = units.max(1);
        self.buy.fill(0.0);
        self.sell.fill(0.0);
        self.center = None;
        self.at = None;
        self.scale = 0.0;
        self.last_price = None;
        self.last_side = None;
        self.last_move = None;
        self.rescale = false;
    }

    pub fn row_of(&self, price: Price) -> i64 {
        price.0.div_euclid(self.units)
    }

    fn slot(row: i64) -> usize {
        row.rem_euclid(ROWS as i64) as usize
    }

    fn in_window(&self, row: i64) -> bool {
        self.center
            .is_some_and(|c| row >= c - (ROWS / 2) as i64 && row < c + (ROWS / 2) as i64)
    }

    /// Move the stored values to market time `now` (never backwards).
    pub fn advance(&mut self, now: UtcMillis) {
        let Some(at) = self.at else {
            self.at = Some(now.0);
            return;
        };
        if now.0 <= at {
            return;
        }
        let dt = (now.0 - at) as f64;
        // The scale follows the rows as the trades left them (before this step's decay).
        if std::mem::take(&mut self.rescale) {
            self.update_scale(dt);
        }
        let k = decay(dt, self.half_life_ms);
        for v in self.buy.iter_mut().chain(self.sell.iter_mut()) {
            *v *= k;
        }
        self.at = Some(now.0);
    }

    /// Add one counted trade (in feed order).
    pub fn trade(&mut self, time: UtcMillis, price: Price, qty: Qty, aggressor: Option<Aggressor>) {
        if qty.0 <= 0 {
            return;
        }
        self.advance(time);
        let row = self.row_of(price);
        if !self.in_window(row) {
            self.recenter(row);
        }
        // A trade older than the stored values counts as already decayed by its age.
        let age = self.at.map_or(0.0, |a| (a - time.0).max(0) as f64);
        let q = qty.0 as f64 * decay(age, self.half_life_ms);
        let side = aggressor.or_else(|| self.tick_side(price));
        let s = Self::slot(row);
        match side {
            Some(Aggressor::Buy) => self.buy[s] += q,
            Some(Aggressor::Sell) => self.sell[s] += q,
            None => {
                self.buy[s] += q * 0.5;
                self.sell[s] += q * 0.5;
            }
        }
        if let Some(prev) = self.last_price {
            if price.0 > prev.0 {
                self.last_move = Some(Move::Up);
            } else if price.0 < prev.0 {
                self.last_move = Some(Move::Down);
            }
        }
        self.last_price = Some(price);
        self.rescale = true;
        if side.is_some() {
            self.last_side = side;
        }
        // The first trade sets the scale; the percentile takes over from there.
        if self.scale <= 0.0 {
            self.scale = self.buy[s] + self.sell[s];
        }
    }

    /// Tick rule for trades without an aggressor.
    fn tick_side(&self, price: Price) -> Option<Aggressor> {
        match self.last_price {
            Some(p) if price.0 > p.0 => Some(Aggressor::Buy),
            Some(p) if price.0 < p.0 => Some(Aggressor::Sell),
            Some(_) => self.last_side,
            None => None,
        }
    }

    /// Center the window on `row`, clearing the rows that leave it (no allocation).
    fn recenter(&mut self, row: i64) {
        let half = (ROWS / 2) as i64;
        let (lo, hi) = (row - half, row + half);
        match self.center {
            Some(c) if (row - c).abs() < ROWS as i64 => {
                // Rows of the old window that the new one doesn't hold.
                for old in (c - half)..(c + half) {
                    if old < lo || old >= hi {
                        let s = Self::slot(old);
                        self.buy[s] = 0.0;
                        self.sell[s] = 0.0;
                    }
                }
            }
            _ => {
                self.buy.fill(0.0);
                self.sell.fill(0.0);
            }
        }
        self.center = Some(row);
    }

    /// The scale follows a high percentile of the active rows: quickly up, slowly down.
    fn update_scale(&mut self, dt: f64) {
        let floor = self.scale * ACTIVE_SHARE;
        let mut n = 0;
        for s in 0..ROWS {
            let v = self.buy[s] + self.sell[s];
            if v > floor && v > 0.0 {
                self.scratch[n] = v;
                n += 1;
            }
        }
        if n == 0 {
            return;
        }
        let k = ((n - 1) as f64 * SCALE_PERCENTILE).round() as usize;
        let (_, target, _) = self.scratch[..n].select_nth_unstable_by(k, f64::total_cmp);
        let target = *target;
        let tau = if target > self.scale {
            SCALE_RISE_MS
        } else {
            SCALE_FALL_MS
        };
        self.scale += (target - self.scale) * (1.0 - (-dt / tau).exp());
    }

    /// Row `row` as painted at the stored time (call [`Trail::advance`] first).
    pub fn cell(&self, row: i64) -> Cell {
        if !self.in_window(row) || self.scale <= 0.0 {
            return Cell::default();
        }
        let s = Self::slot(row);
        let (b, v) = (self.buy[s], self.sell[s]);
        let total = b + v;
        if total <= 0.0 {
            return Cell::default();
        }
        let alpha = ((total / self.scale).sqrt() as f32 * MAX_ALPHA).min(MAX_ALPHA);
        Cell {
            alpha: if alpha < VISIBLE { 0.0 } else { alpha },
            net: ((b - v) / total) as f32,
        }
    }

    /// The newest trade's row and which way price last moved (the chevron).
    pub fn newest(&self) -> Option<(i64, Option<Move>)> {
        Some((self.row_of(self.last_price?), self.last_move))
    }

    /// Advance to `now` and say whether any row is still drawn: the DOM asks for frames
    /// only while this is true. Exactly what [`Trail::cell`] would paint (same threshold).
    pub fn needs_animation(&mut self, now: UtcMillis) -> bool {
        self.advance(now);
        if self.scale <= 0.0 {
            return false;
        }
        let limit = self.scale * f64::from(VISIBLE / MAX_ALPHA).powi(2);
        (0..ROWS).any(|s| self.buy[s] + self.sell[s] >= limit)
    }
}

fn clamp_half_life(ms: u32) -> u32 {
    ms.clamp(MIN_HALF_LIFE_MS, MAX_HALF_LIFE_MS)
}

/// `0.5 ^ (dt / half_life)`.
fn decay(dt_ms: f64, half_life_ms: u32) -> f64 {
    (-(dt_ms / f64::from(half_life_ms)) * std::f64::consts::LN_2).exp()
}

/// The next half-life preset after `ms` (wrapping); custom values rejoin the presets.
pub fn next_half_life(ms: u32) -> u32 {
    HALF_LIFE_PRESETS_MS
        .iter()
        .copied()
        .find(|&p| p > ms)
        .unwrap_or(HALF_LIFE_PRESETS_MS[0])
}

#[cfg(test)]
mod tests;
