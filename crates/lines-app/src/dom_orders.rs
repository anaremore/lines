//! The DOM's own orders (roadmap 7): working orders in the orders column at their price, drag
//! ghosts, the position marker at the average price, and rejections on their row.
//!
//! [`OrdersLayer::prepare`] copies this frame's state out of the trading cache (`try_lock`: a
//! busy pump never stalls a frame; the previous frame's copy is drawn instead) into buffers
//! reused frame to frame, so painting allocates nothing in steady state. [`OrdersLayer::paint`]
//! draws it over the ladder rows (`crate::dom_trade` decides styles and rows).
//!
//! Looks (docs/design.md §3; shape, never colour alone):
//! - **pending** (sent, no ack): an outline;
//! - **held** (a bracket exit waiting for its entry): a dashed outline, dim;
//! - **working**: a solid block in the side's colour;
//! - **partially filled**: an outline with the filled share solid, and `filled/qty`;
//! - a stop is prefixed `S`; a **ghost** (dragged, not yet confirmed) is a dashed outline in
//!   its order's role colour on the selection fill, with its label, at the new price while the
//!   order stays at its old one;
//! - the **position** is a bracket of lines around the price cell of the average price's row
//!   plus `L n` / `S n` in the orders column;
//! - a **rejection** is a warning bar across its row with the reason, for a few seconds.

use std::fmt::Write as _;
use std::time::Instant;

use gpui::{App, Bounds, Hsla, Pixels, Window, fill, point, px, size};
use lines_core::ladder::LadderRow;
use lines_core::{ContractKey, Price};
use lines_trading::{AccountId, OrderId, Side};

use crate::order_resize::ResizeHit;

use crate::dom_trade::{
    Ghost, MarkRole, MarkStyle, OrderMark, group_key, mark_group_text, mark_text, order_mark,
    position_row, row_of,
};
use crate::text::Glyphs;
use crate::theme::Theme;
use crate::trading::TradeCache;

/// A refusal to tag on its row: `KIND: short` (`text[start..mid]`) or, if that doesn't fit
/// the orders column, just `short` (`text[mid..end]`).
#[derive(Clone, Copy, Debug)]
struct RowFlash {
    price: Price,
    /// DENIED / REJECTED.
    label: &'static str,
    start: usize,
    mid: usize,
    end: usize,
}

/// An order being dragged: where it was grabbed and where the pointer is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragOrder {
    pub order: OrderId,
    pub to: Price,
    /// Where the order was when the drag began, and the held exits that move with it (an
    /// unfilled entry's, with the setting on): previewed at their price shifted by the move.
    pub from: Price,
    pub legs: [Option<(OrderId, Price)>; 2],
}

impl DragOrder {
    /// A drag of `order` from `at`, moving `legs` with it.
    pub fn new(order: OrderId, at: Price, legs: [Option<(OrderId, Price)>; 2]) -> DragOrder {
        DragOrder {
            order,
            to: at,
            from: at,
            legs,
        }
    }

    /// Screenshot fixture (`--drag-order PRICE`): the account's newest working entry on
    /// `contract` held dragged to `to`, its held stop and target with it when `with_legs` (the
    /// setting), as a press on it and a move would leave it. Nothing is sent.
    pub fn fixture(
        s: &crate::trading::TradeState,
        account: &lines_trading::AccountId,
        contract: lines_core::ContractKey,
        to: Price,
        with_legs: bool,
    ) -> Option<DragOrder> {
        let o = s
            .open_orders(account, contract)
            .filter(|o| o.role == lines_trading::Role::Entry)
            .max_by_key(|o| o.id)?;
        let from = crate::dom_trade::order_price(o)?;
        let legs = if with_legs {
            s.held_legs(o.id)
        } else {
            [None; 2]
        };
        Some(DragOrder {
            to,
            ..DragOrder::new(o.id, from, legs)
        })
    }

    /// Where the order and its legs are drawn while dragged: each order and its new price.
    pub fn previews(&self) -> impl Iterator<Item = (OrderId, Price)> + '_ {
        let d = self.to.0 - self.from.0;
        std::iter::once((self.order, self.to)).chain(
            self.legs
                .iter()
                .flatten()
                .map(move |&(id, p)| (id, Price(p.0 + d))),
        )
    }
}

/// The narrowest mark that draws its − and + (each at least 20 px, with room for its size).
pub const RESIZE_MARK_W: f32 = 60.0;
const RESIZE_BTN_W: f32 = 20.0;

/// What a press on a row of the orders column is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkPress {
    /// No mark there.
    Nothing,
    /// A mark for one order: that order.
    One(OrderId),
    /// A mark for this many orders (`S 200 ×20`): no action on it may touch only one of them.
    Many(usize),
}

