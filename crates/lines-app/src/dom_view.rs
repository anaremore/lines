//! The DOM pane (roadmap 5, read-only): a price ladder of fixed rows indexed by integer ticks.
//!
//! Columns, left to right: a reserved slot for the trader's own orders (empty until order
//! entry, milestone 7), bid size, price, ask size, traded volume. Sizes are numbers with a
//! subtle depth bar behind them; numbers use tabular digits, right-aligned.
//!
//! Frame flow: the pane polls its store's revision (8 ms) and redraws only on change, so a
//! burst of depth updates coalesces into one frame (decision 0001). Paint `try_lock`s the
//! store, builds the visible rows into a fixed buffer (`lines_core::ladder`, allocation-free,
//! work bounded by the visible price range) and draws them with cached glyphs (`crate::text`).
//!
//! Centring follows the inside market, but never while the pointer is over the ladder
//! (docs/design.md §7); C re-centres, the wheel moves by rows, G / Shift+G change the price
//! grouping (1, 2, 5, 10, 25 ticks, or any value via `--dom-grouping`). Ctrl+K opens the
//! symbol picker (letters are DOM commands here, so typing a symbol needs Ctrl+K first).

use std::fmt::Write as _;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Render, ScrollEvent, Styled, Window, canvas, div, fill,
    point, prelude::*, px, size,
};
use lines_core::ladder::{
    self, GROUPING_PRESETS, Inside, LadderRow, LadderStats, LadderView, build_ladder, inside_row,
};
use lines_core::{FixedStr, Product};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;

use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::wsdoc::{DomState, PaneDoc};

/// Most rows a ladder shows (the row buffer is allocated once at this size).
const MAX_ROWS: usize = 256;
const ROW_H: f32 = 18.0;
const TEXT_PX: f32 = 12.0;
/// Reserved for the trader's working orders (milestone 7).
const ORDERS_W: f32 = 22.0;
const PAD: f32 = 6.0;

#[derive(Clone, Debug)]
pub struct DomOptions {
    /// Ticks per row.
    pub grouping: u32,
}

impl Default for DomOptions {
    fn default() -> Self {
        DomOptions { grouping: 1 }
    }
}

pub struct DomView {
    feed: PaneFeed,
    common: PaneCommon,
    picker: PickerUi,
    store: Arc<ChartStore>,
    theme: Theme,
    /// Screenshot mode: no pointer, nothing time-dependent.
    frozen: bool,
    seen_revision: u64,
    product: Product,
    has_depth: bool,
    grouping: u32,
    view: LadderView,
    rows: Vec<LadderRow>,
    stats: LadderStats,
    inside: Inside,
    bounds: Bounds<Pixels>,
    /// Unapplied wheel movement, in rows.
    scroll_rows: f32,
    focus: FocusHandle,
    glyphs: Glyphs,
}

impl EventEmitter<FeedChanged> for DomView {}

impl Pane for DomView {
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
        doc.dom = Some(DomState {
            grouping: self.grouping,
        });
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.picker.close();
        self.store = feed.store().clone();
        {
            let d = self.store.read();
            self.product = d.product.clone();
            self.has_depth = d.capabilities.depth;
        }
        self.feed = feed;
        self.view = LadderView::default();
        self.rows.clear();
        self.stats = LadderStats::default();
        self.inside = Inside::Empty;
        self.seen_revision = u64::MAX;
        cx.notify();
    }
}

impl DomView {
    pub fn new(
        feed: PaneFeed,
        theme: Theme,
        frozen: bool,
        opts: DomOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = feed.store().clone();
        let (product, has_depth) = {
            let d = store.read();
            (d.product.clone(), d.capabilities.depth)
        };
        pane::watch_store(cx);
        DomView {
            feed,
            common: PaneCommon::default(),
            picker: PickerUi::new(None, cx),
            store,
            theme,
            frozen,
            seen_revision: u64::MAX,
            product,
            has_depth,
            grouping: opts.grouping.max(1),
            view: LadderView::default(),
            rows: Vec::with_capacity(MAX_ROWS),
            stats: LadderStats::default(),
            inside: Inside::Empty,
            bounds: Bounds::default(),
            scroll_rows: 0.0,
            focus: cx.focus_handle(),
            glyphs: Glyphs::new(TEXT_PX),
        }
    }

    /// Restore saved view state (price grouping).
    pub fn restore(&mut self, state: &DomState) {
        self.set_grouping(state.grouping.clamp(1, 10_000));
    }

    fn picker_click(&mut self, row: usize, cx: &mut Context<Self>) {
        let today = pane::today(self.frozen, &self.store);
        let outcome = self.picker.click(row, today, &self.feed.availability);
        self.picked(outcome, cx);
    }

