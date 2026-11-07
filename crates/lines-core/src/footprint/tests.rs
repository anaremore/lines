use super::*;
use crate::bars::BarSeries;
use crate::price::Price;
use crate::profile::{ProfileRow, SideVolume};
use crate::tape::Aggressor;
use crate::time::UtcMillis;
use crate::timeframe::{Derived, Timeframe};

const TICK: i64 = 25;
const MIN: i64 = 60_000;

/// A deterministic tape: `n` trades over `minutes`, prices wandering on the tick grid,
/// sides mixed (a few with no side).
fn tape(minutes: i64, n: i64) -> Vec<(UtcMillis, Price, Qty, Option<Aggressor>)> {
    let mut p = 600_000i64;
    let mut x = 12345u64;
    (0..n)
        .map(|i| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let step = ((x >> 33) % 5) as i64 - 2;
            p += step * TICK;
            let side = match (x >> 40) % 7 {
                0 => None,
                k if k % 2 == 0 => Some(Aggressor::Buy),
                _ => Some(Aggressor::Sell),
            };
            let t = UtcMillis(i * minutes * MIN / n);
            (t, Price(p), Qty(1 + ((x >> 50) % 9) as i64), side)
        })
        .collect()
}

fn store(
    trades: &[(UtcMillis, Price, Qty, Option<Aggressor>)],
    period: i64,
) -> (Footprint, BarSeries) {
    let mut fp = Footprint::new(period, 10_000);
    let mut bars = BarSeries::new(period, 10_000);
    for &(t, p, q, a) in trades {
        fp.add(t, p, q, a);
        bars.apply_trade(t, p, q);
    }
    (fp, bars)
}

fn frame() -> FootprintFrame {
    FootprintFrame::with_capacity(512, 50_000, 2_048)
}

/// Every bar's cells as (row, buy, sell, unknown) plus its POC row and delta.
/// A bar's cells as (row, buy, sell, unknown), its POC row and its delta.
type BarSnapshot = (Vec<(i64, i64, i64, i64)>, Option<i64>, i64);

fn snapshot(f: &FootprintFrame) -> Vec<BarSnapshot> {
    f.bars
        .iter()
        .map(|b| {
            let cells = f.cells_of(b);
            (
                cells
                    .iter()
                    .map(|c| (c.row, c.buy.0, c.sell.0, c.unknown.0))
                    .collect(),
                b.poc.map(|i| cells[i].row),
                b.delta,
            )
        })
        .collect()
}

#[test]
fn a_higher_timeframe_bar_is_the_exact_merge_of_its_minutes() {
    let trades = tape(60, 3_000);
    let (fp1, bars1) = store(&trades, MIN);
    for tf in [
        Timeframe::Minutes(5),
        Timeframe::Minutes(15),
        Timeframe::Hours(1),
    ] {
        let mut d = Derived::new(tf, 1_000);
        d.update(&bars1, None); // no calendar: buckets align to UTC
        let view = d.view();
        let mut f = frame();
        f.build(
            &fp1,
            view,
            0..view.len(),
            2 * TICK,
            ImbalanceRule::default(),
        );
        assert_eq!(f.bars.len(), view.len());
        // The same trades counted directly at the higher period give the same cells.
        let period = tf.nominal_ms();
        let (fp_n, bars_n) = store(&trades, period);
        let mut g = frame();
        let direct = crate::timeframe::BarsView::fixed(bars_n.bars(), period);
        g.build(
            &fp_n,
            direct,
            0..direct.len(),
            2 * TICK,
            ImbalanceRule::default(),
        );
        assert_eq!(snapshot(&f), snapshot(&g), "{tf}");
        // And each bar's volume is the sum of its minutes' (nothing lost or doubled).
        let mut one = frame();
        let v1 = crate::timeframe::BarsView::fixed(bars1.bars(), MIN);
        one.build(&fp1, v1, 0..v1.len(), 2 * TICK, ImbalanceRule::default());
        let total: i64 = f.bars.iter().map(|b| b.total.0).sum();
        let total1: i64 = one.bars.iter().map(|b| b.total.0).sum();
        let traded: i64 = trades.iter().map(|t| t.2.0).sum();
        assert_eq!((total, total1), (traded, traded), "{tf}");
    }
}