impl MarkPress {
    /// The order a drag or a right-click acts on: `Ok(None)` with no mark there, and for a mark
    /// standing for several orders the refusal to show, never its first order. A DOM narrower
    /// than `KEYS_IN_HEADER` gets the short words: the status line shows what fits, whole.
    pub fn order(self, dom_width: f32) -> Result<Option<OrderId>, String> {
        match self {
            MarkPress::Nothing => Ok(None),
            MarkPress::One(id) => Ok(Some(id)),
            MarkPress::Many(n) if crate::trade_bar::keys_in_header(dom_width) => Err(format!(
                "{n} orders on this mark: use the chart's order list"
            )),
            MarkPress::Many(n) => Err(format!("×{n}: chart's list")),
        }
    }
}

/// The mark at `x` px into the orders cell (`cell_w` wide), laid out as `paint` draws `n` of them
/// side by side.
fn mark_press(slots: &[(OrderMark, i64, usize); 2], n: usize, x: f32, cell_w: f32) -> MarkPress {
    if n == 0 {
        return MarkPress::Nothing;
    }
    let w = ((cell_w - 2.0) / n as f32).max(1.0);
    let k = (((x - 1.0) / w).floor().max(0.0) as usize).min(n - 1);
    let (m, _, count) = &slots[k];
    if *count > 1 {
        MarkPress::Many(*count)
    } else {
        MarkPress::One(m.id)
    }
}

/// The piece of the resize control at `x` px into a single-order mark: a mark for several orders
/// has none (no action on it may touch only one of them), and a narrow mark is all value.
fn resize_zone(
    slots: &[(OrderMark, i64, usize); 2],
    n: usize,
    x: f32,
    cell_w: f32,
) -> Option<(OrderId, ResizeHit)> {
    // Only inside the cell: a press in another column is that column's.
    if x < 0.0 || x >= cell_w {
        return None;
    }
    let MarkPress::One(id) = mark_press(slots, n, x, cell_w) else {
        return None;
    };
    let w = ((cell_w - 2.0) / n as f32).max(1.0);
    let k = (((x - 1.0) / w).floor().max(0.0) as usize).min(n - 1);
    let into = x - 1.0 - k as f32 * w;
    let hit = if w < RESIZE_MARK_W {
        ResizeHit::Value
    } else if into < RESIZE_BTN_W {
        ResizeHit::Down
    } else if into >= w - RESIZE_BTN_W - 1.0 {
        ResizeHit::Up
    } else {
        ResizeHit::Value
    };
    Some((id, hit))
}

pub struct OrdersLayer {
    /// The single-order mark under the pointer, and whether its + can do anything.
    pub hover: Option<OrderId>,
    hover_grows: bool,
    marks: Vec<OrderMark>,
    flashes: Vec<RowFlash>,
    text: String,
    /// Signed quantity and average price of the position, if any.
    position: Option<(i64, f64)>,
    /// Drags waiting for the broker, drawn at their new price.
    pub ghosts: Vec<Ghost>,
    pub drag: Option<DragOrder>,
}

impl Default for OrdersLayer {
    fn default() -> Self {
        OrdersLayer {
            hover: None,
            hover_grows: false,
            marks: Vec::with_capacity(64),
            flashes: Vec::with_capacity(8),
            text: String::with_capacity(512),
            position: None,
            ghosts: Vec::with_capacity(8),
            drag: None,
        }
    }
}

/// What became of the ghosts this frame: refusals to show (price, reason).
pub type GhostRefusals = Vec<(Price, String)>;

impl OrdersLayer {
    /// Copy this frame's orders, position and rejections for `account` / `contract`. Returns
    /// refused drags (rare; allocates only then). Keeps the last frame's copy if the cache is
    /// busy.
    pub fn prepare(
        &mut self,
        cache: &TradeCache,
        account: Option<&AccountId>,
        contract: Option<ContractKey>,
    ) -> GhostRefusals {
        let mut refused = Vec::new();
        let (Some(account), Some(contract)) = (account, contract) else {
            self.marks.clear();
            self.flashes.clear();
            self.position = None;
            self.ghosts.clear();
            return refused;
        };
        let Some(s) = cache.try_read() else {
            return refused;
        };
        self.marks.clear();
        self.marks
            .extend(s.open_orders(account, contract).filter_map(order_mark));
        self.position = s
            .position(account, contract)
            .and_then(|p| p.avg_price.map(|a| (p.qty, a)));
        self.flashes.clear();
        self.text.clear();
        let now = Instant::now();
        for f in s.flashes(account, Some(contract), now) {
            if let Some(price) = f.price {
                // Whose refusal (the core's: denied, never sent; the broker's: rejected) and
                // why, in a word. The full message is in the header.
                let short = crate::dom_trade::short_reason(&f.message);
                let start = self.text.len();
                self.text.push_str(f.kind.label());
                self.text.push_str(": ");
                self.text.push_str(short);
                let mid = self.text.len();
                self.text.push_str(short);
                self.flashes.push(RowFlash {
                    price,
                    label: f.kind.label(),
                    start,
                    mid,
                    end: self.text.len(),
                });
            }
        }
        self.hover_grows = self.hover.and_then(|id| s.order(id)).is_some_and(|o| {
            let working = o
                .parent
                .and_then(|p| s.order(p))
                .is_some_and(lines_trading::Order::is_open);
            let position = s.position(account, contract).map_or(0, |p| p.qty);
            crate::order_resize::can_grow(o, working, position)
        });
        self.ghosts.retain(|g| {
            let (order, result) = (s.order(g.order), s.result(g.command));
            let outcome = match g.leg {
                Some(l) => crate::dom_trade::leg_outcome(g, l, order, result, s.order(l.entry)),
                None => crate::dom_trade::ghost_outcome(g, order, result),
            };
            match outcome {
                crate::dom_trade::GhostOutcome::Pending => true,
                crate::dom_trade::GhostOutcome::Acked => false,
                crate::dom_trade::GhostOutcome::Rejected(why) => {
                    // A leg that stayed: where it still is.
                    refused.push((g.leg.map_or(g.price, |l| l.was), why));
                    false
                }
            }
        });
        refused
    }

