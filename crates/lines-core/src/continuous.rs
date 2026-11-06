//! Continuous series for display (decision 0013).
//!
//! Stored bars are always per contract and never adjusted. A continuous series is built on
//! demand from the stored segments:
//! - [`Adjustment::None`] (default): each bar is the contract that was front month at that
//!   time, at actually traded prices.
//! - [`Adjustment::Difference`]: earlier segments are shifted by the price gap at each later
//!   roll, so the series is continuous at rolls. Shifts are integer price units, so adjusted
//!   prices stay exact and on the tick grid (decision 0002).
//!
//! Ratio adjustment is deliberately not offered: it produces off-tick prices.

use crate::bars::Bar;
use crate::instrument::ContractKey;
use crate::price::Price;
use crate::time::UtcMillis;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Adjustment {
    #[default]
    None,
    Difference,
}

/// One contract's stored bars and the time from which it is front month.
#[derive(Copy, Clone, Debug)]
pub struct Segment<'a> {
    pub contract: ContractKey,
    /// Front month from this instant (inclusive) until the next segment's `from`.
    pub from: UtcMillis,
    /// This contract's stored bars (sorted by time); bars outside its window are ignored for
    /// output but used to measure the roll gap.
    pub bars: &'a [Bar],
}

/// A roll marker in the output.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RollMarker {
    /// Index of the first output bar of the new contract.
    pub index: usize,
    pub from: ContractKey,
    pub to: ContractKey,
    /// New close minus old close at the roll, in price units (`None` if no bar of the new
    /// contract overlaps the old one before the roll, so the gap is unknown).
    pub gap: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Continuous {
    pub bars: Vec<Bar>,
    pub rolls: Vec<RollMarker>,
}

fn shift(b: &Bar, by: i64) -> Bar {
    Bar {
        open: Price(b.open.0 + by),
        high: Price(b.high.0 + by),
        low: Price(b.low.0 + by),
        close: Price(b.close.0 + by),
        ..*b
    }
}

/// Gap at a roll from `old` to `new` at time `t`: compare the closes of the last bar of each
/// contract *before* `t` that share the same bar time.
fn roll_gap(old: &[Bar], new: &[Bar], t: UtcMillis) -> Option<i64> {
    let old_before = &old[..old.partition_point(|b| b.time < t)];
    let new_before = &new[..new.partition_point(|b| b.time < t)];
    old_before.iter().rev().find_map(|o| {
        new_before
            .binary_search_by_key(&o.time, |b| b.time)
            .ok()
            .map(|i| new_before[i].close.0 - o.close.0)
    })
}

/// Build a continuous series from segments in roll order (sorted by `from`).
pub fn build(segments: &[Segment<'_>], adjustment: Adjustment) -> Continuous {
    let mut out = Continuous::default();
    let n = segments.len();
    // Cumulative shift for each segment: sum of the gaps of all *later* rolls.
    let mut gaps = vec![None; n];
    for i in 1..n {
        gaps[i] = roll_gap(segments[i - 1].bars, segments[i].bars, segments[i].from);
    }
    let mut shifts = vec![0i64; n];
    if adjustment == Adjustment::Difference {
        for i in (0..n.saturating_sub(1)).rev() {
            shifts[i] = shifts[i + 1] + gaps[i + 1].unwrap_or(0);
        }
    }
    for (i, seg) in segments.iter().enumerate() {
        let until = segments.get(i + 1).map(|s| s.from);
        if i > 0 {
            out.rolls.push(RollMarker {
                index: out.bars.len(),
                from: segments[i - 1].contract,
                to: seg.contract,
                gap: gaps[i],
            });
        }
        out.bars.extend(
            seg.bars
                .iter()
                .filter(|b| b.time >= seg.from && until.is_none_or(|u| b.time < u))
                .map(|b| shift(b, shifts[i])),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::{Month, ProductId};
    use crate::qty::Qty;

    const M: i64 = 60_000;

    fn bar(t: i64, c: i64) -> Bar {
        Bar {
            time: UtcMillis(t * M),
            open: Price(c),
            high: Price(c + 25),
            low: Price(c - 25),
            close: Price(c),
            volume: Qty(1),
        }
    }
    fn key(m: u8) -> ContractKey {
        ContractKey::monthly(ProductId(1), 2025, Month::new(m).unwrap())
    }

    #[test]
    fn unadjusted_and_difference() {
        // Old contract trades 0..10 at 1000; new contract trades 5..15 at 1100 (gap +100).
        let old: Vec<_> = (0..10).map(|t| bar(t, 1000)).collect();
        let new: Vec<_> = (5..15).map(|t| bar(t, 1100)).collect();
        let segs = [
            Segment {
                contract: key(9),
                from: UtcMillis(0),
                bars: &old,
            },
            Segment {
                contract: key(12),
                from: UtcMillis(8 * M),
                bars: &new,
            },
        ];

        let raw = build(&segs, Adjustment::None);
        assert_eq!(raw.bars.len(), 8 + 7);
        assert_eq!(raw.bars[7].close, Price(1000));
        assert_eq!(raw.bars[8].close, Price(1100));
        assert_eq!(
            raw.rolls,
            [RollMarker {
                index: 8,
                from: key(9),
                to: key(12),
                gap: Some(100)
            }]
        );

        let adj = build(&segs, Adjustment::Difference);
        assert_eq!(
            adj.bars[0].close,
            Price(1100),
            "old segment shifted up by the gap"
        );
        assert_eq!(adj.bars[7].high, Price(1125));
        assert_eq!(adj.bars[8].close, Price(1100), "latest contract unadjusted");
        // Inputs untouched: display-only.
        assert_eq!(old[0].close, Price(1000));
    }

    #[test]
    fn adjustment_stays_on_tick_grid_and_accumulates() {
        let tick = 25;
        let a: Vec<_> = (0..10).map(|t| bar(t, 1000)).collect();
        let b: Vec<_> = (0..20).map(|t| bar(t, 1050)).collect();
        let c: Vec<_> = (0..30).map(|t| bar(t, 1125)).collect();
        let segs = [
            Segment {
                contract: key(3),
                from: UtcMillis(0),
                bars: &a,
            },
            Segment {
                contract: key(6),
                from: UtcMillis(10 * M),
                bars: &b,
            },
            Segment {
                contract: key(9),
                from: UtcMillis(20 * M),
                bars: &c,
            },
        ];
        let adj = build(&segs, Adjustment::Difference);
        assert_eq!(adj.bars[0].close, Price(1000 + 50 + 75));
        assert_eq!(adj.bars[10].close, Price(1050 + 75));
        assert_eq!(adj.bars[20].close, Price(1125));
        assert!(
            adj.bars
                .iter()
                .all(|b| b.close.0 % tick == 0 && b.high.0 % tick == 0)
        );
    }

    #[test]
    fn unknown_gap_is_reported_not_guessed() {
        let a: Vec<_> = (0..5).map(|t| bar(t, 1000)).collect();
        let b: Vec<_> = (6..9).map(|t| bar(t, 1100)).collect(); // no overlap before the roll
        let segs = [
            Segment {
                contract: key(3),
                from: UtcMillis(0),
                bars: &a,
            },
            Segment {
                contract: key(6),
                from: UtcMillis(6 * M),
                bars: &b,
            },
        ];
        let adj = build(&segs, Adjustment::Difference);
        assert_eq!(adj.rolls[0].gap, None);
        assert_eq!(adj.bars[0].close, Price(1000), "no gap known: no shift");
    }
}
