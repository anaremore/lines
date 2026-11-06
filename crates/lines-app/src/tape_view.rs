//! The time & sales pane (roadmap 5): the newest prints, newest on top.
//!
//! Each row: time (UTC, ms), price, size, and the aggressor shown by colour *and* a marker
//! (▲ buyer lifted the offer, ▼ seller hit the bid, · unknown), so it never relies on colour
//! alone (docs/design.md §4). Prints at or above the large-print size get a highlighted row
//! and bold text.
//!
//! Prints come from the store's bounded ring (`lines_core::Tape`). Paint copies the visible
//! rows into a fixed buffer under a `try_lock` and draws them with cached glyphs: no
//! allocation.

use std::fmt::Write as _;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, IntoElement, ParentElement, Pixels, Render, Styled, Window,
    canvas, div, fill, point, px, size,
};
use lines_core::tape::write_tape_time;
use lines_core::{Aggressor, FixedStr, Print, Product, Qty};
use lines_feed::store::ChartStore;

use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane};
use crate::text::{Glyphs, f};
use crate::theme::Theme;

const MAX_ROWS: usize = 256;
const ROW_H: f32 = 17.0;
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 8.0;

#[derive(Clone, Debug, Default)]
pub struct TapeOptions {
    /// Prints of at least this size are highlighted. `None`: 10 contracts, or 1 unit of a
    /// fractional product.
    pub large_print: Option<Qty>,
}

/// The default large-print threshold for a product.
pub fn default_large_print(p: &Product) -> Qty {
    if p.qty.decimals > 0 {
        Qty(10i64.pow(p.qty.decimals as u32))
    } else {
        Qty(10)
    }
}

pub struct TapeView {
    feed: PaneFeed,
    store: Arc<ChartStore>,
    theme: Theme,
    seen_revision: u64,
    product: Product,
    large: Qty,
    custom_large: Option<Qty>,
    prints: Vec<Print>,
    glyphs: Glyphs,
}

impl EventEmitter<FeedChanged> for TapeView {}

impl Pane for TapeView {
    fn store(&self) -> &Arc<ChartStore> {
        &self.store
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.store = feed.sub.store().clone();
        self.product = self.store.read().product.clone();
        self.large = self
            .custom_large
            .unwrap_or_else(|| default_large_print(&self.product));
        self.feed = feed;
        self.prints.clear();
        self.seen_revision = u64::MAX;
        cx.notify();
    }
}

impl TapeView {
    pub fn new(feed: PaneFeed, theme: Theme, opts: TapeOptions, cx: &mut Context<Self>) -> Self {
        let store = feed.sub.store().clone();
        let product = store.read().product.clone();
        pane::watch_store(cx);
        TapeView {
            feed,
            store,
            theme,
            seen_revision: u64::MAX,
            large: opts
                .large_print
                .unwrap_or_else(|| default_large_print(&product)),
            custom_large: opts.large_print,
            product,
            prints: Vec::with_capacity(MAX_ROWS),
            glyphs: Glyphs::new(TEXT_PX),
        }
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let rows = ((f(bounds.size.height) / ROW_H).floor() as usize).clamp(1, MAX_ROWS);
        if let Some(d) = self.store.try_read() {
            self.prints.clear();
            for p in d.tape.iter_newest().take(rows) {
                self.prints.push(*p); // within capacity: no allocation
            }
        }
        self.draw(bounds, rows, window, cx);
    }

    fn draw(&mut self, bounds: Bounds<Pixels>, rows: usize, window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, oy) = (f(bounds.origin.x), f(bounds.origin.y));
        let w = f(bounds.size.width);
        window.paint_quad(fill(bounds, t.bg));
        if self.prints.is_empty() {
            self.glyphs.paint(
                "no trades yet",
                ox + PAD,
                oy + 4.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }
        // Columns: time | price | size | marker.
        let marker_w = 18.0;
        let time_r = ox + PAD + (w * 0.36).max(84.0);
        let size_r = ox + w - PAD - marker_w;
        let price_r = time_r + (size_r - time_r) * 0.55;
        let fmt = self.product.price;
        let qf = self.product.qty;
        let mut s = FixedStr::<48>::new();
        for i in 0..self.prints.len().min(rows) {
            let p = self.prints[i];
            let y = oy + i as f32 * ROW_H;
            let large = p.qty >= self.large;
            if large {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(y)), size(px(w), px(ROW_H))),
                    t.large_print,
                ));
            }
            let (color, marker) = match p.aggressor {
                Some(Aggressor::Buy) => (t.up, "▲"),
                Some(Aggressor::Sell) => (t.down, "▼"),
                None => (t.text_main, "·"),
            };
            s.clear();
            let _ = write_tape_time(p.time, &mut s);
            self.glyphs.paint(
                s.as_str(),
                ox + PAD,
                y,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            s.clear();
            let _ = fmt.write(p.price, &mut s);
            self.glyphs
                .paint_right(s.as_str(), price_r, y, ROW_H, color, large, window, cx);
            s.clear();
            let _ = qf.write(p.qty, &mut s);
            self.glyphs
                .paint_right(s.as_str(), size_r, y, ROW_H, color, large, window, cx);
            self.glyphs
                .paint_right(marker, ox + w - PAD, y, ROW_H, color, false, window, cx);
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + ROW_H - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
    }

    fn header(&self) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(48);
        if let Some(d) = self.store.try_read() {
            let _ = write!(left, "{}  Time & sales", pane::contract_text(&d));
        }
        let mut right = String::with_capacity(32);
        let mut q = FixedStr::<32>::new();
        let _ = self.product.qty.write(self.large, &mut q);
        let _ = write!(right, "large ≥ {q}");
        div()
            .flex()
            .flex_row()
            .justify_between()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(div().text_color(t.text_main).child(left))
            .child(div().text_color(t.text_dim).child(right))
    }
}

impl Render for TapeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(self.theme.bg)
            .child(self.header())
            .child(
                div().flex_1().size_full().child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), window, cx| {
                            entity.update(cx, |view, cx| view.paint(bounds, window, cx));
                        },
                    )
                    .size_full(),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::catalog;

    #[test]
    fn large_print_defaults_follow_the_quantity_unit() {
        assert_eq!(
            default_large_print(&catalog::get(catalog::NQ).unwrap()),
            Qty(10)
        );
        let btc = catalog::get(catalog::BTC_USD).unwrap();
        assert_eq!(
            default_large_print(&btc),
            Qty(10i64.pow(btc.qty.decimals as u32))
        );
    }
}