    /// What a press in the orders cell is on: the mark drawn there, on the row containing
    /// `price`, at `x` px into a cell `cell_w` wide (for drag and right-click).
    pub fn press_at(&self, price: Price, units: i64, x: f32, cell_w: f32) -> MarkPress {
        let row = row_of(price.0, units);
        let (slots, n, _) = row_marks(&self.marks, |m| row_of(m.price.0, units) == row);
        mark_press(&slots, n, x, cell_w)
    }

    /// The resize control's piece under a press (`resize_zone`), on the row containing `price`.
    pub fn zone_at(
        &self,
        price: Price,
        units: i64,
        x: f32,
        cell_w: f32,
    ) -> Option<(OrderId, ResizeHit)> {
        let row = row_of(price.0, units);
        let (slots, n, _) = row_marks(&self.marks, |m| row_of(m.price.0, units) == row);
        resize_zone(&slots, n, x, cell_w)
    }

    /// The newest mark that stands for one order (`--order-controls`).
    pub fn first_single(&self, units: i64) -> Option<OrderId> {
        self.marks
            .iter()
            .rev()
            .find(|m| {
                let row = row_of(m.price.0, units);
                let (slots, n, _) = row_marks(&self.marks, |o| row_of(o.price.0, units) == row);
                slots.iter().take(n).any(|s| s.0.id == m.id && s.2 == 1)
            })
            .map(|m| m.id)
    }

    /// Where order `id` rests, if it has a mark.
    pub fn price_of(&self, id: OrderId) -> Option<Price> {
        self.marks.iter().find(|m| m.id == id).map(|m| m.price)
    }

    /// Whether order `id` has its own mark (not one for several orders) on one of `rows`.
    pub fn drawn_alone(&self, id: OrderId, rows: &[LadderRow], units: i64) -> bool {
        self.price_of(id).is_some_and(|p| {
            let row = row_of(p.0, units);
            rows.iter().any(|r| row_of(r.price.0, units) == row) && {
                let (slots, n, _) = row_marks(&self.marks, |m| row_of(m.price.0, units) == row);
                slots.iter().take(n).any(|s| s.0.id == id && s.2 == 1)
            }
        })
    }

    /// Whether order `id`'s mark is on the row containing `price`.
    pub fn on_row(&self, id: OrderId, price: Price, units: i64) -> bool {
        self.marks
            .iter()
            .any(|m| m.id == id && row_of(m.price.0, units) == row_of(price.0, units))
    }

