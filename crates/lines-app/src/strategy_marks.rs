//! Strategy markers on the chart (decision 0025, phase c): a strategy's entries, exits and
//! skipped signals on the chart of its contract, with the reason on hover. A toggleable layer
//! (per chart, saved as the optional `ChartState.hide_strategy_marks`).
//!
//! - **Entries and exits** are the instance's own fills: fills (from `TradeState`'s ring of
//!   the core's `Event::Fill`s) of orders its decisions created (`DecisionRecord::result`
//!   `Ok(ids)`), on its account and contract. Replaying them in time order on its own
//!   position says which is which: a fill that grows the position is an entry, any other an
//!   exit. Fills of orders the core made for it (a Stop's or the daily loss's flatten) aren't
//!   its decisions' orders and get no marker.
//! - **Skipped signals** are decisions with no command, and commands the core refused, at
//!   their first level's price (a decision without a level has no price and no marker).
//! - **Shapes, not color alone:** a buy entry is an upward wedge under its price, a sell
//!   entry a downward wedge over it (up/down colors), an exit a hollow square (text color), a
//!   skip a small hollow circle (dim).
//! - **No allocation in prepare or paint:** marks are rebuilt only when their inputs move, and
//!   their positions go into a buffer reserved then.

use gpui::{App, BorderStyle, Bounds, Pixels, Window, point, px, quad, size};
use lines_core::drawings::TimeAxis;
use lines_core::timeframe::BarsView;
use lines_core::viewport::Viewport;
use lines_core::{ContractKey, Price, Product, UtcMillis};
use lines_trading::{FillEvent, Side};

use crate::strategy_panel::StrategyInfo;
use crate::theme::Theme;

/// What a marker shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MarkKind {
    /// A fill that opened or added: its side gives the wedge's direction.
    Entry(Side),
    /// A fill that reduced or closed.
    Exit,
    /// A decision with no command, or a refused command.
    Skip,
}

/// One marker.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub at: UtcMillis,
    pub price: Price,
    pub kind: MarkKind,
    /// The hover text: instance, what happened, why.
    pub text: String,
}

/// Fills kept for markers (the newest; older ones lose their markers).
pub const KEEP_FILLS: usize = 4096;

/// Build the markers for `contract` from the instances and the recent fills.
pub fn build(
    infos: &[StrategyInfo],
    fills: &[FillEvent],
    contract: ContractKey,
    product: &Product,
) -> Vec<Mark> {
    let mut out = Vec::new();
    for info in infos.iter().filter(|i| i.contract == contract) {
        // Order → the reason of the decision that created it.
        let mut reasons: Vec<(lines_trading::OrderId, &str)> = Vec::new();
        for d in &info.decisions {
            if let Some(Ok(ids)) = &d.result {
                reasons.extend(ids.iter().map(|&o| (o, d.decision.reason.as_str())));
            }
        }
        let mut pos: i64 = 0;
        for f in fills
            .iter()
            .filter(|f| f.account == info.account && f.contract == contract)
        {
            let Some(&(_, why)) = reasons.iter().find(|(o, _)| *o == f.order) else {
                continue;
            };
            let delta = if f.side == Side::Buy {
                f.qty.0
            } else {
                -f.qty.0
            };
            let grows = pos == 0 || (pos > 0) == (delta > 0);
            pos += delta;
            let side = if f.side == Side::Buy { "Buy" } else { "Sell" };
            let (kind, what) = if grows {
                (MarkKind::Entry(f.side), "entry")
            } else {
                (MarkKind::Exit, "exit")
            };
            out.push(Mark {
                at: f.at,
                price: f.price,
                kind,
                text: format!(
                    "{}: {what}, {side} {} @ {}. {why}",
                    info.label,
                    f.qty.0,
                    product.price.format::<32>(f.price).as_str()
                ),
            });
        }
        for d in &info.decisions {
            // Skipped: no orders came of it (no command, or the core refused it).
            let skipped = match (&d.decision.command, &d.result) {
                (_, Some(Ok(_))) => None,
                (None, _) => Some(d.decision.reason.clone()),
                (Some(_), Some(Err(r))) => {
                    Some(format!("{} (denied: {})", d.decision.reason, r.message))
                }
                _ => None,
            };
            let (Some(text), Some(level)) = (skipped, d.decision.levels.first()) else {
                continue;
            };
            out.push(Mark {
                at: d.at,
                price: level.price,
                kind: MarkKind::Skip,
                text: format!("{}: {text}", info.label),
            });
        }
    }
    out.sort_by_key(|m| m.at.0);
    out
}

/// Marker size (logical px): a wedge's width and height, a square's and a circle's side.
const SIZE: f32 = 9.0;
/// How far (logical px) a hover may be from a marker's center to show its reason.
pub const HIT: f32 = 8.0;

