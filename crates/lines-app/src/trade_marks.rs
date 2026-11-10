//! Trade history on the chart (owner-approved mock): the journal's closed trades on the chart
//! of their contract.
//!
//! - **Fills as markers:** a buy is a wedge under its price pointing up, a sell a wedge over
//!   it pointing down (up/down colors, and the shape says the side, never color alone). One
//!   marker per order at its average price. The journal records fills, not orders, so an
//!   "order" here is a run of consecutive same-side fills in the trade (`orders`): two orders
//!   that fill back to back on the same side read as one (docs/known-gaps.md).
//! - **Entry to exit:** a dotted line from the average entry (first entry's time) to the
//!   average exit (last exit's time), green for a winner, red for a loser, dim for flat, and the
//!   last exit's label carries the P&L, so color is never the only cue.
//! - **Labels** (`B 2 @ 7510.00`, the exit's `· +$1,800`) follow the P&L display setting:
//!   with it off, side, size and price only.
//! - **Hover** lists the trade (side, size, prices, times, ticks, money, duration) and the
//!   hovered order's fills; a **click** opens the trade in the journal.
//! - **Options** per chart (`TradesConfig`, an optional `ChartState` field): shown (Alt+E),
//!   entry–exit lines, labels, which accounts (this pane's or all) and the period (today, this
//!   week, all, by the session's trade date on the chart's clock: the replay's in a replay).
//! - **No allocation in prepare or paint:** marks are rebuilt only when their inputs move (the
//!   journal's revision, the contract, the options, the P&L display, the trade date), their
//!   positions go into buffers reserved then, and label text is shaped once and cached.

use gpui::{App, Bounds, Pixels, ShapedLine, Window, point, px, size};
use lines_core::drawings::TimeAxis;
use lines_core::timeframe::BarsView;
use lines_core::viewport::Viewport;
use lines_core::{CivilDate, ContractKey, Product, UtcMillis};
use lines_trading::journal::{Trade, TradeFill};
use lines_trading::{AccountId, Side};
use serde::{Deserialize, Serialize};

use crate::pnl::PnlDisplay;
use crate::theme::Theme;

/// Which trades a chart shows, by trade date.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    /// The chart's current trade date.
    #[default]
    Today,
    /// Since the Monday of the current trade date's week.
    Week,
    /// Every trade the journal holds.
    All,
}

impl Period {
    pub const ALL: [Period; 3] = [Period::Today, Period::Week, Period::All];

    pub fn label(self) -> &'static str {
        match self {
            Period::Today => "Today",
            Period::Week => "This week",
            Period::All => "All",
        }
    }

    /// Whether a trade on `date` is in the period, `today` being the chart's trade date.
    pub fn contains(self, date: CivilDate, today: CivilDate) -> bool {
        match self {
            Period::Today => date == today,
            Period::Week => {
                let monday = today.add_days(-(today.weekday() as i64));
                date >= monday && date <= today
            }
            Period::All => true,
        }
    }
}

/// A chart's trade options (saved per chart; every field optional, written only when it
/// differs from the default: on, lines and labels, this pane's account, today).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradesConfig {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_lines: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_labels: bool,
    /// Every account's trades (paper and replay included), not only this pane's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_accounts: bool,
    #[serde(default, skip_serializing_if = "Period::is_today")]
    pub period: Period,
}

impl Period {
    fn is_today(&self) -> bool {
        *self == Period::Today
    }
}

impl TradesConfig {
    pub fn is_default(&self) -> bool {
        *self == TradesConfig::default()
    }
}

/// One order: a run of consecutive same-side fills in a trade.
#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub side: Side,
    /// The first fill's time (the marker's bar).
    pub at: UtcMillis,
    /// Average price, in price units (display; an average needn't be on the tick grid).
    pub price: f64,
    pub qty: i64,
    /// The fills: index range into the trade's `fills`.
    pub fills: std::ops::Range<usize>,
}

