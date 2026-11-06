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
//!
//! Typing a symbol (or Ctrl+K) opens the symbol picker, as on the chart.

use std::fmt::Write as _;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Render, Styled, Window, canvas, div, fill, point,
    prelude::*, px, size,
};
use lines_core::tape::write_tape_time;
use lines_core::{Aggressor, FixedStr, Print, Product, Qty};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;

use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::wsdoc::{PaneDoc, TapeState};

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
    common: PaneCommon,
    picker: PickerUi,
    focus: FocusHandle,
    /// Screenshot mode: contract choices use the replay's date.
    frozen: bool,
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

    fn feed(&self) -> &PaneFeed {
        &self.feed
    }

    fn common(&mut self) -> &mut PaneCommon {
        &mut self.common
    }

    fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn save_state(&self, doc: &mut PaneDoc) {
        doc.tape = self.custom_large.map(|q| TapeState {
            large_print: Some(q.0),
        });
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.picker.close();
        self.store = feed.store().clone();
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
    pub fn new(
        feed: PaneFeed,
        theme: Theme,
        frozen: bool,
        opts: TapeOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = feed.store().clone();
        let product = store.read().product.clone();
        pane::watch_store(cx);
        TapeView {
            feed,
            common: PaneCommon::default(),
            picker: PickerUi::new(None, cx),
            focus: cx.focus_handle(),
            frozen,
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

    /// Restore saved view state (a custom large-print size).
    pub fn restore(&mut self, state: &TapeState) {
        if let Some(q) = state.large_print.filter(|&q| q > 0) {
            self.custom_large = Some(Qty(q));
            self.large = Qty(q);
        }
    }

    fn on_mouse_down(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.picker.close() {
            cx.notify();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if self.picker.is_open() {
            let today = pane::today(self.frozen, &self.store);
            let outcome = self.picker.key(ev, today, &self.feed.availability, cx);
            self.picked(outcome, cx);
        } else if m.control && !m.alt && k.key == "k" {
            self.picker.open("", cx);
        } else if let Some(letter) = k.key_char.as_deref().filter(|c| {
            !m.control
                && !m.alt
                && !m.platform
                && c.len() == 1
                && c.as_bytes()[0].is_ascii_alphabetic()
        }) {
            self.picker.open(letter, cx);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn picker_click(&mut self, row: usize, cx: &mut Context<Self>) {
        let today = pane::today(self.frozen, &self.store);
        let outcome = self.picker.click(row, today, &self.feed.availability);
        self.picked(outcome, cx);
    }

    fn picked(&mut self, outcome: PickerKey, cx: &mut Context<Self>) {
        let PickerKey::Confirm(target, product) = outcome else {
            return;
        };
        match self.switch_to(target, cx) {
            Ok(()) => self.picker.switched(product, cx),
            Err(e) => self.picker.error = Some(format!("{e:#}")),
        }
    }

    fn switch_to(&mut self, target: Target, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let feed = self.feed.retarget(&Feeds::registry(cx), target)?;
        self.set_feed(feed.clone(), cx);
        cx.emit(FeedChanged(feed));
        Ok(())
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        let rows = ((f(bounds.size.height) / ROW_H).floor() as usize).clamp(1, MAX_ROWS);
        if let Some(d) = self.store.try_read() {
            self.prints.clear();
            for p in d.tape.iter_newest().take(rows) {
                self.prints.push(*p); // within capacity: no allocation
            }
        }
        self.draw(bounds, rows, window, cx);
        pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
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
        if let Some(title) = pane::unavailable_title(&self.feed) {
            let _ = write!(left, "{title}  Time & sales");
        } else if let Some(d) = self.store.try_read() {
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
            .gap_2()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .children(pane::link_chip(self.common.link, &t))
                    .child(div().text_color(t.text_main).child(left))
                    .children(pane::stale_badge(&self.store, &t)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_dim)
                    .child(right),
            )
    }
}

impl Render for TapeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let t = self.theme;
        let today = pane::today(self.frozen, &self.store);
        let overlay =
            self.picker
                .overlay(&t, &self.feed.availability, today, Self::picker_click, cx);
        let body = match pane::unavailable_body(&self.feed, &t) {
            Some(b) => b.into_any_element(),
            None => canvas(
                |_, _, _| (),
                move |bounds, (), window, cx| {
                    entity.update(cx, |view, cx| view.paint(bounds, window, cx));
                },
            )
            .size_full()
            .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header())
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .child(body)
                    .when_some(overlay, |d, o| d.child(o)),
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
