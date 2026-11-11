//! The time & sales pane (roadmap 5): the newest prints, newest on top.
//!
//! Each row: time (the display zone, ms), price, size, and the aggressor shown by colour *and* a marker
//! (▲ buyer lifted the offer, ▼ seller hit the bid, · unknown), so it never relies on colour
//! alone (docs/design.md §4). Prints at or above the large-print size get a highlighted row
//! and bold text.
//!
//! Prints come from the store's bounded ring (`lines_core::Tape`). Paint copies the visible
//! rows into a fixed buffer under a `try_lock` and draws them with cached glyphs: no
//! allocation.
//!
//! Typing a symbol (or Ctrl+K) opens the symbol picker, as on the chart. A slim header row names
//! the columns (time, price, size, side); its dividers resize them (drag, double-click to fit)
//! and right-clicking it shows or hides them (`crate::pane_columns`).

use std::fmt::Write as _;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, Styled,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::tape::write_tape_time;
use lines_core::{Aggressor, FixedStr, Print, Product, Qty};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;

use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pane_columns::{
    self, ColumnCmd, ColumnMenu, Columns, HeaderHit, Table, TapeCol, header_row_h,
};
use crate::picker_view::{PickerKey, PickerUi};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::wsdoc::{PaneDoc, TapeState};

const MAX_ROWS: usize = 256;
const ROW_H_100: f32 = 17.0;
/// [`ROW_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn row_h() -> f32 {
    crate::zoom::f(ROW_H_100)
}
const TEXT_PX: f32 = 12.0;
const PAD_100: f32 = 8.0;
/// [`PAD_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn pad() -> f32 {
    crate::zoom::f(PAD_100)
}

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
    /// The pane's width as last painted (0: not yet): which header parts fit.
    pane_w: f32,
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
    /// Column widths and visibility, and this frame's layout.
    cols: Columns,
    /// The table's bounds in the last frame (for header-row hits).
    bounds: Bounds<Pixels>,
}

