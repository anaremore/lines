//! Strategy markers on the chart (decision 0025, phase c): a strategy's entries, exits and
//! skipped signals on the chart of its contract, with the reason on hover. A toggleable layer
//! (per chart, saved as the optional `ChartState.hide_strategy_marks`).
//!
//! - **Entries and exits** are the instance's own fills (from `TradeState`'s ring of the
//!   core's `Event::Fill`s). Its own means what the core counts as its own
//!   (`strategy_owns`): orders its Place, Bracket and Exit decisions created, its Flatten's
//!   own orders (never what a Flatten or Cancel All merely cancelled) and their bracket exits.
//!   Every fill on the account and contract moves the replayed position, so one the instance
//!   didn't decide (a Stop's or the daily loss's flatten, a person's) keeps the next ones
//!   right, though it gets no marker. A fill that grows the position is an entry, any other
//!   an exit; one that reverses it is both.
//! - **Skipped signals** are decisions with no command, and commands the core refused or that
//!   were never sent, at their first level's price (a decision without a level has no price
//!   and no marker).
//! - **Shapes, not color alone:** a buy entry is an upward wedge under its price, a sell
//!   entry a downward wedge over it (up/down colors), an exit an X (text color), a
//!   skip a small hollow circle (dim).
//! - **No allocation in prepare or paint:** marks are rebuilt only when their inputs move, and
//!   their positions go into a buffer reserved then.

use gpui::{App, BorderStyle, Bounds, Pixels, Window, point, px, quad, size};
use lines_core::drawings::TimeAxis;
use lines_core::timeframe::BarsView;
use lines_core::viewport::Viewport;
use lines_core::{ContractKey, Price, Product, UtcMillis};
use lines_trading::{Command, FillEvent, Side};

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
    /// The hover text's first line: instance, what, when.
    pub head: String,
    /// Its second: the order, if any, and why.
    pub text: String,
}

/// Fills kept for markers (the newest; older ones lose their markers).
pub const KEEP_FILLS: usize = 4096;

/// `level-touch #3 · Exit · 21:58:59 UTC` (UTC, like the chart's time axis).
fn head(label: &str, what: &str, at: UtcMillis) -> String {
    let clock = crate::journal_view::clock_text(at);
    format!(
        "{label} · {what} · {} UTC",
        clock.get(6..).unwrap_or_default()
    )
}

/// What the UI knows of an order (`TradeState::order`).
pub type OrderInfo = (lines_trading::Role, Option<lines_trading::OrderId>);

/// The orders the instance's decisions created, each with its decision's reason: the core's
/// rule (`strategy_owns`), never an order a decision only cancelled or modified.
fn owned<'a>(
    info: &'a StrategyInfo,
    order: &impl Fn(lines_trading::OrderId) -> Option<OrderInfo>,
) -> Vec<(lines_trading::OrderId, &'a str)> {
    let mut out = Vec::new();
    for d in &info.decisions {
        let Some(Ok(ids)) = &d.result else {
            continue;
        };
        let reason = d.decision.reason.as_str();
        match &d.decision.command {
            Some(Command::PlaceOrder(_) | Command::PlaceExit(_) | Command::PlaceBracket(_)) => {
                out.extend(ids.iter().map(|&o| (o, reason)));
            }
            Some(Command::Flatten { .. }) => out.extend(
                ids.iter()
                    .filter(|&&o| {
                        order(o).is_some_and(|(role, _)| role == lines_trading::Role::Flatten)
                    })
                    .map(|&o| (o, reason)),
            ),
            _ => {}
        }
    }
    out
}