/// The layer: marks rebuilt on change, positions per frame.
#[derive(Default)]
pub struct StrategyMarksLayer {
    pub hidden: bool,
    marks: Vec<Mark>,
    /// Plot-local centers of the marks on the plot this frame, with their index.
    shown: Vec<(f32, f32, usize)>,
    /// The inputs `marks` was built from (source revision, trading revision, contract).
    built: Option<(u64, u64, ContractKey)>,
}

impl StrategyMarksLayer {
    /// Rebuild when the inputs moved; true if the marks changed.
    pub fn update(
        &mut self,
        key: (u64, u64, ContractKey),
        build: impl FnOnce() -> Vec<Mark>,
    ) -> bool {
        if self.built == Some(key) {
            return false;
        }
        self.built = Some(key);
        let marks = build();
        let changed = marks != self.marks;
        self.marks = marks;
        // Reserved here, so `prepare` never grows it.
        self.shown = Vec::with_capacity(self.marks.len());
        changed
    }

    /// Positions for this frame (allocation-free: within the reserved buffer).
    pub fn prepare(&mut self, bars: BarsView<'_>, vp: &Viewport, plot_w: f32, plot_h: f32) {
        self.shown.clear();
        if self.hidden {
            return;
        }
        let axis = TimeAxis::new(bars);
        for (i, m) in self.marks.iter().enumerate() {
            let Some(x) = axis.x_of(vp, m.at) else {
                continue;
            };
            let y = vp.price_to_y(m.price.0 as f64) as f32;
            let inside =
                (-SIZE..=plot_w + SIZE).contains(&x) && (-SIZE..=plot_h + SIZE).contains(&y);
            if inside && self.shown.len() < self.shown.capacity() {
                self.shown.push((x, y, i));
            }
        }
    }

    /// The mark under plot-local (`x`, `y`), nearest first.
    pub fn hit(&self, x: f32, y: f32) -> Option<&Mark> {
        hit(&self.shown, x, y).map(|i| &self.marks[i])
    }

    pub fn paint(&self, plot: Bounds<Pixels>, theme: &Theme, window: &mut Window, _: &mut App) {
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        for &(x, y, i) in &self.shown {
            let (cx, cy) = (ox + x, oy + y);
            match self.marks[i].kind {
                MarkKind::Entry(side) => {
                    // A wedge of stacked bars, pointing at the price: under it for a buy
                    // (pointing up), over it for a sell (pointing down).
                    let color = if side == Side::Buy {
                        theme.up
                    } else {
                        theme.down
                    };
                    let rows = 4;
                    let step = SIZE / rows as f32;
                    for r in 0..rows {
                        let w = step * (r as f32 + 1.0) * 2.0 - step;
                        let dy = 2.0 + step * r as f32;
                        let top = if side == Side::Buy {
                            cy + dy
                        } else {
                            cy - dy - step
                        };
                        window.paint_quad(gpui::fill(
                            Bounds::new(point(px(cx - w / 2.0), px(top)), size(px(w), px(step))),
                            color,
                        ));
                    }
                }
                MarkKind::Exit => window.paint_quad(quad(
                    Bounds::new(
                        point(px(cx - SIZE / 2.0), px(cy - SIZE / 2.0)),
                        size(px(SIZE), px(SIZE)),
                    ),
                    px(0.0),
                    gpui::transparent_black(),
                    px(2.0),
                    theme.text_main,
                    BorderStyle::Solid,
                )),
                MarkKind::Skip => {
                    let s = SIZE * 0.8;
                    window.paint_quad(quad(
                        Bounds::new(
                            point(px(cx - s / 2.0), px(cy - s / 2.0)),
                            size(px(s), px(s)),
                        ),
                        px(s / 2.0),
                        gpui::transparent_black(),
                        px(1.5),
                        theme.text_dim,
                        BorderStyle::Solid,
                    ));
                }
            }
        }
    }
}

/// The index of the mark nearest (`x`, `y`) within [`HIT`], if any.
pub fn hit(shown: &[(f32, f32, usize)], x: f32, y: f32) -> Option<usize> {
    shown
        .iter()
        .map(|&(mx, my, i)| ((mx - x).powi(2) + (my - y).powi(2), i))
        .filter(|&(d, _)| d <= HIT * HIT)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, i)| i)
}

#[cfg(test)]
mod tests {
    use lines_core::{Month, Qty};
    use lines_feed::catalog;
    use lines_strategy::backtest::DecisionRecord;
    use lines_strategy::{Decision, LevelRef};
    use lines_trading::strategy::{StrategyId, StrategyState};
    use lines_trading::{AccountId, Command, Money, OrderId, Reject, RejectCode};

    use super::*;

    fn es() -> ContractKey {
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
    }