impl EventEmitter<FeedChanged> for TapeView {}
impl EventEmitter<ColumnMenu> for TapeView {}

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
        let columns = self.cols.save(&TapeCol::ALL);
        doc.tape = (self.custom_large.is_some() || !columns.is_empty()).then(|| TapeState {
            large_print: self.custom_large.map(|q| q.0),
            columns,
        });
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    /// Prints only (depth below the touch doesn't change the tape), and the display zone.
    fn revision(&self) -> u64 {
        self.store
            .chart_revision()
            .wrapping_add(lines_core::display_tz::epoch() << 48)
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
    /// A new look (the Settings window's appearance).
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }
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
            pane_w: 0.0,
            large: opts
                .large_print
                .unwrap_or_else(|| default_large_print(&product)),
            custom_large: opts.large_print,
            product,
            prints: Vec::with_capacity(MAX_ROWS),
            glyphs: Glyphs::new(TEXT_PX),
            cols: Columns::new(Table::Tape),
            bounds: Bounds::default(),
        }
    }

    /// Restore saved view state (a custom large-print size, columns).
    pub fn restore(&mut self, state: &TapeState) {
        if let Some(q) = state.large_print.filter(|&q| q > 0) {
            self.custom_large = Some(Qty(q));
            self.large = Qty(q);
        }
        self.cols.load(&TapeCol::ALL, &state.columns);
    }

    /// A column command on this tape (decision 0018).
    pub fn column_command(
        &mut self,
        cmd: ColumnCmd,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        self.cols.apply(cmd)?;
        cx.notify();
        Ok(())
    }

    /// The columns (for the column menu).
    pub fn columns(&self) -> &Columns {
        &self.cols
    }

    /// A window position in table coordinates (from the header row's top-left).
    fn table_xy(&self, p: Point<Pixels>) -> (f32, f32) {
        (f(p.x - self.bounds.origin.x), f(p.y - self.bounds.origin.y))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.picker.close() {
            cx.notify();
        }
        let (x, y) = self.table_xy(ev.position);
        match self.cols.hit(x, y, ev.click_count) {
            HeaderHit::Divider(d) => {
                self.cols.start_drag(d, x);
                cx.stop_propagation();
                cx.notify();
            }
            HeaderHit::AutoFit(c) => {
                let _ = self.column_command(ColumnCmd::AutoFit(c), cx);
                cx.stop_propagation();
            }
            HeaderHit::Row | HeaderHit::Outside => {}
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.cols.drag.is_none() {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.cols.drag = None;
            cx.notify();
            return;
        }
        let (x, _) = self.table_xy(ev.position);
        if let Some(cmd) = self.cols.drag_to(x) {
            let _ = self.column_command(cmd, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.cols.drag.take().is_some() {
            cx.notify();
        }
    }

    /// Right-click on the header row: the column menu (the workspace shows it, window-wide).
    fn on_right_down(&mut self, ev: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = self.table_xy(ev.position);
        if self.cols.hit(x, y, 1) != HeaderHit::Outside {
            cx.emit(ColumnMenu(ev.position));
            cx.stop_propagation();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.picker.is_open() && pane::unavailable_key(self, ev, cx) {
            return;
        }
        let k = &ev.keystroke;
        let m = k.modifiers;
        if self.picker.is_open() {
            let today = pane::today(self.frozen, &self.store);
            let outcome = self
                .picker
                .key(ev, today, &crate::sources::pick(&self.feed, cx), cx);
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
        let outcome = self
            .picker
            .click(row, today, &crate::sources::pick(&self.feed, cx));
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
        let feed = crate::sources::retarget(&self.feed, target, cx)?;
        self.set_feed(feed.clone(), cx);
        cx.emit(FeedChanged(feed));
        Ok(())
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        let rows = (((f(bounds.size.height) - header_row_h()) / row_h()).floor() as usize)
            .clamp(1, MAX_ROWS);
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
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let w = f(bounds.size.width);
        let h = f(bounds.size.height);
        self.pane_w = w;
        window.paint_quad(fill(bounds, t.bg));
        let fmt = self.product.price;
        let qf = self.product.qty;
        let mut s = FixedStr::<48>::new();
        let zone = lines_core::display_tz::current();

        // What each column must hold this frame: its title and its widest text (large prints
        // are bold, the wider face). Columns are laid out from these: never narrower.
        self.cols.name_zone(
            self.prints
                .first()
                .map_or_else(lines_feed::now_utc, |p| p.time),
        );
        self.cols
            .measure_headers(&mut self.glyphs, &t, pad(), window);
        let n = self.prints.len().min(rows);
        for i in 0..n {
            let p = self.prints[i];
            s.clear();
            let _ = write_tape_time(p.time, &zone, &mut s);
            let tw = self.glyphs.width(s.as_str(), t.text_dim, false, window);
            self.cols.need(TapeCol::Time.index(), tw, pad());
            s.clear();
            let _ = fmt.write(p.price, &mut s);
            let pw = self.glyphs.width(s.as_str(), t.text_main, true, window);
            self.cols.need(TapeCol::Price.index(), pw, pad());
            s.clear();
            let _ = qf.write(p.qty, &mut s);
            let qw = self.glyphs.width(s.as_str(), t.text_main, true, window);
            self.cols.need(TapeCol::Size.index(), qw, pad());
        }
        let mw = self.glyphs.width("▲", t.text_main, false, window);
        self.cols.need(TapeCol::Side.index(), mw, pad() * 0.5);
        let side_w = mw + pad();
        // Narrow: every column narrows to what its numbers need, then the time gives way;
        // price, size and the side marker (direction is never color alone) stay whole.
        self.cols.layout_tape(w);
        self.cols.paint_header(
            &mut self.glyphs,
            &t,
            bounds,
            pad(),
            &[true, false, false, false],
            window,
            cx,
        );
        let cell = |c: TapeCol| self.cols.cell(c.index(), ox);
        let (time, price, qty, side) = (
            cell(TapeCol::Time),
            cell(TapeCol::Price),
            cell(TapeCol::Size),
            cell(TapeCol::Side),
        );
        let oy = top + header_row_h();
        let body = Bounds::new(point(px(ox), px(oy)), size(px(w), px(h - header_row_h())));
        if self.prints.is_empty() {
            self.glyphs.paint(
                "no trades yet",
                ox + pad(),
                oy + 4.0,
                row_h(),
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }
        for i in 0..n {
            let p = self.prints[i];
            let y = oy + i as f32 * row_h();
            let large = p.qty >= self.large;
            if large {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(y)), size(px(w), px(row_h()))),
                    t.large_print,
                ));
            }
            let (color, marker) = match p.aggressor {
                Some(Aggressor::Buy) => (t.up, "▲"),
                Some(Aggressor::Sell) => (t.down, "▼"),
                None => (t.text_main, "·"),
            };
            if let Some((x, _)) = time {
                s.clear();
                let _ = write_tape_time(p.time, &zone, &mut s);
                self.glyphs.paint(
                    s.as_str(),
                    x + pad(),
                    y,
                    row_h(),
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            if let Some((x, cw)) = price {
                s.clear();
                let _ = fmt.write(p.price, &mut s);
                self.glyphs.paint_right(
                    s.as_str(),
                    x + cw - pad(),
                    y,
                    row_h(),
                    color,
                    large,
                    window,
                    cx,
                );
            }
            if let Some((x, cw)) = qty {
                s.clear();
                let _ = qf.write(p.qty, &mut s);
                self.glyphs.paint_right(
                    s.as_str(),
                    x + cw - pad(),
                    y,
                    row_h(),
                    color,
                    large,
                    window,
                    cx,
                );
            }
            if let Some((x, cw)) = side {
                // At its own width from the left: beside the size even when the column takes
                // the leftover (the time gave way and the side column fills).
                self.glyphs.paint_right(
                    marker,
                    x + cw.min(side_w) - pad() * 0.5,
                    y,
                    row_h(),
                    color,
                    false,
                    window,
                    cx,
                );
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + row_h() - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        self.cols.paint_rules(&t, body, window);
    }

    fn header(&self, window: &Window, cx: &App) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(48);
        let mut word = None;
        // The source's tag sits between the contract and "Time & sales", in the secondary
        // color.
        let mut tag = None;
        let mut contract = String::new();
        if let Some(title) = pane::unavailable_title(&self.feed) {
            let _ = write!(left, "{title}  Time & sales");
        } else if let Some(d) = self.store.try_read() {
            word = pane::status_word(&d.status);
            contract = pane::contract_text(&d);
            tag = crate::sources::header_tag(&self.feed, cx).map(|s| pane::source_tag(&s));
            pane::keep_refused(&mut contract, &mut tag, self.feed.store().logon_refused());
            left.push_str(&contract);
        }
        let mut right = String::with_capacity(32);
        let mut q = FixedStr::<32>::new();
        let _ = self.product.qty.write(self.large, &mut q);
        if let Some(n) = pane::adopted_note(&self.feed) {
            let _ = write!(right, "{n}  ·  ");
        }
        let _ = write!(right, "large ≥ {q}");
        // A narrow header drops the source tag first; the contract and words never.
        let tw = pane::text_px(window, crate::zoom::ui(12.0));
        // A narrow pane shows only ⋯, so its header keeps only that gutter free.
        // An unavailable pane paints no canvas, so its last width may be stale: the full gutter.
        let gutter = if pane::unavailable_title(&self.feed).is_some() {
            pane::controls_gutter(crate::layout::PaneKind::Tape)
        } else {
            pane::controls_gutter_at(crate::layout::PaneKind::Tape, self.pane_w)
        };
        // In 100 % units, as `text_px` and the fit's gaps are (the gutter doesn't zoom); 0 or
        // less until the pane is laid out.
        let room = (self.pane_w - gutter) / crate::zoom::get();
        if let Some(tg) = &tag {
            // In 100 % units, as `text_px` and the fit's gaps are (the gutter doesn't zoom).
            let rest = pane::side_header_px(
                true, // a market pane always shows its link marker
                &contract,
                "Time & sales",
                word,
                self.store.stale_for().is_some(),
                &right,
                &tw,
            );
            if !pane::source_tag_fits(room, rest, tg, &tw) {
                tag = None;
            }
        }
        let has_contract = !contract.is_empty();
        let stale = self.store.stale_for().is_some();
        // The pane's word only when it fits whole beside everything that never gives way (the
        // right-hand text goes first); else left out, with its gap.
        // The left group's least width: all of it but the pane's word, which gives way first.
        let keep = pane::side_header_keep_px(true, &left, tag.as_deref(), word, None, stale, &tw);
        // The pane's word only when it fits whole beside all that; else left out, with its gap.
        let shows_name =
            has_contract && pane::side_header_shows_name(keep, tw("Time & sales"), room);
        div()
            .flex()
            .flex_row()
            .justify_between()
            .gap_2()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(gutter))
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(crate::zoom::ui(12.0))
            .child(
                pane::header_content()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .min_w(crate::zoom::ui(keep.ceil() + 1.0))
                            .child(pane::market_link_chip(self.common.link, &t))
                            // The contract (and the tag, when it fits) never gives way; the
                            // pane's name after them is the only item that shrinks: it gives way
                            // first, whole, its gap with it.
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .min_w_0()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_color(t.text_main)
                                            .relative()
                                            .children(crate::layout_rec::text_marker(&left))
                                            .child(left),
                                    )
                                    .children(tag.map(|tg| {
                                        div()
                                            .flex_shrink_0()
                                            .ml_2()
                                            .text_color(t.text_dim)
                                            .relative()
                                            .children(crate::layout_rec::text_marker(&tg))
                                            .child(tg)
                                    }))
                                    .when(shows_name, |d| {
                                        d.child(pane::whole_or_none_at(
                                            "Time & sales",
                                            t.text_main,
                                            false,
                                            Some(tw("Time & sales")),
                                        ))
                                    }),
                            )
                            // Every state but live is a word beside the contract that never
                            // gives way, as on the chart: an ended or failed feed must not
                            // read as live (docs/design.md §7).
                            .children(word.map(|w| {
                                div()
                                    .flex_shrink_0()
                                    .text_color(t.text_main)
                                    .relative()
                                    .children(crate::layout_rec::text_marker(w))
                                    .child(w)
                            }))
                            .children(pane::stale_badge(&self.store, &t)),
                    )
                    // Whole or not at all, never cut to a bare "l…".
                    .child(pane::whole_or_none(right, t.text_dim, true)),
            )
    }
}