/// Build the markers for `contract` from the instances and the recent fills (oldest first).
pub fn build<'f>(
    infos: &[StrategyInfo],
    fills: impl Iterator<Item = &'f FillEvent> + Clone,
    contract: ContractKey,
    product: &Product,
    order: impl Fn(lines_trading::OrderId) -> Option<OrderInfo>,
) -> Vec<Mark> {
    let mut out = Vec::new();
    for info in infos.iter().filter(|i| i.contract == contract) {
        let own = owned(info, &order);
        // The account's position on the contract, from every fill.
        let mut pos: i64 = 0;
        for f in fills
            .clone()
            .filter(|f| f.account == info.account && f.contract == contract)
        {
            let delta = if f.side == Side::Buy {
                f.qty.0
            } else {
                -f.qty.0
            };
            let before = pos;
            pos += delta;
            // Its own, or a bracket exit of its own.
            let parent = order(f.order).and_then(|(_, p)| p);
            let Some(&(_, why)) = own
                .iter()
                .find(|(o, _)| *o == f.order || Some(*o) == parent)
            else {
                continue;
            };
            let side = if f.side == Side::Buy { "Buy" } else { "Sell" };
            let mut mark = |what: &str, kind: MarkKind, qty: i64| {
                out.push(Mark {
                    at: f.at,
                    price: f.price,
                    kind,
                    head: head(&info.label, what, f.at),
                    text: format!(
                        "{side} {qty} @ {} · {why}",
                        product.price.format::<32>(f.price).as_str()
                    ),
                });
            };
            if before == 0 || (before > 0) == (delta > 0) {
                mark("Entry", MarkKind::Entry(f.side), f.qty.0);
            } else if pos != 0 && (pos > 0) != (before > 0) {
                // Reverses: closes what was open and opens the other way.
                mark("Exit", MarkKind::Exit, before.abs());
                mark("Entry", MarkKind::Entry(f.side), pos.abs());
            } else {
                mark("Exit", MarkKind::Exit, f.qty.0);
            }
        }
        for d in &info.decisions {
            // Skipped: no orders came of it (no command, refused, or never sent).
            let skipped = match (&d.decision.command, &d.result) {
                (_, Some(Ok(_))) => None,
                (None, _) => Some(d.decision.reason.clone()),
                (Some(_), Some(Err(r))) => {
                    Some(format!("{} (denied: {})", d.decision.reason, r.message))
                }
                (Some(_), None) => d
                    .not_sent
                    .as_ref()
                    .map(|why| format!("{} (not sent: {why})", d.decision.reason)),
            };
            let (Some(text), Some(level)) = (skipped, d.decision.levels.first()) else {
                continue;
            };
            out.push(Mark {
                at: d.at,
                price: level.price,
                kind: MarkKind::Skip,
                head: head(&info.label, "Skipped", d.at),
                text,
            });
        }
    }
    out.sort_by_key(|m| m.at.0);
    out
}

/// Plot-local x of a mark at `at`: on the bar holding that time, not part-way to the next one
/// (a fill at :59 is still that minute's bar).
fn mark_x(axis: &TimeAxis<'_>, vp: &Viewport, at: UtcMillis) -> Option<f32> {
    axis.index_of(at).map(|i| vp.index_to_x(i.floor()) as f32)
}