/// A trade's orders: runs of consecutive same-side fills, each at its average price.
pub fn orders(fills: &[TradeFill]) -> Vec<Order> {
    let mut out: Vec<Order> = Vec::new();
    let mut start = 0;
    for i in 0..=fills.len() {
        let ends = i == fills.len() || fills[i].side != fills[start].side;
        if ends && i > start {
            let run = &fills[start..i];
            let qty: i64 = run.iter().map(|f| f.qty.0).sum();
            let value: i128 = run
                .iter()
                .map(|f| f.price.0 as i128 * f.qty.0 as i128)
                .sum();
            out.push(Order {
                side: run[0].side,
                at: run[0].at,
                price: if qty > 0 {
                    value as f64 / qty as f64
                } else {
                    run[0].price.0 as f64
                },
                qty,
                fills: start..i,
            });
            start = i;
        }
    }
    out
}

/// One marker.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub side: Side,
    pub at: UtcMillis,
    pub price: f64,
    /// `B 2 @ 7510.00`, and on the trade's last exit `· +$1,800` (as the P&L display allows).
    pub label: String,
    /// Its trade (an index into `Built::trades`).
    pub trade: usize,
    /// The order's fills, one line each, for the hover.
    pub fills: Vec<String>,
}

/// The dotted line from a trade's average entry to its average exit.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Link {
    pub from: (UtcMillis, f64),
    pub to: (UtcMillis, f64),
    /// Net P&L's sign: `Some(true)` a winner, `Some(false)` a loser, `None` flat.
    pub win: Option<bool>,
}

/// A trade as the hover says it, and its journal id (a click opens it there).
#[derive(Clone, Debug, PartialEq)]
pub struct TradeText {
    pub id: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Built {
    pub marks: Vec<Mark>,
    pub links: Vec<Link>,
    pub trades: Vec<TradeText>,
}

/// What a chart shows: the trades on `contract`, of `account` (`None`: every account), whose
/// trade date is in `period` from `today`, with labels and hover text as `mode` allows.
pub fn build(
    trades: &[Trade],
    contract: ContractKey,
    account: Option<&AccountId>,
    period: Period,
    today: CivilDate,
    product: &Product,
    mode: PnlDisplay,
) -> Built {
    let mut out = Built::default();
    let price_text = |v: f64| {
        crate::journal_view::decimal(
            v * product.price.scale.num() as f64 / product.price.scale.den() as f64,
            product.price.scale.den(),
        )
    };
    let qty_text = |q: i64| {
        let mut s = String::new();
        let _ = product.qty.write(lines_core::Qty(q), &mut s);
        s
    };
    for t in trades {
        if t.contract != contract
            || account.is_some_and(|a| *a != t.account)
            || !period.contains(t.trade_date, today)
        {
            continue;
        }
        let index = out.trades.len();
        let row = crate::journal_view::row(t, mode);
        let [
            _,
            account_cell,
            contract_cell,
            side_cell,
            qty_cell,
            entry,
            exit,
            pnl,
            ticks,
            duration,
            ..,
        ] = &row.cells;
        let mut summary = format!("{side_cell} {qty_cell} {contract_cell} · {account_cell}");
        if t.incomplete {
            summary.push_str(" · entry approximate");
        }
        let mut result = Vec::new();
        if mode.ticks() && ticks != "—" {
            result.push(format!("{ticks} t"));
        }
        if pnl != "—" {
            result.push(pnl.clone());
        }
        result.push(duration.clone());
        out.trades.push(TradeText {
            id: t.id.clone(),
            lines: vec![
                summary,
                format!(
                    "Entry {entry} · {}",
                    crate::journal_view::clock_text(t.entry_time)
                ),
                format!(
                    "Exit {exit} · {}",
                    crate::journal_view::clock_text(t.exit_time)
                ),
                result.join(" · "),
                "Click: open in the journal".to_owned(),
            ],
        });
        let orders = orders(&t.fills);
        let last_exit = orders.iter().rposition(|o| o.side != t.side);
        for (k, o) in orders.iter().enumerate() {
            let letter = if o.side == Side::Buy { "B" } else { "S" };
            let mut label = format!("{letter} {} @ {}", qty_text(o.qty), price_text(o.price));
            if Some(k) == last_exit && pnl != "—" {
                label.push_str(" · ");
                label.push_str(pnl);
            }
            let fills = t.fills[o.fills.clone()]
                .iter()
                .map(|f| {
                    format!(
                        "{letter} {} @ {} · {}",
                        qty_text(f.qty.0),
                        price_text(f.price.0 as f64),
                        crate::journal_view::clock_text(f.at)
                    )
                })
                .collect();
            out.marks.push(Mark {
                side: o.side,
                at: o.at,
                price: o.price,
                label,
                trade: index,
                fills,
            });
        }
        if let (Some(entry), Some(exit)) = (t.avg_entry(), t.avg_exit()) {
            let net = t.net();
            out.links.push(Link {
                from: (t.entry_time, entry),
                to: (t.exit_time, exit),
                win: (net.0 != 0).then_some(net.0 > 0),
            });
        }
    }
    out
}

/// Marker size (logical px at 100 %): a wedge's width and height.
const SIZE_100: f32 = 9.0;
/// How far (logical px at 100 %) a hover may be from a marker's center.
const HIT_100: f32 = 12.0;
/// A label's text size and padding at 100 %.
const LABEL_PX_100: f32 = 11.0;
const LABEL_PAD_100: f32 = 4.0;
/// Dots along an entry–exit line: every `DOT_STEP` px, `DOT` px square.
const DOT_STEP: f32 = 6.0;
const DOT: f32 = 2.0;

/// The layer: marks rebuilt on change, positions per frame.
#[derive(Default)]
pub struct TradeMarksLayer {
    pub config: TradesConfig,
    built: Built,
    /// Plot-local marker centers this frame, with their mark index.
    shown: Vec<(f32, f32, usize)>,
    /// Plot-local entry–exit segments this frame, with their link index.
    lines: Vec<(f32, f32, f32, f32, usize)>,
    /// Each mark's label, shaped once (at the zoom generation in `shaped_at`).
    shaped: Vec<Option<ShapedLine>>,
    shaped_at: u32,
    /// The inputs `built` came from.
    key: Option<BuildKey>,
}

/// What the marks depend on.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildKey {
    pub journal: u64,
    pub contract: ContractKey,
    pub account: Option<AccountId>,
    pub period: Period,
    pub today: CivilDate,
    pub mode: PnlDisplay,
    pub zone_epoch: u64,
}