impl TapeView {
    /// Taking typed text now (the symbol picker): the workspace leaves plain and Alt keys
    /// to it (`commands::runs_while_typing`).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open()
    }
}

impl Render for TapeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let t = self.theme;
        let today = pane::today(self.frozen, &self.store);
        let overlay = self.picker.overlay(
            &t,
            &crate::sources::pick_if(self.picker.is_open(), &self.feed, cx),
            today,
            Self::picker_click,
            cx,
        );
        let offer = self.common.offer_switch;
        let body = match pane::unavailable_body(&self.feed, &t, offer, cx) {
            Some(b) => b.into_any_element(),
            None => {
                let e = entity.clone();
                canvas(
                    // Resize cursors over the column dividers (last frame's layout).
                    move |bounds, window, cx| e.read(cx).cols.divider_hitboxes(bounds, window),
                    move |bounds, hitboxes, window, cx| {
                        pane_columns::set_divider_cursors(&hitboxes, window);
                        entity.update(cx, |view, cx| {
                            let was = view.pane_w;
                            view.paint(bounds, window, cx);
                            // The header's source tag fits by this width: lay it out again
                            // after a resize, even on a quiet feed.
                            if view.pane_w != was {
                                cx.notify();
                            }
                        });
                    },
                )
            }
            .size_full()
            .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header(window, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .child(body)
                    .when_some(overlay, |d, o| d.child(o)),
            )
    }
}

impl crate::picker_view::PickerHost for TapeView {
    fn picker_ui(&mut self) -> &mut crate::picker_view::PickerUi {
        &mut self.picker
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