    fn info(decisions: Vec<DecisionRecord>) -> StrategyInfo {
        use crate::strategy_panel::StrategySource;
        let mut i = crate::strategy_panel::DemoStrategies::sample(
            &AccountId::new("paper"),
            es(),
            UtcMillis(0),
        )
        .snapshot()
        .remove(2);
        i.id = StrategyId(3);
        i.state = StrategyState::Armed;
        i.decisions = decisions;
        i
    }

    fn rec(
        at: i64,
        reason: &str,
        command: bool,
        result: Option<Result<Vec<OrderId>, Reject>>,
        level: Option<i64>,
    ) -> DecisionRecord {
        DecisionRecord {
            at: UtcMillis(at),
            decision: Decision {
                rule: "r".into(),
                reason: reason.into(),
                levels: level
                    .map(|p| LevelRef {
                        source: "demo".into(),
                        kind: "support".into(),
                        label: None,
                        price: Price(p),
                    })
                    .into_iter()
                    .collect(),
                command: command.then(|| Command::CancelAll {
                    account: AccountId::new("paper"),
                    contract: None,
                }),
            },
            result,
            not_sent: None,
        }
    }

    fn fill(order: u64, side: Side, qty: i64, price: i64, at: i64) -> FillEvent {
        FillEvent {
            account: AccountId::new("paper"),
            order: OrderId(order),
            contract: es(),
            side,
            price: Price(price),
            qty: Qty(qty),
            fee: Money::ZERO,
            realised: Money::ZERO,
            trade_id: format!("t{order}-{at}"),
            reconciled: false,
            at: UtcMillis(at),
        }
    }

    #[test]
    fn its_fills_are_entries_then_exits_with_their_reasons() {
        let i = info(vec![
            rec(
                10,
                "Buy at Support: touched from above",
                true,
                Some(Ok(vec![OrderId(7), OrderId(8)])),
                Some(601_200),
            ),
            rec(
                20,
                "Skipped: the target crosses the zone boundary",
                false,
                None,
                Some(601_500),
            ),
            rec(
                25,
                "Sell at Resistance",
                true,
                Some(Err(Reject::new(RejectCode::Strategy, "outside its hours"))),
                Some(602_000),
            ),
            rec(26, "A note without a level", false, None, None),
        ]);
        let product = catalog::get(catalog::ES).unwrap();
        let fills = [
            fill(7, Side::Buy, 2, 601_200, 11),
            // Someone else's order on the same contract: no marker.
            fill(99, Side::Buy, 1, 601_300, 12),
            fill(8, Side::Sell, 2, 601_400, 30),
        ];
        let m = build(&[i], &fills, es(), &product);
        let kinds: Vec<MarkKind> = m.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MarkKind::Entry(Side::Buy),
                MarkKind::Skip,
                MarkKind::Skip,
                MarkKind::Exit
            ]
        );
        assert_eq!(
            m[0].text,
            "level-touch #3: entry, Buy 2 @ 6012.00. Buy at Support: touched from above"
        );
        assert!(
            m[2].text.contains("denied: outside its hours"),
            "{}",
            m[2].text
        );
        assert_eq!(m[1].price, Price(601_500));
        assert!(
            m[3].text.contains("exit, Sell 2 @ 6014.00"),
            "{}",
            m[3].text
        );
        // Another contract's chart shows none of them.
        let nq = ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap());
        assert!(build(&[info(Vec::new())], &fills, nq, &product).is_empty());
    }

    /// The screenshot path: the demo source's decisions behind a script's fills give an
    /// entry, an exit (its stop) and a skipped signal, each with its reason.
    #[test]
    fn demo_decisions_behind_real_fills_make_every_kind() {
        use crate::strategy_panel::{DemoStrategies, StrategySource};
        let demo = DemoStrategies::sample(&AccountId::new("paper"), es(), UtcMillis(0));
        let fills = [
            fill(1, Side::Buy, 10, 601_950, 100),
            fill(2, Side::Sell, 10, 601_800, 200),
        ];
        let before = demo.revision();
        demo.attach(
            StrategyId(3),
            &fills,
            Some((UtcMillis(150), Price(602_050))),
        );
        assert_ne!(demo.revision(), before);
        let product = catalog::get(catalog::ES).unwrap();
        let m = build(&demo.snapshot(), &fills, es(), &product);
        let kinds: Vec<MarkKind> = m.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [MarkKind::Entry(Side::Buy), MarkKind::Skip, MarkKind::Exit]
        );
        assert!(m[2].text.contains("Stop: Support broke"), "{}", m[2].text);
    }

    #[test]
    fn a_hover_finds_the_nearest_mark_within_reach() {
        let shown = [(10.0, 10.0, 0), (14.0, 10.0, 1), (100.0, 100.0, 2)];
        assert_eq!(hit(&shown, 13.0, 10.0), Some(1));
        assert_eq!(hit(&shown, 9.0, 11.0), Some(0));
        assert_eq!(hit(&shown, 50.0, 50.0), None);
    }
}