    /// Paint over the ladder. `orders`: the orders column's cell (x, w), if shown; `price`:
    /// the price column's cell. Nothing here draws outside those two cells, and refusal tags
    /// stay inside the orders cell (clipped): the price column is never covered.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        glyphs: &mut Glyphs,
        t: &Theme,
        rows: &[LadderRow],
        units: i64,
        tick: i64,
        orders: Option<(f32, f32)>,
        price: Option<(f32, f32)>,
        oy: f32,
        row_h: f32,
        hair: f32,
        price_fmt: &lines_core::PriceFormat,
        typing: Option<(OrderId, &str)>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let rect =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        let side_colour = |s: Side| if s == Side::Buy { t.up } else { t.down };
        let mark_colour = |t: &Theme, m: &OrderMark| match MarkRole::of(m) {
            MarkRole::Entry => side_colour(m.side),
            MarkRole::Target => t.order_target,
            MarkRole::Stop => t.order_stop,
        };
        let pos_row = self.position.map(|(_, avg)| position_row(avg, tick, units));
        let mut buf = lines_core::FixedStr::<24>::new();
        for (i, r) in rows.iter().enumerate() {
            let y = oy + i as f32 * row_h;
            let row = row_of(r.price.0, units);
            // Position marker: lines above and below the price cell, and L n / S n.
            if pos_row == Some(row)
                && let Some((px_, pw)) = price
            {
                let lw = 2.0 * hair.max(0.5);
                window.paint_quad(fill(rect(px_, y, pw, lw), t.text_main));
                window.paint_quad(fill(rect(px_, y + row_h - lw, pw, lw), t.text_main));
            }
            let Some((ox, ow)) = orders else {
                continue;
            };
            // Orders on this row: up to two kinds, side by side; several of one kind are one
            // mark (`S 200 ×20`), so a row never shows part of its orders as if it were all.
            let (slots, n, more) = row_marks(&self.marks, |m| row_of(m.price.0, units) == row);
            for (k, slot) in slots.iter().take(n).enumerate() {
                let (m, left, count) = (&slot.0, slot.1, slot.2);
                let w = (ow - 2.0) / n as f32;
                let (x, yy, h) = (ox + 1.0 + k as f32 * w, y + 2.0, row_h - 4.0);
                // Colour by role: entries take their side's, exits their own.
                let role = MarkRole::of(m);
                let c = match role {
                    MarkRole::Entry => side_colour(m.side),
                    MarkRole::Target => t.order_target,
                    MarkRole::Stop => t.order_stop,
                };
                // Several orders: no one order's fill to show.
                let style = if count > 1 && m.style == MarkStyle::Partial {
                    MarkStyle::Working
                } else {
                    m.style
                };
                let text_colour = match style {
                    MarkStyle::Working => {
                        window.paint_quad(fill(rect(x, yy, w - 1.0, h), c));
                        t.bg
                    }
                    MarkStyle::Partial => {
                        let share = (m.filled.0 as f32 / m.qty.0.max(1) as f32).clamp(0.0, 1.0);
                        window.paint_quad(fill(rect(x, yy, (w - 1.0) * share, h), c));
                        outline(window, rect(x, yy, w - 1.0, h), c, hair, false);
                        // The label goes on its own opaque pill (below).
                        t.mark_label().0
                    }
                    MarkStyle::Pending => {
                        outline(window, rect(x, yy, w - 1.0, h), c, hair, false);
                        c
                    }
                    MarkStyle::Held => {
                        // Dashed in the role's colour: held, and still recognisably a target
                        // or a stop.
                        outline(window, rect(x, yy, w - 1.0, h), c, hair, true);
                        t.text_dim
                    }
                };
                // Shape by role: a target is notched along its top edge, a stop along its
                // bottom edge, an entry is a plain block.
                let notch = 2.0 * hair.max(0.5);
                let notch_y = match role {
                    MarkRole::Target => Some(yy),
                    MarkRole::Stop => Some(yy + h - notch),
                    MarkRole::Entry => None,
                };
                if let Some(ny) = notch_y {
                    let cut = if style == MarkStyle::Working {
                        t.panel
                    } else {
                        c
                    };
                    window.paint_quad(fill(rect(x + 2.0, ny, w - 5.0, notch), cut));
                }
                // − and + show on the hovered mark for one order (never on a group's); its size
                // sits clear of the +.
                let controls = count == 1 && self.hover == Some(m.id) && w >= RESIZE_MARK_W;
                let text_w = if controls { w - RESIZE_BTN_W - 1.0 } else { w };
                buf.clear();
                if count > 1 {
                    let _ = mark_group_text(m, left, count, false, &mut buf);
                    let plus = if more && k == 1 { 8.0 } else { 0.0 };
                    if glyphs.width(buf.as_str(), text_colour, true, window) + plus > w - 7.0 {
                        buf.clear();
                        let _ = mark_group_text(m, left, count, true, &mut buf);
                    }
                } else if let Some((_, digits)) = typing.filter(|(id, _)| *id == m.id) {
                    // Its size being typed, in the mark.
                    let _ = std::fmt::Write::write_fmt(&mut buf, format_args!("{digits}_"));
                } else {
                    let _ = mark_text(m, &mut buf);
                }
                if more && k == 1 {
                    let _ = std::fmt::Write::write_str(&mut buf, "+");
                }
                // A partial fill's label on an opaque pill: readable over the fill (AA).
                if style == MarkStyle::Partial {
                    let tw = glyphs.width(buf.as_str(), text_colour, true, window);
                    let (l, top, pw, ph) = label_pill(x, yy, text_w, h, tw, hair, role);
                    window.paint_quad(fill(rect(l, top, pw, ph), t.mark_label().1));
                }
                // Inset past the outline (dashed when held) so the label never sits on it.
                glyphs.paint_right(
                    buf.as_str(),
                    x + text_w - 5.0,
                    y,
                    row_h,
                    text_colour,
                    true,
                    window,
                    cx,
                );
                if controls {
                    let ends = [
                        (x, "−", t.text_main),
                        (
                            x + w - 1.0 - RESIZE_BTN_W,
                            "+",
                            if self.hover_grows {
                                t.text_main
                            } else {
                                t.text_dim
                            },
                        ),
                    ];
                    for (bx, sign, colour) in ends {
                        // 20 px each way: the row and a pixel past it on each side.
                        let b = rect(bx, y - 1.0, RESIZE_BTN_W, row_h + 2.0);
                        window.paint_quad(fill(b, t.panel));
                        outline(window, b, t.control_edge, hair, false);
                        glyphs.paint(sign, bx + 6.0, y, row_h, colour, true, window, cx);
                    }
                }
            }
            if n == 0
                && pos_row == Some(row)
                && let Some((q, _)) = self.position
            {
                buf.clear();
                let _ = std::fmt::Write::write_fmt(
                    &mut buf,
                    format_args!("{}{}", if q > 0 { "▲" } else { "▼" }, q.abs()),
                );
                glyphs.paint_right(
                    buf.as_str(),
                    ox + ow - 3.0,
                    y,
                    row_h,
                    t.text_main,
                    true,
                    window,
                    cx,
                );
            }
            // Ghosts and the order being dragged (with the legs moving with it): a dashed
            // outline in the order's role colour at the new price on the selection fill (a held
            // mark has none), its label inside when the row has no mark or position label.
            let ghost_here = self
                .ghosts
                .iter()
                .filter(|g| g.qty.is_none())
                .map(|g| (g.order, g.price))
                .chain(self.drag.iter().flat_map(|d| d.previews()))
                .find(|(_, p)| row_of(p.0, units) == row);
            if let Some((id, _)) = ghost_here {
                let mark = self.marks.iter().find(|m| m.id == id);
                let c = mark.map_or(t.accent, |m| mark_colour(t, m));
                let r = rect(ox + 1.0, y + 2.0, ow - 3.0, row_h - 4.0);
                window.paint_quad(fill(r, t.ghost_selected));
                outline(window, r, c, hair, true);
                if n == 0
                    && pos_row != Some(row)
                    && let Some(m) = mark
                {
                    buf.clear();
                    let _ = mark_text(m, &mut buf);
                    glyphs.paint_right(
                        buf.as_str(),
                        ox + ow - 7.0,
                        y,
                        row_h,
                        t.text_main,
                        true,
                        window,
                        cx,
                    );
                }
            }
        }
        // Refusals: a short tag in the orders cell of their row (`DENIED: closed`, or just
        // `closed` when the column is narrow), clipped to the cell. Without an orders column
        // the header's list is where they show.
        let Some((ox, ow)) = orders else {
            return;
        };
        // Refused prices off the visible rows: counted in the orders column's title cell
        // ("2 hidden"), never in a price row (it would read as a refusal at that row's price),
        // never a bare number (it would read as an order count): beside the "Ord" title when
        // there is room, else in its place while they last (`dom_trade::hidden_placement`);
        // when the words don't fit even there, nothing here (the panel's status line still
        // counts them as "+N more").
        if let (Some(top), Some(bottom)) = (rows.first(), rows.last()) {
            let edges = crate::dom_trade::offscreen_refusals(
                self.flashes.iter().map(|f| (f.price, f.label)),
                row_of(top.price.0, units),
                row_of(bottom.price.0, units),
                units,
            );
            let n = edges[0].n + edges[1].n;
            // Orders off the visible rows (the owner's case: a target nobody could see): the
            // title cell names the nearest one ("↑ T1 5820.00"), or both edges in short
            // ("↑T1 ↓S1"); refusals, brief and urgent, take the cell first.
            if n == 0 && ow > 0.0 {
                let orders = crate::dom_trade::offscreen_orders(
                    &self.marks,
                    row_of(top.price.0, units),
                    row_of(bottom.price.0, units),
                    units,
                );
                // The nearest of the off-screen orders (fewest rows away), as "↑Tgt 5819.50",
                // else "↑Tgt" when the price doesn't fit; never cut.
                let near = match orders {
                    [Some((a, _)), Some((b, _))] => {
                        let top_r = row_of(top.price.0, units);
                        let bot_r = row_of(bottom.price.0, units);
                        if row_of(a.price.0, units) - top_r <= bot_r - row_of(b.price.0, units) {
                            Some((a, true))
                        } else {
                            Some((b, false))
                        }
                    }
                    [Some((m, _)), None] => Some((m, true)),
                    [None, Some((m, _))] => Some((m, false)),
                    [None, None] => None,
                };
                if let Some((m, above)) = near {
                    let mut s = lines_core::FixedStr::<48>::new();
                    let _ = crate::chart_trade::write_edge_priced(&mut s, &m, above, price_fmt);
                    if glyphs.width(s.as_str(), t.text_main, false, window) + 8.0 > ow {
                        s.clear();
                        let _ = crate::chart_trade::write_edge_short(&mut s, &m, above);
                    }
                    if glyphs.width(s.as_str(), t.text_main, false, window) + 8.0 <= ow {
                        let hh = crate::pane_columns::header_row_h();
                        let ty = oy - hh;
                        let cell = rect(ox, ty, ow, hh);
                        window.with_content_mask(
                            Some(gpui::ContentMask { bounds: cell }),
                            |window| {
                                window.paint_quad(fill(cell, t.panel));
                                // Opaque over the "Ord" title, in its whole cell: it replaces
                                // the title, nothing shows half covered.
                                crate::layout_rec::set_layer(1);
                                glyphs.paint(
                                    s.as_str(),
                                    ox + 4.0,
                                    ty,
                                    hh,
                                    mark_colour(t, &m),
                                    false,
                                    window,
                                    cx,
                                );
                                crate::layout_rec::set_layer(0);
                            },
                        );
                    }
                }
            }
            if n > 0 && ow > 0.0 {
                let mut s = lines_core::FixedStr::<32>::new();
                let _ = write!(s, "{n} hidden");
                let title_w = glyphs.width("Ord", t.text_dim, false, window);
                let w = glyphs.width(s.as_str(), t.text_main, true, window) + 6.0;
                let hh = crate::pane_columns::header_row_h();
                let ty = oy - hh;
                let pill = match crate::dom_trade::hidden_placement(ow, title_w, w) {
                    crate::dom_trade::HiddenPlacement::Beside => {
                        Some(rect(ox + 4.0, ty + 1.0, w, hh - 2.0))
                    }
                    // In the title's place: the whole cell, so "Ord" never shows half covered.
                    crate::dom_trade::HiddenPlacement::InsteadOfTitle => {
                        Some(rect(ox + 1.0, ty + 1.0, ow - 2.0, hh - 2.0))
                    }
                    crate::dom_trade::HiddenPlacement::None => None,
                };
                let instead = crate::dom_trade::hidden_placement(ow, title_w, w)
                    == crate::dom_trade::HiddenPlacement::InsteadOfTitle;
                if let Some(pill) = pill {
                    window.with_content_mask(Some(gpui::ContentMask { bounds: pill }), |window| {
                        window.paint_quad(fill(pill, t.panel));
                        outline(window, pill, t.warn, hair, false);
                        let x = f32::from(pill.origin.x) + 3.0;
                        // In the title's place it covers "Ord" whole; beside it, the lint still
                        // checks the two.
                        crate::layout_rec::set_layer(u8::from(instead));
                        glyphs.paint(s.as_str(), x, ty, hh, t.text_main, true, window, cx);
                        crate::layout_rec::set_layer(0);
                    });
                }
            }
        }
        for f in &self.flashes {
            let row = row_of(f.price.0, units);
            let Some(i) = rows.iter().position(|r| row_of(r.price.0, units) == row) else {
                continue;
            };
            let y = oy + i as f32 * row_h;
            let long = &self.text[f.start..f.mid];
            let text = if glyphs.width(long, t.on_warn, true, window) + 6.0 <= ow {
                long
            } else {
                &self.text[f.mid..f.end]
            };
            let cell = rect(ox, y + 1.0, ow, row_h - 2.0);
            window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                window.paint_quad(fill(cell, t.warn));
                glyphs.paint(text, ox + 3.0, y, row_h, t.on_warn, true, window, cx);
            });
        }
    }
}