#[test]
fn bars_without_trades_get_no_cells() {
    let trades = tape(10, 200);
    let (fp, mut bars) = store(&trades, MIN);
    // A history bar before the trades (no footprint).
    bars.upsert(crate::bars::Bar {
        time: UtcMillis(-MIN),
        open: Price(600_000),
        high: Price(600_100),
        low: Price(599_900),
        close: Price(600_050),
        volume: Qty(40),
    });
    let view = crate::timeframe::BarsView::fixed(bars.bars(), MIN);
    let mut f = frame();
    f.build(&fp, view, 0..view.len(), TICK, ImbalanceRule::default());
    assert_eq!(view.len(), 11);
    assert_eq!(f.with_data, 10);
    assert!(
        f.bars.iter().all(|b| b.bar != 0),
        "the history bar is a candle"
    );
}

fn rows(v: &[(i64, i64)]) -> Vec<ProfileRow> {
    v.iter()
        .map(|&(buy, sell)| {
            let volume = SideVolume {
                buy: Qty(buy),
                sell: Qty(sell),
                unknown: Qty::ZERO,
            };
            ProfileRow {
                volume,
                total: volume.total(),
            }
        })
        .collect()
}

#[test]
fn imbalances_compare_diagonally() {
    let rule = ImbalanceRule {
        ratio_pct: 300,
        min_volume: Qty(5),
    };
    // Rows low to high as (buy, sell).
    let r = rows(&[(2, 10), (30, 4), (6, 40), (5, 0), (1, 1)]);
    let at = |k| imbalance_at(&r, k, rule);
    // Row 1: buy 30 vs sell 10 one row below: 300% exactly → buy imbalance. Sell 4 < min.
    assert_eq!(
        at(1),
        Imbalance {
            buy: true,
            sell: false
        }
    );
    // Row 0: sell 10 vs buy 30 above: no. Buy 2 vs nothing below, but under the minimum.
    assert_eq!(at(0), Imbalance::default());
    // Row 2: buy 6 vs sell 4 below: 150%, no. Sell 40 vs buy 5 above: 800% → sell.
    assert_eq!(
        at(2),
        Imbalance {
            buy: false,
            sell: true
        }
    );
    // Row 3: buy 5 vs sell 40 below: no. Sell 0: never.
    assert_eq!(at(3), Imbalance::default());
    // Just under the ratio: 299%.
    let r = rows(&[(0, 100), (299, 0)]);
    assert!(!imbalance_at(&r, 1, rule).buy);
    let r = rows(&[(0, 100), (300, 0)]);
    assert!(imbalance_at(&r, 1, rule).buy);
    // An empty opposite counts as 0: at least `min` against nothing is an imbalance.
    let r = rows(&[(0, 0), (5, 0)]);
    assert!(imbalance_at(&r, 1, rule).buy);
    let r = rows(&[(0, 7)]);
    assert!(
        imbalance_at(&r, 0, rule).sell,
        "top row's sell vs nothing above"
    );
}

#[test]
fn poc_ties_follow_the_profile_rule() {
    // One bar, rows 0..=4 with totals 5, 9, 1, 9, 2: tie between rows 1 and 3, both one
    // row off the middle (2): the lower wins, as in the volume profile.
    let mut fp = Footprint::new(MIN, 10);
    let mut bars = BarSeries::new(MIN, 10);
    for (k, q) in [(0i64, 5i64), (1, 9), (2, 1), (3, 9), (4, 2)] {
        let p = Price(600_000 + k * TICK);
        fp.add(UtcMillis(k), p, Qty(q), Some(Aggressor::Buy));
        bars.apply_trade(UtcMillis(k), p, Qty(q));
    }
    let view = crate::timeframe::BarsView::fixed(bars.bars(), MIN);
    let mut f = frame();
    f.build(&fp, view, 0..1, TICK, ImbalanceRule::default());
    let b = f.bars[0];
    assert_eq!(f.cells_of(&b)[b.poc.unwrap()].row, 600_000 / TICK + 1);
    // The same rows through the profile: the same POC.
    let mut p = ProfileRows::with_capacity(16);
    p.build(TICK, 70, || fp.range(UtcMillis(0), UtcMillis(MIN)));
    assert_eq!(p.poc, b.poc);
}

