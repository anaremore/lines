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
//! - a stop is prefixed `S`; a **ghost** (dragged, not yet confirmed) is a dashed accent
//!   outline at the new price while the order stays at its old one;
//! - the **position** is a bracket of lines around the price cell of the average price's row
//!   plus `L n` / `S n` in the orders column;
//! - a **rejection** is a warning bar across its row with the reason, for a few seconds.

use std::time::Instant;

use gpui::{App, Bounds, Hsla, Pixels, Window, fill, point, px, size};
use lines_core::ladder::LadderRow;
use lines_core::{ContractKey, Price};
use lines_trading::{AccountId, OrderId, Side};

use crate::dom_trade::{Ghost, MarkStyle, OrderMark, order_mark, position_row, row_of};
use crate::text::Glyphs;
use crate::theme::Theme;
use crate::trading::TradeCache;

/// A rejection to show on its row.
#[derive(Clone, Copy, Debug)]
struct RowFlash {
    price: Price,
    /// Byte range of the message in `OrdersLayer::text`.
    start: usize,
    end: usize,
}

/// An order being dragged: where it was grabbed and where the pointer is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragOrder {
    pub order: OrderId,
    pub to: Price,
}

pub struct OrdersLayer {
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
                let start = self.text.len();
                self.text.push_str(&f.message);
                self.flashes.push(RowFlash {
                    price,
                    start,
                    end: self.text.len(),
                });
            }
        }
        self.ghosts.retain(|g| {
            match crate::dom_trade::ghost_outcome(g, s.order(g.order), s.result(g.command)) {
                crate::dom_trade::GhostOutcome::Pending => true,
                crate::dom_trade::GhostOutcome::Acked => false,
                crate::dom_trade::GhostOutcome::Rejected(why) => {
                    refused.push((g.price, why));
                    false
                }
            }
        });
        refused
    }

    /// The order drawn on the row containing `price` (the first; for drag and right-click).
    pub fn order_at(&self, price: Price, units: i64) -> Option<&OrderMark> {
        let row = row_of(price.0, units);
        self.marks.iter().find(|m| row_of(m.price.0, units) == row)
    }

    /// Paint over the ladder. `orders`: the orders column's cell (x, w), if shown; `price`:
    /// the price column's cell; `x0..x1`: the ladder's width (for rejection bars).
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
        (x0, x1): (f32, f32),
        oy: f32,
        row_h: f32,
        hair: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let rect =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        let side_colour = |s: Side| if s == Side::Buy { t.up } else { t.down };
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
            // Orders on this row: up to two, side by side.
            let mut on_row = self
                .marks
                .iter()
                .filter(|m| row_of(m.price.0, units) == row);
            let first = on_row.next();
            let second = on_row.next();
            let more = on_row.next().is_some();
            let n = first.is_some() as usize + second.is_some() as usize;
            for (k, m) in [first, second].into_iter().flatten().enumerate() {
                let w = (ow - 2.0) / n as f32;
                let (x, yy, h) = (ox + 1.0 + k as f32 * w, y + 2.0, row_h - 4.0);
                let c = side_colour(m.side);
                let text_colour = match m.style {
                    MarkStyle::Working => {
                        window.paint_quad(fill(rect(x, yy, w - 1.0, h), c));
                        t.bg
                    }
                    MarkStyle::Partial => {
                        let share = (m.filled.0 as f32 / m.qty.0.max(1) as f32).clamp(0.0, 1.0);
                        window.paint_quad(fill(rect(x, yy, (w - 1.0) * share, h), c));
                        outline(window, rect(x, yy, w - 1.0, h), c, hair, false);
                        t.text_main
                    }
                    MarkStyle::Pending => {
                        outline(window, rect(x, yy, w - 1.0, h), c, hair, false);
                        c
                    }
                    MarkStyle::Held => {
                        outline(window, rect(x, yy, w - 1.0, h), t.text_dim, hair, true);
                        t.text_dim
                    }
                };
                buf.clear();
                let _ = if m.stop {
                    std::fmt::Write::write_fmt(&mut buf, format_args!("S"))
                } else {
                    Ok(())
                };
                let _ = if m.style == MarkStyle::Partial {
                    std::fmt::Write::write_fmt(&mut buf, format_args!("{}/{}", m.filled.0, m.qty.0))
                } else {
                    std::fmt::Write::write_fmt(&mut buf, format_args!("{}", m.qty.0 - m.filled.0))
                };
                if more && k == 1 {
                    let _ = std::fmt::Write::write_str(&mut buf, "+");
                }
                glyphs.paint_right(
                    buf.as_str(),
                    x + w - 3.0,
                    y,
                    row_h,
                    text_colour,
                    true,
                    window,
                    cx,
                );
            }
            if n == 0
                && pos_row == Some(row)
                && let Some((q, _)) = self.position
            {
                buf.clear();
                let _ = std::fmt::Write::write_fmt(
                    &mut buf,
                    format_args!("{}{}", if q > 0 { "L" } else { "S" }, q.abs()),
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
            // Ghosts and the order being dragged: a dashed accent outline at the new price.
            let ghost_here = self.ghosts.iter().any(|g| row_of(g.price.0, units) == row)
                || self.drag.is_some_and(|d| row_of(d.to.0, units) == row);
            if ghost_here {
                outline(
                    window,
                    rect(ox + 1.0, y + 2.0, ow - 3.0, row_h - 4.0),
                    t.accent,
                    hair,
                    true,
                );
            }
        }
        // Rejections: a warning bar across their row with the reason.
        for f in &self.flashes {
            let row = row_of(f.price.0, units);
            let Some(i) = rows.iter().position(|r| row_of(r.price.0, units) == row) else {
                continue;
            };
            let y = oy + i as f32 * row_h;
            window.paint_quad(fill(rect(x0, y, x1 - x0, row_h), t.warn));
            glyphs.paint(
                &self.text[f.start..f.end],
                x0 + 4.0,
                y,
                row_h,
                t.on_warn,
                true,
                window,
                cx,
            );
        }
    }
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
    let dash: f32 = 3.0;
    let mut dx = 0.0;
    while dx < w {
        let seg = dash.min(w - dx);
        window.paint_quad(fill(r(x + dx, y, seg, lw), c));
        window.paint_quad(fill(r(x + dx, y + h - lw, seg, lw), c));
        dx += dash * 2.0;
    }
    let mut dy = 0.0;
    while dy < h {
        let seg = dash.min(h - dy);
        window.paint_quad(fill(r(x, y + dy, lw, seg), c));
        window.paint_quad(fill(r(x + w - lw, y + dy, lw, seg), c));
        dy += dash * 2.0;
    }
}