/// Marker size (logical px): a wedge's width and height, a square's and a circle's side.
const SIZE: f32 = 9.0;
/// How far (logical px) a hover may be from a marker's center to show its reason.
pub const HIT: f32 = 12.0; // a wedge reaches 11 px from its price

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
        if marks == self.marks {
            return false; // what's shown (and a hover on it) stands
        }
        self.marks = marks;
        // Reserved here, so `prepare` never grows it.
        self.shown = Vec::with_capacity(self.marks.len());
        true
    }

    /// Positions for this frame (allocation-free: within the reserved buffer).
    pub fn prepare(&mut self, bars: BarsView<'_>, vp: &Viewport, plot_w: f32, plot_h: f32) {
        self.shown.clear();
        if self.hidden {
            return;
        }
        let axis = TimeAxis::new(bars);
        for (i, m) in self.marks.iter().enumerate() {
            let Some(x) = mark_x(&axis, vp, m.at) else {
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
                    // A halo first, so it stands clear of a candle or line in its own color.
                    for r in 0..rows {
                        let w = 5.0 + 2.0 * r as f32;
                        let dy = 2.0 + step * r as f32;
                        let top = if side == Side::Buy {
                            cy + dy
                        } else {
                            cy - dy - step
                        };
                        window.paint_quad(gpui::fill(
                            Bounds::new(
                                point(px(cx - w / 2.0), px(top - 1.0)),
                                size(px(w), px(step + 2.0)),
                            ),
                            theme.bg,
                        ));
                    }
                    for r in 0..rows {
                        let w = 3.0 + 2.0 * r as f32;
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
                MarkKind::Exit => {
                    // An X (no candle has that shape) on a halo.
                    let back = SIZE + 3.0;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(px(cx - back / 2.0), px(cy - back / 2.0)),
                            size(px(back), px(back)),
                        ),
                        theme.bg,
                    ));
                    for k in -2..=2 {
                        let d = k as f32 * 2.0;
                        for y in [cy + d, cy - d] {
                            window.paint_quad(gpui::fill(
                                Bounds::new(
                                    point(px(cx + d - 1.0), px(y - 1.0)),
                                    size(px(2.0), px(2.0)),
                                ),
                                theme.text_main,
                            ));
                        }
                    }
                }
                MarkKind::Skip => {
                    let s = SIZE;
                    window.paint_quad(quad(
                        Bounds::new(
                            point(px(cx - s / 2.0), px(cy - s / 2.0)),
                            size(px(s), px(s)),
                        ),
                        px(s / 2.0),
                        theme.bg,
                        px(2.0),
                        theme.text_dim,
                        BorderStyle::Solid,
                    ));
                }
            }
        }
    }
}