#[test]
fn grouping_coarsens_until_rows_are_readable() {
    // ES: 25 units a tick. 0.4 px per unit = 10 px a tick: grouping 1 is readable.
    assert_eq!(fit_grouping(1, TICK, 0.4, 9.0), 1);
    // 0.2 px per unit = 5 px a tick: 2 ticks = 10 px.
    assert_eq!(fit_grouping(1, TICK, 0.2, 9.0), 2);
    // A wanted grouping is never made finer.
    assert_eq!(fit_grouping(5, TICK, 0.4, 9.0), 5);
    // Far out: presets, then 50, 100, 250, 500, …
    assert_eq!(fit_grouping(1, TICK, 0.01, 9.0), 50); // 50 ticks = 12.5 px
    assert_eq!(fit_grouping(1, TICK, 0.001, 9.0), 500);
    assert_eq!(fit_grouping(1, TICK, 0.0005, 9.0), 1_000);
}

/// The row guard uses the trades' own prices: a bar whose high/low a history snapshot
/// narrowed below what was traded still never grows the scratch buffer.
#[test]
fn trades_outside_the_bars_range_never_grow_the_buffer() {
    let mut fp = Footprint::new(MIN, 10);
    for k in 0..10i64 {
        fp.add(
            UtcMillis(k),
            Price(600_000 + k * TICK),
            Qty(1),
            Some(Aggressor::Buy),
        );
    }
    // The bar says one tick wide; the trades span ten.
    let bar = crate::bars::Bar {
        time: UtcMillis(0),
        open: Price(600_000),
        high: Price(600_000),
        low: Price(600_000),
        close: Price(600_000),
        volume: Qty(10),
    };
    let bars = [bar];
    let view = crate::timeframe::BarsView::fixed(&bars, MIN);
    let mut f = FootprintFrame::with_capacity(4, 100, 4);
    f.build(&fp, view, 0..1, TICK, ImbalanceRule::default());
    assert!(
        f.truncated && f.bars.is_empty(),
        "a candle, not a reallocation"
    );
    assert_eq!(f.with_data, 1);
    let mut g = FootprintFrame::with_capacity(4, 100, 16);
    g.build(&fp, view, 0..1, TICK, ImbalanceRule::default());
    assert_eq!(g.cells_of(&g.bars[0]).len(), 10, "every traded row");
}

#[test]
fn buffers_bound_the_work() {
    let trades = tape(60, 3_000);
    let (fp, bars) = store(&trades, MIN);
    let view = crate::timeframe::BarsView::fixed(bars.bars(), MIN);
    // Room for 5 bars only.
    let mut f = FootprintFrame::with_capacity(5, 50_000, 2_048);
    f.build(&fp, view, 0..view.len(), TICK, ImbalanceRule::default());
    assert_eq!(f.bars.len(), 5);
    assert!(f.truncated);
    // Too few rows for a bar: that bar is skipped, never a reallocation.
    let mut g = FootprintFrame::with_capacity(100, 50_000, 2);
    let caps = (g.cells.capacity(), g.bars.capacity());
    g.build(&fp, view, 0..view.len(), TICK, ImbalanceRule::default());
    assert!(g.truncated);
    assert_eq!((g.cells.capacity(), g.bars.capacity()), caps);
    assert!(g.bars.len() < view.len());
}

/// Tick bars (roadmap 9b) carry their own trades' volume per price: each bar's cells equal
/// those of a footprint that counted exactly that bar's trades.
#[test]
fn a_tick_bar_footprint_counts_its_own_trades() {
    use crate::ticks::{TickBars, TickReset};
    let trades = tape(30, 4_000);
    let n = 233usize;
    let mut tb = TickBars::new(n as u32, TickReset::Continuous, None, 1_000);
    // The reference: the same trades, bar k's filed under minute k.
    let mut direct = Footprint::new(MIN, 10_000);
    let mut minutes = BarSeries::new(MIN, 10_000);
    for (k, &(t, p, q, a)) in trades.iter().enumerate() {
        tb.push(t, p, q, a);
        let slot = UtcMillis((k / n) as i64 * MIN);
        direct.add(slot, p, q, a);
        minutes.apply_trade(slot, p, q);
    }
    let view = tb.view();
    assert_eq!(view.len(), trades.len().div_ceil(n));
    let mut f = frame();
    // The store's own footprint is ignored for tick bars: pass an empty one.
    f.build(
        &Footprint::new(MIN, 10),
        view,
        0..view.len(),
        TICK,
        ImbalanceRule::default(),
    );
    let mut g = frame();
    let one = crate::timeframe::BarsView::fixed(minutes.bars(), MIN);
    g.build(&direct, one, 0..one.len(), TICK, ImbalanceRule::default());
    assert_eq!(f.with_data, view.len());
    assert_eq!(snapshot(&f), snapshot(&g));
}
