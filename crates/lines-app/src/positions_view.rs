//! The Positions pane: every open position on every account (`positions.rs` builds the rows).
//! A row's Close and the header's Cancel all and Flatten all are the DOM's own commands for the
//! listed contracts, behind the same second-press guard. A click on a row switches the pane's
//! link group to it. Built from the trading cache's revisions, never the market's.

use std::sync::Arc;
use std::time::Instant;

use gpui::{
    Context, EventEmitter, FocusHandle, FontWeight, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, Styled, Window, div, prelude::*, px,
};
use lines_feed::store::ChartStore;
use lines_trading::Source;

use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pnl;
use crate::positions::{self as model, Row};
use crate::theme::Theme;
use crate::trading::{TradeCache, Trading};
use crate::wsdoc::PaneDoc;

const MIN_HIT: f32 = 20.0;
const ROW_H: f32 = 22.0;
/// Column widths, px: badge, contract, side and quantity, avg, last, P&L, stop, target, Close.
const COLS: [f32; 9] = [52.0, 150.0, 82.0, 80.0, 80.0, 96.0, 104.0, 104.0, 56.0];
const TITLES: [&str; 9] = [
    "Account", "Contract", "Side", "Avg", "Last", "Open P&L", "Stop", "Target", "",
];

pub struct PositionsView {
    feed: PaneFeed,
    common: PaneCommon,
    focus: FocusHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    cache: Option<Arc<TradeCache>>,
    seen_revision: u64,
    rows: Vec<Row>,
    /// A refusal or a "press again", until the next press.
    note: Option<String>,
}

impl EventEmitter<FeedChanged> for PositionsView {}

impl Pane for PositionsView {
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

    fn save_state(&self, _: &mut PaneDoc) {}

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    /// Every account's trading revision, the account list's and the P&L display's: atomics
    /// only, never the market's.
    fn revision(&self) -> u64 {
        let mut r = crate::trading::accounts_epoch().wrapping_mul(1_000_003);
        if let Some(c) = &self.cache {
            r = r.wrapping_add(c.any_revision());
        }
        r.wrapping_add(pnl::display_epoch() << 44)
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.store = feed.store().clone();
        self.feed = feed;
        cx.notify();
    }
}