/// The marks of the orders `on_row` picks: up to two kinds, each its first order, what is left
/// across that kind and how many; how many kinds fit, and whether more kinds are there.
fn row_marks(
    marks: &[OrderMark],
    on_row: impl Fn(&OrderMark) -> bool,
) -> ([(OrderMark, i64, usize); 2], usize, bool) {
    let blank = marks.first().copied().map(|m| (m, 0, 0));
    let mut slots = [blank.unwrap_or((
        OrderMark {
            id: OrderId(0),
            side: Side::Buy,
            price: Price(0),
            style: MarkStyle::Working,
            qty: lines_core::Qty(0),
            filled: lines_core::Qty(0),
            stop: false,
            role: lines_trading::order::Role::Single,
            exit: false,
        },
        0,
        0,
    )); 2];
    let (mut n, mut more) = (0, false);
    for m in marks.iter().filter(|m| on_row(m)) {
        let left = m.qty.0 - m.filled.0;
        if let Some(s) = slots[..n]
            .iter_mut()
            .find(|s| group_key(&s.0) == group_key(m))
        {
            s.1 += left;
            s.2 += 1;
        } else if n < 2 {
            slots[n] = (*m, left, 1);
            n += 1;
        } else {
            more = true;
        }
    }
    (slots, n, more)
}

