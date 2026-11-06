//! Positions with exact P&L: open quantity is kept as FIFO lots (price, quantity), so realised
//! P&L is an exact integer of price units × quantity units, never an average rounded down.

use std::collections::VecDeque;

use lines_core::{Price, Qty};
use serde::{Deserialize, Serialize};

use crate::types::Side;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lot {
    /// Price units.
    pub price: i64,
    /// Signed: positive long, negative short (all lots of a position share the sign).
    pub qty: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub lots: VecDeque<Lot>,
}

impl Position {
    /// Signed quantity (quantity units).
    pub fn qty(&self) -> i64 {
        self.lots.iter().map(|l| l.qty).sum()
    }

    pub fn is_flat(&self) -> bool {
        self.lots.is_empty()
    }

    /// Average open price in price units (display only).
    pub fn avg_price(&self) -> Option<f64> {
        let q = self.qty();
        (q != 0).then(|| {
            self.lots
                .iter()
                .map(|l| l.price as i128 * l.qty as i128)
                .sum::<i128>() as f64
                / q as f64
        })
    }

    /// Apply a fill; returns the realised P&L in price units × quantity units.
    pub fn apply_fill(&mut self, side: Side, price: Price, qty: Qty) -> i128 {
        let mut left = qty.0 * side.sign();
        let mut realised = 0i128;
        while left != 0 {
            match self.lots.front_mut() {
                // Opposite sign: close FIFO.
                Some(lot) if lot.qty.signum() != left.signum() => {
                    let close = lot.qty.abs().min(left.abs());
                    // Long lot closed by a sell: (fill − lot) × qty; short: (lot − fill) × qty.
                    realised +=
                        (price.0 - lot.price) as i128 * close as i128 * lot.qty.signum() as i128;
                    lot.qty -= close * lot.qty.signum();
                    left -= close * left.signum();
                    if lot.qty == 0 {
                        self.lots.pop_front();
                    }
                }
                _ => {
                    self.lots.push_back(Lot {
                        price: price.0,
                        qty: left,
                    });
                    left = 0;
                }
            }
        }
        realised
    }

    /// Open P&L at `mark`, price units × quantity units.
    pub fn unrealised(&self, mark: Price) -> i128 {
        self.lots
            .iter()
            .map(|l| (mark.0 - l.price) as i128 * l.qty as i128)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_realised_is_exact_through_reversals() {
        let mut p = Position::default();
        assert_eq!(p.apply_fill(Side::Buy, Price(100), Qty(2)), 0);
        assert_eq!(p.apply_fill(Side::Buy, Price(103), Qty(1)), 0);
        assert_eq!(p.qty(), 3);
        assert_eq!(p.avg_price(), Some(101.0));
        assert_eq!(p.unrealised(Price(104)), 4 * 2 + 1);
        // Sell 4: closes 2@100 (+10), 1@103 (+2), opens 1 short @105.
        assert_eq!(p.apply_fill(Side::Sell, Price(105), Qty(4)), 10 + 2);
        assert_eq!(p.qty(), -1);
        assert_eq!(p.unrealised(Price(100)), 5);
        assert_eq!(p.apply_fill(Side::Buy, Price(107), Qty(1)), -2);
        assert!(p.is_flat());
    }
}