impl PositionsView {
    pub fn new(feed: PaneFeed, theme: Theme, cx: &mut Context<Self>) -> Self {
        let store = feed.store().clone();
        pane::watch_store(cx);
        PositionsView {
            feed,
            common: PaneCommon::default(),
            focus: cx.focus_handle(),
            store,
            theme,
            cache: Trading::try_global(cx).map(|t| t.cache.clone()),
            seen_revision: u64::MAX,
            rows: Vec::new(),
            note: None,
        }
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Nothing here takes typed text.
    pub fn takes_text(&self) -> bool {
        false
    }

    /// The rows from the trading state; busy this instant: last time's.
    fn refresh(&mut self, cx: &gpui::App) -> bool {
        let Some(t) = Trading::try_global(cx) else {
            self.rows.clear();
            return true;
        };
        let accounts: Vec<_> = t.accounts().into_iter().map(|a| (a.id, a.badge)).collect();
        let Some(st) = t.cache.try_read() else {
            return false;
        };
        self.rows = model::rows(&st, &accounts);
        true
    }

    /// A press that sends: refused once after something moved under the pointer, as Flatten
    /// and Cancel all are everywhere (`popover::second_press_exit`), and said.
    fn guarded(&mut self, ev: &MouseDownEvent) -> bool {
        if crate::popover::second_press_exit(ev.click_count, ev.position) {
            self.note = Some("Press again: that press followed a change here".to_owned());
            return false;
        }
        self.note = None;
        true
    }

    fn send(&mut self, cmds: Vec<lines_trading::Command>, cx: &mut Context<Self>) {
        let Some(t) = Trading::try_global(cx) else {
            self.note = Some("trading isn't running".to_owned());
            return;
        };
        for cmd in cmds {
            t.submit(Source::Dom, Instant::now(), cmd);
        }
    }

    /// The link group goes to the row: a future's contract, an option's underlying.
    fn switch_to(&mut self, row: &Row, cx: &mut Context<Self>) {
        let target = model::target_of(row.contract);
        match self.feed.retarget(&Feeds::registry(cx), target) {
            Ok(feed) => {
                self.set_feed(feed.clone(), cx);
                cx.emit(FeedChanged(feed));
            }
            Err(e) => self.note = Some(format!("Can't show it: {e:#}")),
        }
    }

    fn button(&self, id: SharedString, word: &'static str) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(MIN_HIT))
            .min_w(px(MIN_HIT))
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(t.control_edge)
            .text_color(t.text_main)
            .cursor_pointer()
            .child(word)
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let (count, total) = model::totals(&self.rows);
        let money = crate::picker_view::settings(cx).pnl_display.money();
        let mut sum = String::new();
        if money && count > 0 {
            let _ = pnl::write_money(&mut sum, total);
        }
        let exit = |flatten: bool, this: &Self, cx: &mut Context<Self>| {
            let (id, word) = if flatten {
                ("positions-flatten", "Flatten all")
            } else {
                ("positions-cancel", "Cancel all")
            };
            crate::popover::always_live(this.button(id.into(), word).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if this.guarded(ev) {
                        crate::popover::close_any(cx);
                        let cmds = model::exit_all(&this.rows, flatten);
                        this.send(cmds, cx);
                    }
                    cx.notify();
                }),
            ))
        };
        div()
            .flex()
            .flex_row()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(pane::controls_gutter(
                crate::layout::PaneKind::Positions,
            )))
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(
                pane::header_content()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .overflow_hidden()
                            .children(pane::link_chip(self.common.link, &t))
                            .child(div().flex_shrink_0().child("Positions"))
                            .children((count > 0).then(|| {
                                div()
                                    .flex_shrink_0()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("{count} open"))
                            }))
                            .children(
                                (!sum.is_empty()).then(|| {
                                    div().flex_shrink_0().child(format!("Open P&L {sum}"))
                                }),
                            )
                            .children(self.note.clone().map(|n| {
                                div().min_w_0().truncate().text_color(t.text_main).child(n)
                            })),
                    )
                    // Present while anything is listed, so they never appear under a pointer
                    // mid-run of presses that had nothing to act on.
                    .when(count > 0, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .flex_shrink_0()
                                .child(exit(false, self, cx))
                                .child(exit(true, self, cx)),
                        )
                    }),
            )
    }

    fn cell(width: f32, right: bool) -> gpui::Div {
        let d = div()
            .flex_shrink_0()
            .w(px(width))
            .px_1()
            .overflow_hidden()
            .whitespace_nowrap();
        if right { d.flex().justify_end() } else { d }
    }

    fn titles(&self) -> impl IntoElement {
        let t = self.theme;
        let mut row = div()
            .flex()
            .flex_row()
            .h(px(ROW_H))
            .items_center()
            .px_1()
            .text_color(t.text_dim)
            .border_b_1()
            .border_color(t.grid);
        for (i, title) in TITLES.iter().enumerate() {
            row = row.child(Self::cell(COLS[i], (3..8).contains(&i)).child(*title));
        }
        row
    }

    fn row_el(&self, n: usize, money: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let r = self.rows[n].clone();
        let product = lines_feed::catalog::get(r.contract.product);
        let mut name = String::new();
        if let Some(p) = &product {
            name.push_str(r.contract.label(p).as_str());
        }
        let mut side = String::new();
        let _ = model::write_side(&mut side, r.qty);
        let price = |v: Option<f64>| {
            let mut s = String::new();
            if let (Some(v), Some(p)) = (v, &product) {
                let _ = crate::trade_bar::write_avg(&mut s, p, v);
            }
            s
        };
        let mut pnl = String::new();
        if money {
            let _ = pnl::write_money(&mut pnl, r.pnl);
        }
        let underlying = r
            .contract
            .option_key()
            .and_then(|_| lines_feed::catalog::get(r.contract.product));
        let exit = |e| {
            let mut s = String::new();
            let _ = model::write_exit(&mut s, e, product.as_ref(), underlying.as_ref());
            s
        };
        let side_color = if r.qty > 0 { t.up } else { t.down };
        let pnl_color = if r.pnl.0 < 0 { t.down } else { t.text_main };
        let row = r.clone();
        let close = row.clone();
        div()
            .id(SharedString::from(format!("position-{n}")))
            .flex()
            .flex_row()
            .h(px(ROW_H))
            .items_center()
            .px_1()
            .border_b_1()
            .border_color(t.grid)
            .cursor_pointer()
            .hover(|s| s.bg(t.panel))
            .child(
                Self::cell(COLS[0], false).child(
                    div()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(t.control_edge)
                        .text_size(px(10.0))
                        .child(r.badge),
                ),
            )
            .child(Self::cell(COLS[1], false).child(name))
            .child(
                Self::cell(COLS[2], false)
                    .text_color(side_color)
                    .child(side),
            )
            .child(Self::cell(COLS[3], true).child(price(r.avg)))
            .child(Self::cell(COLS[4], true).child(price(r.last)))
            .child(Self::cell(COLS[5], true).text_color(pnl_color).child(pnl))
            .child(Self::cell(COLS[6], true).child(exit(r.stop)))
            .child(Self::cell(COLS[7], true).child(exit(r.target)))
            .child(
                Self::cell(COLS[8], true).child(
                    self.button(SharedString::from(format!("position-close-{n}")), "Close")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                if this.guarded(ev) {
                                    this.send(vec![model::close(&close)], cx);
                                }
                                cx.notify();
                            }),
                        ),
                ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    // A double-click's second press does nothing more.
                    if ev.click_count <= 1 {
                        this.switch_to(&row, cx);
                    }
                    cx.notify();
                }),
            )
    }
}

impl Render for PositionsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.refresh(cx) {
            // The trading cache was busy this instant: draw again on the next poll.
            self.seen_revision = u64::MAX;
        }
        self.common.paints += 1;
        let t = self.theme;
        let money = crate::picker_view::settings(cx).pnl_display.money();
        let rows: Vec<_> = (0..self.rows.len())
            .map(|n| self.row_el(n, money, cx).into_any_element())
            .collect();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_size(px(12.0))
            .text_color(t.text_main)
            .child(self.header(cx))
            .child(
                div()
                    .id("positions-table")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_focus(&self.focus)
                    .child(self.titles())
                    .children(rows),
            )
    }
}

/// How many positions are open on every listed account: the workspace's `N open`.
pub fn open_count(cx: &gpui::App) -> Option<usize> {
    let t = Trading::try_global(cx)?;
    let accounts = t.accounts();
    let st = t.cache.try_read()?;
    Some(
        accounts
            .iter()
            .map(|a| st.positions_of(&a.id).filter(|p| p.qty != 0).count())
            .sum(),
    )
}