/// A partial mark's label pill (left, top, width, height) in the mark at (`x`, `yy`, `w`,
/// `h`) for a label `tw` wide, right-aligned like the label: inside the outline, and clear of
/// a target's top notch or a stop's bottom notch, so the role's shape stays whole.
#[allow(clippy::too_many_arguments)]
fn label_pill(
    x: f32,
    yy: f32,
    w: f32,
    h: f32,
    tw: f32,
    hair: f32,
    role: MarkRole,
) -> (f32, f32, f32, f32) {
    let lw = hair.max(0.5) * 1.5;
    let notch = 2.0 * hair.max(0.5);
    let top = yy
        + if role == MarkRole::Target {
            notch.max(lw)
        } else {
            lw
        };
    let bottom = yy + h
        - if role == MarkRole::Stop {
            notch.max(lw)
        } else {
            lw
        };
    // The mark's inner right edge is `x + w - 1 - lw`; the label ends at `x + w - 5`.
    let right = x + w - 1.0 - lw;
    let left = (x + w - 7.0 - tw).max(x + lw);
    (left, top, (right - left).max(0.0), (bottom - top).max(0.0))
}

/// A rectangle's outline, solid or dashed.
fn outline(window: &mut Window, b: Bounds<Pixels>, c: Hsla, hair: f32, dashed: bool) {
    let (x, y) = (f32::from(b.origin.x), f32::from(b.origin.y));
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let lw = hair.max(0.5) * 1.5;
    let r = |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
    if !dashed {
        window.paint_quad(fill(r(x, y, w, lw), c));
        window.paint_quad(fill(r(x, y + h - lw, w, lw), c));
        window.paint_quad(fill(r(x, y, lw, h), c));
        window.paint_quad(fill(r(x + w - lw, y, lw, h), c));
        return;
    }
    let d = crate::theme::HELD_DASH;
    let mut dx = 0.0;
    while dx < w {
        let seg = d.on.min(w - dx);
        window.paint_quad(fill(r(x + dx, y, seg, lw), c));
        window.paint_quad(fill(r(x + dx, y + h - lw, seg, lw), c));
        dx += d.on + d.off;
    }
    let mut dy = 0.0;
    while dy < h {
        let seg = d.on.min(h - dy);
        window.paint_quad(fill(r(x, y + dy, lw, seg), c));
        window.paint_quad(fill(r(x + w - lw, y + dy, lw, seg), c));
        dy += d.on + d.off;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(id: u64, side: Side, qty: i64, stop: bool, exit: bool, style: MarkStyle) -> OrderMark {
        OrderMark {
            id: OrderId(id),
            side,
            price: Price(100),
            style,
            qty: lines_core::Qty(qty),
            filled: lines_core::Qty(0),
            stop,
            role: if stop && exit {
                lines_trading::order::Role::StopLoss
            } else {
                lines_trading::order::Role::Single
            },
            exit,
        }
    }

    /// A mark for several orders refuses a drag and a right-click, naming the count and the
    /// chart's list, and touches none of them; a mark for one order is that order. A press acts
    /// on the mark under it, not on the row's first order.
    #[test]
    fn a_mark_for_several_orders_refuses_and_one_for_a_single_order_acts() {
        let stops: Vec<_> = (0..20)
            .map(|i| mark(i, Side::Sell, 10, true, true, MarkStyle::Working))
            .collect();
        let (s, n, _) = row_marks(&stops, |_| true);
        let press = mark_press(&s, n, 30.0, 78.0);
        assert_eq!(press, MarkPress::Many(20));
        assert_eq!(
            press.order(1000.0),
            Err("20 orders on this mark: use the chart's order list".to_owned())
        );
        // A narrow DOM: words that fit its status line (240 px wide: 148 px beside the label,
        // 7.5 px a character), so the count and the pointer both show.
        let short = press.order(240.0).unwrap_err();
        assert_eq!(short, "×20: chart's list");
        assert!(short.chars().count() as f32 * 7.5 <= 148.0, "{short}");
        assert_eq!(
            press.order(0.0),
            press.order(1000.0),
            "not laid out yet: full"
        );
        // One order: that order, as before.
        let one = [mark(7, Side::Sell, 10, true, true, MarkStyle::Working)];
        let (s, n, _) = row_marks(&one, |_| true);
        assert_eq!(
            mark_press(&s, n, 30.0, 78.0).order(240.0),
            Ok(Some(OrderId(7)))
        );
        // Nothing on the row: nothing to do, nothing to say.
        let (s, n, _) = row_marks(&[], |_| true);
        assert_eq!(mark_press(&s, n, 30.0, 78.0).order(240.0), Ok(None));
        // Two marks on a row: each press is on its own mark, wherever the group is.
        let mut both = vec![mark(50, Side::Buy, 1, false, false, MarkStyle::Working)];
        both.extend(stops.iter().copied());
        let (s, n, _) = row_marks(&both, |_| true);
        assert_eq!(n, 2);
        assert_eq!(mark_press(&s, n, 5.0, 78.0), MarkPress::One(OrderId(50)));
        assert_eq!(mark_press(&s, n, 60.0, 78.0), MarkPress::Many(20));
        let mut rev = stops.clone();
        rev.push(mark(50, Side::Buy, 1, false, false, MarkStyle::Working));
        let (s, n, _) = row_marks(&rev, |_| true);
        assert_eq!(mark_press(&s, n, 5.0, 78.0), MarkPress::Many(20));
        assert_eq!(mark_press(&s, n, 60.0, 78.0), MarkPress::One(OrderId(50)));
        // The cell's edges stay on a mark.
        assert_eq!(mark_press(&s, n, -3.0, 78.0), MarkPress::Many(20));
        assert_eq!(mark_press(&s, n, 500.0, 78.0), MarkPress::One(OrderId(50)));
    }

    /// − and + only on a mark for one order: a group mark has no resize control, so it can't
    /// resize just one of its orders (#67's refusal stays what a press on it gets).
    #[test]
    fn a_group_mark_has_no_resize_control_and_a_single_one_has_three_parts() {
        let exit = |id, qty| mark(id, Side::Sell, qty, false, true, MarkStyle::Working);
        let one = |id, qty| (exit(id, qty), qty, 1usize);
        let single = [one(1, 2), one(2, 2)];
        // One mark across a 100 px cell.
        assert_eq!(
            resize_zone(&single, 1, 5.0, 100.0),
            Some((OrderId(1), ResizeHit::Down))
        );
        assert_eq!(
            resize_zone(&single, 1, 50.0, 100.0),
            Some((OrderId(1), ResizeHit::Value))
        );
        assert_eq!(
            resize_zone(&single, 1, 95.0, 100.0),
            Some((OrderId(1), ResizeHit::Up))
        );
        // Two marks in that cell are too narrow for buttons, and each press is its own order's.
        assert_eq!(
            resize_zone(&single, 2, 75.0, 100.0),
            Some((OrderId(2), ResizeHit::Value))
        );
        // A mark for several orders: nothing, wherever the press is.
        let group = [(exit(1, 2), 40, 20usize), one(2, 2)];
        for x in [5.0, 50.0, 95.0] {
            assert_eq!(resize_zone(&group, 1, x, 100.0), None, "{x}");
        }
        assert_eq!(resize_zone(&single, 0, 5.0, 100.0), None);
        // Beside the cell, on the same row: another column's press, never this order's.
        assert_eq!(resize_zone(&single, 1, -1.0, 100.0), None);
        assert_eq!(resize_zone(&single, 1, 100.0, 100.0), None);
        assert_eq!(resize_zone(&single, 1, 150.0, 100.0), None);
    }

    /// A press counts only the orders on its own row.
    #[test]
    fn a_press_reads_only_its_row() {
        let mut marks: Vec<_> = (0..3)
            .map(|i| mark(i, Side::Sell, 10, true, true, MarkStyle::Working))
            .collect();
        marks.push(OrderMark {
            price: Price(125),
            ..mark(9, Side::Sell, 10, true, true, MarkStyle::Working)
        });
        let layer = OrdersLayer {
            marks,
            ..OrdersLayer::default()
        };
        assert_eq!(
            layer.press_at(Price(100), 25, 30.0, 78.0),
            MarkPress::Many(3)
        );
        assert_eq!(
            layer.press_at(Price(125), 25, 30.0, 78.0),
            MarkPress::One(OrderId(9))
        );
        assert_eq!(
            layer.press_at(Price(150), 25, 30.0, 78.0),
            MarkPress::Nothing
        );
    }

    /// Twenty stops on a row are one mark saying so; another kind keeps its own; a third kind
    /// shows as more; held never joins working.
    #[test]
    fn a_row_shows_each_kind_once_with_its_total() {
        let stops: Vec<_> = (0..20)
            .map(|i| mark(i, Side::Sell, 10, true, true, MarkStyle::Working))
            .collect();
        let (s, n, more) = row_marks(&stops, |_| true);
        assert_eq!((n, more, s[0].1, s[0].2), (1, false, 200, 20));
        let mut text = String::new();
        mark_group_text(&s[0].0, s[0].1, s[0].2, false, &mut text).unwrap();
        assert_eq!(text, "S 200 ×20");
        text.clear();
        mark_group_text(&s[0].0, s[0].1, s[0].2, true, &mut text).unwrap();
        assert_eq!(text, "S ×20");
        let mut mixed = stops.clone();
        mixed.push(mark(30, Side::Buy, 1, false, false, MarkStyle::Working));
        mixed.push(mark(31, Side::Sell, 1, true, true, MarkStyle::Held));
        let (_, n, more) = row_marks(&mixed, |_| true);
        assert_eq!((n, more), (2, true));
        let (s, n, _) = row_marks(&mixed, |m| m.id.0 == 31 || m.id.0 == 0);
        assert_eq!((n, s[0].2, s[1].2), (2, 1, 1), "held keeps its own mark");
        let (_, n, more) = row_marks(&[], |_| true);
        assert_eq!((n, more), (0, false));
    }

    /// The pill stays inside the outline and off the role's notch, at DPI 1 and 2, with a
    /// label wider than the mark too.
    #[test]
    fn the_label_pill_leaves_the_outline_and_notch_whole() {
        for hair in [1.0f32, 0.5] {
            let (lw, notch) = (hair.max(0.5) * 1.5, 2.0 * hair.max(0.5));
            for role in [MarkRole::Entry, MarkRole::Target, MarkRole::Stop] {
                for tw in [10.0, 28.0, 200.0] {
                    let (x, yy, w, h) = (100.0, 50.0, 38.0, 14.0);
                    let (l, t, pw, ph) = label_pill(x, yy, w, h, tw, hair, role);
                    assert!(
                        l >= x + lw && l + pw <= x + w - 1.0 - lw + 1e-3,
                        "{role:?} {tw}"
                    );
                    let (top_clear, bottom_clear) = match role {
                        MarkRole::Target => (notch.max(lw), lw),
                        MarkRole::Stop => (lw, notch.max(lw)),
                        MarkRole::Entry => (lw, lw),
                    };
                    assert!(t >= yy + top_clear - 1e-3, "{role:?}");
                    assert!(t + ph <= yy + h - bottom_clear + 1e-3, "{role:?}");
                    // It covers the label (right-aligned to x + w - 5) when the label fits,
                    // and is tall enough for a line of digits.
                    assert!(pw > 0.0 && ph >= 9.0, "{role:?} {tw}");
                    if tw <= w - 7.0 - lw {
                        assert!(
                            l <= x + w - 5.0 - tw && l + pw >= x + w - 5.0,
                            "{role:?} {tw}"
                        );
                    }
                }
            }
        }
    }
}