impl TradeMarksLayer {
    /// Rebuild when the inputs moved; true if what's shown changed.
    pub fn update(&mut self, key: BuildKey, build: impl FnOnce() -> Built) -> bool {
        if self.key.as_ref() == Some(&key) {
            return false;
        }
        self.key = Some(key);
        let built = build();
        if built == self.built {
            return false;
        }
        self.built = built;
        // Reserved here, so `prepare` never grows them.
        self.shown = Vec::with_capacity(self.built.marks.len());
        self.lines = Vec::with_capacity(self.built.links.len());
        self.shaped = vec![None; self.built.marks.len()];
        true
    }

    /// Build again on the next poll (an option changed).
    pub fn invalidate(&mut self) {
        self.key = None;
    }

    /// Forget the marks (another market: the next poll builds its own).
    pub fn clear(&mut self) {
        self.built = Built::default();
        self.shown.clear();
        self.lines.clear();
        self.shaped.clear();
        self.key = None;
    }

    pub fn shown(&self) -> bool {
        !self.config.hidden
    }

    /// Positions for this frame (allocation-free: within the reserved buffers).
    pub fn prepare(&mut self, bars: BarsView<'_>, vp: &Viewport, plot_w: f32, plot_h: f32) {
        self.shown.clear();
        self.lines.clear();
        if self.config.hidden {
            return;
        }
        let axis = TimeAxis::new(bars);
        let x_of = |at: UtcMillis| axis.index_of(at).map(|i| vp.index_to_x(i.floor()) as f32);
        let y_of = |p: f64| vp.price_to_y(p) as f32;
        let m = crate::zoom::f(SIZE_100) + 2.0;
        for (i, mk) in self.built.marks.iter().enumerate() {
            let (Some(x), y) = (x_of(mk.at), y_of(mk.price)) else {
                continue;
            };
            let inside = (-m..=plot_w + m).contains(&x) && (-m..=plot_h + m).contains(&y);
            if inside && self.shown.len() < self.shown.capacity() {
                self.shown.push((x, y, i));
            }
        }
        if !self.config.no_lines {
            for (i, l) in self.built.links.iter().enumerate() {
                let (Some(x0), Some(x1)) = (x_of(l.from.0), x_of(l.to.0)) else {
                    continue;
                };
                let (y0, y1) = (y_of(l.from.1), y_of(l.to.1));
                // Wholly off one side of the plot: nothing to draw.
                let off = (x0.max(x1) < 0.0)
                    || (x0.min(x1) > plot_w)
                    || (y0.max(y1) < 0.0)
                    || (y0.min(y1) > plot_h);
                if !off && self.lines.len() < self.lines.capacity() {
                    self.lines.push((x0, y0, x1, y1, i));
                }
            }
        }
    }