    /// The picker confirmed a target: switch and tell linked panes.
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

    fn row_units(&self) -> i64 {
        self.grouping as i64 * self.product.price.tick
    }

    fn visible_rows(&self) -> usize {
        ((f(self.bounds.size.height) / ROW_H).floor() as usize).clamp(1, MAX_ROWS)
    }

    // ---- input ---------------------------------------------------------------------

    fn on_mouse_down(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.picker.close() {
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dy = f(ev.delta.pixel_delta(window.line_height()).y);
        // Wheel up shows higher prices.
        self.scroll_rows += dy / ROW_H;
        let whole = self.scroll_rows.trunc();
        if whole != 0.0 {
            self.scroll_rows -= whole;
            self.view.scroll(whole as i64);
            cx.notify();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if self.picker.is_open() {
            let today = pane::today(self.frozen, &self.store);
            let outcome = self.picker.key(ev, today, &self.feed.availability, cx);
            self.picked(outcome, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if k.modifiers.control && !k.modifiers.alt && k.key == "k" {
            self.picker.open("", cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            return;
        }
        match k.key.as_str() {
            "c" => {
                let units = self.row_units();
                let inside = self
                    .store
                    .try_read()
                    .and_then(|d| inside_row(&d.book, d.last_trade, units));
                self.view.recentre(inside);
            }
            "g" => self.set_grouping(next_grouping(self.grouping, k.modifiers.shift)),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn set_grouping(&mut self, g: u32) {
        let old = self.row_units();
        self.grouping = g.max(1);
        self.view.regroup(old, self.row_units());
    }

    // ---- paint ---------------------------------------------------------------------

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        let rows = self.visible_rows();
        // Hit-tested here, after layout, from the pointer's actual position: nothing the
        // trader aims at moves while the pointer is over the ladder.
        let hovered =
            !self.frozen && window.is_window_hovered() && bounds.contains(&window.mouse_position());
        let units = self.row_units();
        if let Some(d) = self.store.try_read() {
            let inside = inside_row(&d.book, d.last_trade, units);
            self.view.follow(inside, rows, hovered);
            self.inside = ladder::inside(&d.book);
            match self.view.top_row(rows) {
                Some(top) => {
                    self.stats = build_ladder(
                        &d.book,
                        &d.vap,
                        d.last_trade,
                        units,
                        top,
                        rows,
                        &mut self.rows,
                    )
                }
                None => self.rows.clear(),
            }
        }
        self.draw(bounds, window, cx);
        pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
    }

    fn draw(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, oy) = (f(bounds.origin.x), f(bounds.origin.y));
        let w = f(bounds.size.width);
        let h = f(bounds.size.height);
        let rect =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        window.paint_quad(fill(bounds, t.bg));
        // Column geometry.
        let vol_w = (w * 0.2).max(44.0);
        let col = ((w - ORDERS_W - vol_w) / 3.0).max(1.0);
        let bid_x = ox + ORDERS_W;
        let price_x = bid_x + col;
        let ask_x = price_x + col;
        let vol_x = ask_x + col;
        window.paint_quad(fill(rect(ox, oy, ORDERS_W, h), t.panel));

        if self.rows.is_empty() {
            let msg = if self.has_depth {
                "no depth yet"
            } else {
                "no depth from this source"
            };
            self.glyphs.paint(
                msg,
                bid_x + PAD,
                oy + PAD,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }

        let fmt = self.product.price;
        let qf = self.product.qty;
        let (max_size, max_vol) = (self.stats.max_size.0.max(1), self.stats.max_volume.0.max(1));
        let flagged = matches!(self.inside, Inside::Locked | Inside::Crossed);
        let mut s = FixedStr::<48>::new();
        for i in 0..self.rows.len() {
            let r = self.rows[i];
            let y = oy + i as f32 * ROW_H;
            if y + ROW_H > oy + h + 0.5 {
                break;
            }
            // Bid: inside tint, then a bar growing from the price column outwards.
            if r.best_bid {
                window.paint_quad(fill(rect(bid_x, y, col, ROW_H), t.inside_bid));
            }
            if r.bid.0 > 0 {
                let bw = (col * (r.bid.0 as f32 / max_size as f32)).max(hair);
                window.paint_quad(fill(
                    rect(price_x - bw, y + 1.0, bw, ROW_H - 2.0),
                    t.bid_bar,
                ));
                s.clear();
                let _ = qf.write(r.bid, &mut s);
                let bold = r.best_bid;
                self.glyphs.paint_right(
                    s.as_str(),
                    price_x - PAD,
                    y,
                    ROW_H,
                    t.text_main,
                    bold,
                    window,
                    cx,
                );
            }
            if r.best_ask {
                window.paint_quad(fill(rect(ask_x, y, col, ROW_H), t.inside_ask));
            }
            if r.ask.0 > 0 {
                let aw = (col * (r.ask.0 as f32 / max_size as f32)).max(hair);
                window.paint_quad(fill(rect(ask_x, y + 1.0, aw, ROW_H - 2.0), t.ask_bar));
                s.clear();
                let _ = qf.write(r.ask, &mut s);
                let bold = r.best_ask;
                self.glyphs.paint_right(
                    s.as_str(),
                    vol_x - PAD,
                    y,
                    ROW_H,
                    t.text_main,
                    bold,
                    window,
                    cx,
                );
            }
            // Price: the last trade's row is tagged like the chart's last-price tag.
            let (pfg, bold) = if r.last {
                window.paint_quad(fill(rect(price_x, y, col, ROW_H), t.last));
                (t.on_last, true)
            } else {
                (t.text_main, r.best_bid || r.best_ask)
            };
            s.clear();
            let _ = fmt.write(r.price, &mut s);
            self.glyphs
                .paint_right(s.as_str(), ask_x - PAD, y, ROW_H, pfg, bold, window, cx);
            // A locked or crossed book: the inside rows are outlined, not only tinted.
            if flagged && (r.best_bid || r.best_ask) {
                window.paint_quad(fill(rect(bid_x, y, vol_x - bid_x, hair), t.text_main));
                window.paint_quad(fill(
                    rect(bid_x, y + ROW_H - hair, vol_x - bid_x, hair),
                    t.text_main,
                ));
            }
            // Traded volume.
            if r.volume.0 > 0 {
                let vw = ((vol_w - PAD) * (r.volume.0 as f32 / max_vol as f32)).max(hair);
                window.paint_quad(fill(rect(vol_x, y + 2.0, vw, ROW_H - 4.0), t.volume_bar));
                s.clear();
                let _ = qf.write(r.volume, &mut s);
                self.glyphs.paint_right(
                    s.as_str(),
                    ox + w - PAD,
                    y,
                    ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            window.paint_quad(fill(
                rect(bid_x, y + ROW_H - hair, w - ORDERS_W, hair),
                t.grid,
            ));
        }
        // Column rules.
        for x in [price_x, ask_x, vol_x] {
            window.paint_quad(fill(rect(x, oy, hair, h), t.grid));
        }
    }

    fn header(&self) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(64);
        let mut right = String::with_capacity(64);
        if let Some(title) = pane::unavailable_title(&self.feed) {
            let _ = write!(left, "{title}  DOM");
        } else if let Some(d) = self.store.try_read() {
            let _ = write!(left, "{}  DOM", pane::contract_text(&d));
        }
        let ticks = if self.grouping == 1 { "tick" } else { "ticks" };
        let _ = write!(right, "{} {ticks}/row", self.grouping);
        let state = match self.inside {
            Inside::Locked => Some("locked"),
            Inside::Crossed => Some("crossed"),
            Inside::OneSided => Some("one-sided"),
            _ => None,
        };
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
                    .flex()
                    .flex_row()
                    .gap_2()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(t.text_dim)
                    .when_some(state, |d, s| {
                        // The book's state is text, not only colour.
                        d.child(
                            div()
                                .text_color(t.text_main)
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(s),
                        )
                    })
                    .child(right),
            )
    }
}

/// The next grouping preset after (or before, `back`) `g`; custom values rejoin the presets.
pub fn next_grouping(g: u32, back: bool) -> u32 {
    if back {
        GROUPING_PRESETS
            .iter()
            .rev()
            .copied()
            .find(|&p| p < g)
            .unwrap_or(GROUPING_PRESETS[GROUPING_PRESETS.len() - 1])
    } else {
        GROUPING_PRESETS
            .iter()
            .copied()
            .find(|&p| p > g)
            .unwrap_or(GROUPING_PRESETS[0])
    }
}

impl Render for DomView {
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
                    .on_scroll(cx.listener(Self::on_scroll))
                    .child(body)
                    .when_some(overlay, |d, o| d.child(o)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_cycles_through_presets() {
        assert_eq!(next_grouping(1, false), 2);
        assert_eq!(next_grouping(25, false), 1);
        assert_eq!(next_grouping(3, false), 5, "custom rejoins the presets");
        assert_eq!(next_grouping(1, true), 25);
        assert_eq!(next_grouping(10, true), 5);
    }
}