/// The hover text's box (plot-local left, top, width): beside the pointer, on whichever side
/// has room, inside the plot (markers mostly sit at the live edge, by the price axis).
pub fn tip_place(x: f32, y: f32, plot_w: f32, plot_h: f32) -> (f32, f32, f32) {
    const WIDTH: f32 = 320.0;
    const HEIGHT: f32 = 60.0; // up to three lines of text
    const GAP: f32 = 12.0;
    const EDGE: f32 = 4.0;
    let w = WIDTH.min((plot_w - 2.0 * EDGE).max(0.0));
    let left = if x + GAP + w <= plot_w - EDGE {
        x + GAP
    } else {
        (x - GAP - w).max(EDGE)
    };
    let top = if y + GAP + HEIGHT <= plot_h - EDGE {
        y + GAP
    } else {
        (y - GAP - HEIGHT).max(EDGE)
    };
    (left, top, w)
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

    /// The commands a test decision can carry.
    #[derive(Clone, Copy)]
    enum Cmd {
        None,
        Place,
        Flatten,
    }

    fn rec(
        at: i64,
        reason: &str,
        cmd: Cmd,
        result: Option<Result<Vec<OrderId>, Reject>>,
        level: Option<i64>,
    ) -> DecisionRecord {
        let account = AccountId::new("paper");
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
                command: match cmd {
                    Cmd::None => None,
                    Cmd::Place => Some(Command::PlaceOrder(lines_trading::OrderSpec {
                        account,
                        contract: es(),
                        side: Side::Buy,
                        qty: Qty(1),
                        kind: lines_trading::OrderKind::Market,
                    })),
                    Cmd::Flatten => Some(Command::Flatten {
                        account,
                        contract: Some(es()),
                    }),
                },
            },
            result,
            not_sent: None,
        }
    }

    /// No order the UI knows (an order placed by a decision needs nothing from here).
    fn unknown(_: OrderId) -> Option<OrderInfo> {
        None
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
        let mut never = rec(
            27,
            "Sell at Resistance again",
            Cmd::Place,
            None,
            Some(602_100),
        );
        never.not_sent = Some("too many decisions at once".into());
        let i = info(vec![
            rec(
                10,
                "Buy at Support: touched from above",
                Cmd::Place,
                Some(Ok(vec![OrderId(7), OrderId(8)])),
                Some(601_200),
            ),
            rec(
                20,
                "Skipped: the target crosses the zone boundary",
                Cmd::None,
                None,
                Some(601_500),
            ),
            rec(
                25,
                "Sell at Resistance",
                Cmd::Place,
                Some(Err(Reject::new(RejectCode::Strategy, "outside its hours"))),
                Some(602_000),
            ),
            rec(26, "A note without a level", Cmd::None, None, None),
            never,
        ]);
        let product = catalog::get(catalog::ES).unwrap();
        let fills = [
            fill(7, Side::Buy, 2, 601_200, 11),
            fill(8, Side::Sell, 2, 601_400, 30),
        ];
        let m = build(&[i], fills.iter(), es(), &product, unknown);
        let kinds: Vec<MarkKind> = m.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MarkKind::Entry(Side::Buy),
                MarkKind::Skip,
                MarkKind::Skip,
                MarkKind::Skip,
                MarkKind::Exit
            ]
        );
        assert_eq!(m[0].head, "level-touch #3 · Entry · 00:00:00 UTC");
        assert_eq!(
            m[0].text,
            "Buy 2 @ 6012.00 · Buy at Support: touched from above"
        );
        assert!(
            m[2].text.contains("denied: outside its hours"),
            "{}",
            m[2].text
        );
        assert!(m[3].text.contains("not sent: too many"), "{}", m[3].text);
        assert_eq!(m[1].price, Price(601_500));
        assert!(m[4].head.contains("Exit"), "{}", m[4].head);
        assert!(m[4].text.contains("Sell 2 @ 6014.00"), "{}", m[4].text);
        // Another contract's chart shows none of them.
        let nq = ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap());
        assert!(build(&[info(Vec::new())], fills.iter(), nq, &product, unknown).is_empty());
    }

    /// A Stop's flatten is no decision of the instance's, but it moves the position: the next
    /// short is an entry, its cover an exit. Reproduced from a review finding.
    #[test]
    fn a_flatten_it_did_not_decide_keeps_the_next_marks_right() {
        let i = info(vec![
            rec(10, "Long", Cmd::Place, Some(Ok(vec![OrderId(1)])), None),
            rec(30, "Short", Cmd::Place, Some(Ok(vec![OrderId(3)])), None),
            rec(40, "Cover", Cmd::Place, Some(Ok(vec![OrderId(4)])), None),
        ]);
        let product = catalog::get(catalog::ES).unwrap();
        let fills = [
            fill(1, Side::Buy, 2, 601_200, 11),
            fill(2, Side::Sell, 2, 601_100, 20), // the core's flatten
            fill(3, Side::Sell, 2, 601_000, 31),
            fill(4, Side::Buy, 2, 600_900, 41),
        ];
        let m = build(&[i], fills.iter(), es(), &product, unknown);
        let kinds: Vec<MarkKind> = m.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MarkKind::Entry(Side::Buy),
                MarkKind::Entry(Side::Sell),
                MarkKind::Exit
            ]
        );
    }

    /// A Flatten's result lists the orders it cancelled too: a person's order is never its
    /// own. A bracket exit is its parent's. Reproduced from a review finding.
    #[test]
    fn only_what_a_decision_created_is_its_own() {
        let i = info(vec![
            rec(10, "Long", Cmd::Place, Some(Ok(vec![OrderId(1)])), None),
            rec(
                30,
                "Out",
                Cmd::Flatten,
                Some(Ok(vec![OrderId(9), OrderId(5)])),
                None,
            ),
        ]);
        let product = catalog::get(catalog::ES).unwrap();
        let fills = [
            fill(1, Side::Buy, 1, 601_200, 11),
            fill(9, Side::Buy, 1, 601_250, 12), // a person's, cancelled by the Flatten
            fill(2, Side::Sell, 1, 601_300, 20), // the bracket exit of order 1
            fill(5, Side::Sell, 1, 601_350, 31), // the Flatten's own
        ];
        let order = |o: OrderId| match o.0 {
            2 => Some((lines_trading::Role::StopLoss, Some(OrderId(1)))),
            5 => Some((lines_trading::Role::Flatten, None)),
            9 => Some((lines_trading::Role::Entry, None)),
            _ => None,
        };
        let m = build(&[i], fills.iter(), es(), &product, order);
        let texts: Vec<&str> = m.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Buy 1 @ 6012.00 · Long",
                "Sell 1 @ 6013.00 · Long",
                "Sell 1 @ 6013.50 · Out"
            ]
        );
    }

    /// A fill that reverses the position closes it and opens the other way: both show.
    #[test]
    fn a_reversal_is_an_exit_and_an_entry() {
        let i = info(vec![rec(
            10,
            "Flip",
            Cmd::Place,
            Some(Ok(vec![OrderId(1), OrderId(2)])),
            None,
        )]);
        let product = catalog::get(catalog::ES).unwrap();
        let fills = [
            fill(1, Side::Buy, 1, 601_200, 11),
            fill(2, Side::Sell, 3, 601_100, 20),
        ];
        let m = build(&[i], fills.iter(), es(), &product, unknown);
        let got: Vec<(MarkKind, &str)> = m.iter().map(|m| (m.kind, m.text.as_str())).collect();
        assert_eq!(
            got,
            [
                (MarkKind::Entry(Side::Buy), "Buy 1 @ 6012.00 · Flip"),
                (MarkKind::Exit, "Sell 1 @ 6011.00 · Flip"),
                (MarkKind::Entry(Side::Sell), "Sell 2 @ 6011.00 · Flip"),
            ]
        );
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
        let m = build(&demo.snapshot(), fills.iter(), es(), &product, unknown);
        let kinds: Vec<MarkKind> = m.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [MarkKind::Entry(Side::Buy), MarkKind::Skip, MarkKind::Exit]
        );
        assert!(m[2].text.contains("Stop: Support broke"), "{}", m[2].text);
    }

    /// A fill at :59 of a minute sits on that minute's bar, not near the next one.
    #[test]
    fn a_mark_sits_on_the_bar_holding_its_time() {
        use lines_core::Bar;
        let bar = |m: i64| Bar {
            time: UtcMillis(m * 60_000),
            open: Price(600_000),
            high: Price(600_000),
            low: Price(600_000),
            close: Price(600_000),
            volume: Qty(1),
        };
        let bars = [bar(0), bar(1), bar(2)];
        let view = BarsView::fixed(&bars, 60_000);
        let vp = Viewport::default();
        let axis = TimeAxis::new(view);
        let at = UtcMillis(60_000 + 59_000);
        assert_eq!(mark_x(&axis, &vp, at), Some(vp.index_to_x(1.0) as f32));
        assert_ne!(axis.x_of(&vp, at), mark_x(&axis, &vp, at));
    }

    #[test]
    fn the_hover_text_stays_inside_the_plot() {
        // Room on the right and below: beside the pointer.
        assert_eq!(
            tip_place(100.0, 100.0, 1000.0, 600.0),
            (112.0, 112.0, 320.0)
        );
        // At the live edge and the plot's bottom: flips left and up.
        assert_eq!(
            tip_place(990.0, 590.0, 1000.0, 600.0),
            (658.0, 518.0, 320.0)
        );
        // A plot narrower than the box: it fits the plot, at its left edge.
        assert_eq!(tip_place(100.0, 10.0, 200.0, 40.0), (4.0, 4.0, 192.0));
    }

    #[test]
    fn a_hover_finds_the_nearest_mark_within_reach() {
        let shown = [(10.0, 10.0, 0), (14.0, 10.0, 1), (100.0, 100.0, 2)];
        assert_eq!(hit(&shown, 13.0, 10.0), Some(1));
        assert_eq!(hit(&shown, 9.0, 11.0), Some(0));
        assert_eq!(hit(&shown, 50.0, 50.0), None);
    }
}