    /// The index of the mark nearest plot-local (`x`, `y`), within reach.
    pub fn hit_index(&self, x: f32, y: f32) -> Option<usize> {
        let r = crate::zoom::f(HIT_100);
        self.shown
            .iter()
            .map(|&(mx, my, i)| ((mx - x).powi(2) + (my - y).powi(2), i))
            .filter(|&(d, _)| d <= r * r)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, i)| i)
    }

    /// The mark under plot-local (`x`, `y`): its trade's text and its fills.
    pub fn hit(&self, x: f32, y: f32) -> Option<(&TradeText, &Mark)> {
        let i = self.hit_index(x, y)?;
        let m = &self.built.marks[i];
        Some((&self.built.trades[m.trade], m))
    }

    pub fn paint(
        &mut self,
        plot: Bounds<Pixels>,
        // Study labels shown (Clean chart and "hide study labels" hide the trade labels too).
        labels: bool,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let (pw, ph) = (f32::from(plot.size.width), f32::from(plot.size.height));
        // Entry–exit lines first (under the markers): dots along the segment, inside the plot.
        for &(x0, y0, x1, y1, i) in &self.lines {
            let color = match self.built.links[i].win {
                Some(true) => theme.up,
                Some(false) => theme.down,
                None => theme.text_dim,
            };
            let len = (x1 - x0).hypot(y1 - y0);
            let n = (len / DOT_STEP).floor() as i32;
            for k in 0..=n {
                let f = if n == 0 { 0.0 } else { k as f32 / n as f32 };
                let (x, y) = (x0 + (x1 - x0) * f, y0 + (y1 - y0) * f);
                if (0.0..=pw).contains(&x) && (0.0..=ph).contains(&y) {
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(px(ox + x - DOT / 2.0), px(oy + y - DOT / 2.0)),
                            size(px(DOT), px(DOT)),
                        ),
                        color,
                    ));
                }
            }
        }
        let s = crate::zoom::f(SIZE_100);
        for &(x, y, i) in &self.shown {
            let (cx, cy) = (ox + x, oy + y);
            let buy = self.built.marks[i].side == Side::Buy;
            let color = if buy { theme.up } else { theme.down };
            // A wedge of stacked bars pointing at the price: under it for a buy, over it for a
            // sell, on a halo so it stands clear of a candle in its own color.
            let rows = 4;
            let step = s / rows as f32;
            for (pad, c) in [(1.0, theme.bg), (0.0, color)] {
                for r in 0..rows {
                    let w = 3.0 + 2.0 * pad + 2.0 * r as f32 * s / SIZE_100;
                    let dy = 2.0 + step * r as f32;
                    let top = if buy { cy + dy } else { cy - dy - step };
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(px(cx - w / 2.0), px(top - pad)),
                            size(px(w), px(step + 2.0 * pad)),
                        ),
                        c,
                    ));
                }
            }
        }
        if self.config.no_labels || !labels {
            return;
        }
        // Labels in the chart's tag style: on the background, edged in the side's color, beside
        // the wedge (shaped once per label; a new zoom shapes them again).
        let zoom_gen = crate::zoom::generation();
        if zoom_gen != self.shaped_at {
            self.shaped.iter_mut().for_each(|l| *l = None);
            self.shaped_at = zoom_gen;
        }
        let text_px = crate::zoom::f(LABEL_PX_100);
        let pad = crate::zoom::f(LABEL_PAD_100);
        let h = text_px + 2.0 * pad * 0.5 + 2.0;
        for &(x, y, i) in &self.shown {
            let mark = &self.built.marks[i];
            if self.shaped[i].is_none() {
                let font = window.text_style().font();
                let run = gpui::TextRun {
                    len: mark.label.len(),
                    font,
                    color: theme.text_main,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                self.shaped[i] = Some(window.text_system().shape_line(
                    gpui::SharedString::from(mark.label.clone()),
                    px(text_px),
                    &[run],
                    None,
                ));
            }
            let Some(line) = &self.shaped[i] else {
                continue;
            };
            let w = f32::from(line.width) + 2.0 * pad;
            let buy = mark.side == Side::Buy;
            // Beside the wedge, on its side of the price; inside the plot.
            let left = (ox + x + s * 0.5 + 3.0).min(ox + pw - w).max(ox);
            let top = if buy { oy + y + 2.0 } else { oy + y - 2.0 - h };
            let top = top.clamp(oy, (oy + ph - h).max(oy));
            let edge = if buy { theme.up } else { theme.down };
            window.paint_quad(gpui::quad(
                Bounds::new(point(px(left), px(top)), size(px(w), px(h))),
                px(2.0),
                theme.bg,
                px(1.0),
                edge,
                gpui::BorderStyle::Solid,
            ));
            let _ = line.paint(
                point(px(left + pad), px(top + (h - text_px) * 0.5 - 1.0)),
                px(text_px + 2.0),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use lines_core::{Month, Price, Qty};
    use lines_feed::catalog;
    use lines_trading::Money;

    use super::*;

    fn es() -> ContractKey {
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
    }

    fn fill(at: i64, side: Side, price: i64, qty: i64) -> TradeFill {
        TradeFill {
            trade_id: format!("f{at}"),
            at: UtcMillis(at),
            side,
            price: Price(price),
            qty: Qty(qty),
            fee: Money::ZERO,
            source: None,
            reconciled: false,
        }
    }

    /// A closed ES trade on `account` and `date`: long 2 from 7510.00, out at `exit`.
    fn trade(account: &str, date: CivilDate, exit: i64) -> Trade {
        let fills = vec![
            fill(1_000, Side::Buy, 751000, 1),
            fill(2_000, Side::Buy, 751000, 1),
            fill(60_000, Side::Sell, exit, 2),
        ];
        // ES: 1/100 price units, $50 a point, 2 contracts: (exit − entry) units in dollars.
        let dollars = (exit - 751000) as i128;
        Trade {
            id: format!("{account}-{exit}"),
            account: AccountId(account.into()),
            contract: es(),
            side: Side::Buy,
            entry_time: UtcMillis(1_000),
            exit_time: UtcMillis(60_000),
            trade_date: date,
            max_qty: 2,
            entry_qty: 2,
            entry_value: 751000 * 2,
            exit_qty: 2,
            exit_value: exit as i128 * 2,
            realized: Money(dollars * 10_000_000_000),
            fees: Money::ZERO,
            mae: Money::ZERO,
            mfe: Money::ZERO,
            low: Price(751000),
            high: Price(exit.max(751000)),
            fills,
            tags: Vec::new(),
            note: String::new(),
            screenshots: Vec::new(),
            incomplete: false,
        }
    }

    #[test]
    fn a_chart_shows_its_contracts_trades_with_labels_as_the_pnl_display_allows() {
        let es_product = catalog::get(catalog::ES).unwrap();
        let today = CivilDate::new(2026, 11, 5).unwrap();
        let paper = AccountId("paper".into());
        let trades = vec![
            trade("paper", today, 752800),              // a winner today
            trade("paper", today, 750850),              // a loser today
            trade("sim", today, 752000),                // another account
            trade("paper", today.add_days(-1), 752000), // yesterday
        ];
        let b = build(
            &trades,
            es(),
            Some(&paper),
            Period::Today,
            today,
            &es_product,
            PnlDisplay::Both,
        );
        assert_eq!(b.trades.len(), 2, "this pane's account, today");
        // One marker per order: the two buys are one order at their average.
        assert_eq!(b.marks.len(), 4);
        assert_eq!(b.marks[0].label, "B 2 @ 7510.00");
        assert!(
            b.marks[1].label.starts_with("S 2 @ 7528.00 · "),
            "{}",
            b.marks[1].label
        );
        assert!(
            b.marks[1].label.contains('$'),
            "the exit carries the P&L: {}",
            b.marks[1].label
        );
        assert_eq!(
            b.marks[0].fills.len(),
            2,
            "the hover lists the order's fills"
        );
        assert_eq!(b.links.len(), 2);
        assert_eq!((b.links[0].win, b.links[1].win), (Some(true), Some(false)));
        assert!(b.trades[0].lines.iter().any(|l| l.starts_with("Entry ")));
        // P&L display off: side, size and price only, no money anywhere.
        let off = build(
            &trades,
            es(),
            Some(&paper),
            Period::Today,
            today,
            &es_product,
            PnlDisplay::Off,
        );
        assert_eq!(off.marks[1].label, "S 2 @ 7528.00");
        assert!(
            off.trades
                .iter()
                .flat_map(|t| &t.lines)
                .all(|l| !l.contains('$'))
        );
        // All accounts, this week.
        let all = build(
            &trades,
            es(),
            None,
            Period::Week,
            today,
            &es_product,
            PnlDisplay::Both,
        );
        assert_eq!(all.trades.len(), 4);
        // Another contract: nothing.
        let nq = ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap());
        assert!(
            build(
                &trades,
                nq,
                None,
                Period::All,
                today,
                &es_product,
                PnlDisplay::Both
            )
            .marks
            .is_empty()
        );
    }

    #[test]
    fn an_order_is_a_run_of_same_side_fills_at_its_average() {
        let f = [
            fill(1, Side::Buy, 751000, 1),
            fill(2, Side::Buy, 751025, 1),
            fill(3, Side::Sell, 752800, 2),
        ];
        let o = orders(&f);
        assert_eq!(o.len(), 2);
        assert_eq!(
            (o[0].side, o[0].qty, o[0].fills.clone()),
            (Side::Buy, 2, 0..2)
        );
        assert_eq!(o[0].price, 751012.5);
        assert_eq!(o[0].at, UtcMillis(1));
        assert_eq!((o[1].side, o[1].qty, o[1].price), (Side::Sell, 2, 752800.0));
        assert!(orders(&[]).is_empty());
    }

    #[test]
    fn periods_by_trade_date() {
        // 2026-11-05 is a Thursday.
        let today = CivilDate::new(2026, 11, 5).unwrap();
        assert!(Period::Today.contains(today, today));
        assert!(!Period::Today.contains(today.add_days(-1), today));
        assert!(Period::Week.contains(today.add_days(-3), today), "Monday");
        assert!(
            !Period::Week.contains(today.add_days(-4), today),
            "last Sunday"
        );
        assert!(!Period::Week.contains(today.add_days(1), today));
        assert!(Period::All.contains(today.add_days(-400), today));
    }

    #[test]
    fn the_config_is_written_only_when_it_differs() {
        assert_eq!(
            serde_json::to_string(&TradesConfig::default()).unwrap(),
            "{}"
        );
        let c = TradesConfig {
            all_accounts: true,
            period: Period::Week,
            ..Default::default()
        };
        let text = serde_json::to_string(&c).unwrap();
        assert_eq!(text, r#"{"all_accounts":true,"period":"week"}"#);
        assert_eq!(serde_json::from_str::<TradesConfig>(&text).unwrap(), c);
        assert_eq!(
            serde_json::from_str::<TradesConfig>("{}").unwrap(),
            TradesConfig::default()
        );
    }
}
