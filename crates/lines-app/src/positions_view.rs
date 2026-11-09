//! The Positions pane: every open position on every account (`positions.rs` builds the rows).
//! A row's Flatten and the header's Flatten all are the DOM's own Flatten for the listed
//! account and contract, behind the same second-press guard; Flatten all asks first. A
//! Flatten that can't close is said in the header, as the DOM and the chain say it. A click on
//! a row switches the pane's link group to it. Built from the trading cache's revisions, never
//! the market's, and once a second for a P&L going stale.

use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, Context, EventEmitter, FocusHandle, FontWeight, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, Styled, Window, canvas, div, prelude::*,
    px,
};
use lines_core::ContractKey;
use lines_feed::store::ChartStore;
use lines_trading::{AccountId, CommandId, OrderId, OrderState, Role, Source};

use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pnl;
use crate::positions::{self as model, Col, Row};
use crate::theme::Theme;
use crate::trading::{TradeCache, Trading};
use crate::wsdoc::PaneDoc;

const MIN_HIT: f32 = 20.0;
const ROW_H: f32 = 22.0;

/// A Flatten sent from here, until its outcome is known.
struct Asked {
    id: CommandId,
    label: String,
}

/// A Flatten's market order, watched until it closes or ends without closing.
struct Watching {
    order: OrderId,
    label: String,
    account: AccountId,
    contract: ContractKey,
}

pub struct PositionsView {
    feed: PaneFeed,
    common: PaneCommon,
    focus: FocusHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    cache: Option<Arc<TradeCache>>,
    seen_revision: u64,
    rows: Vec<Row>,
    /// The accounts listed, for the filter and the header's tag.
    accounts: Vec<model::Listed>,
    /// One account, or all.
    filter: Option<AccountId>,
    filter_open: bool,
    /// Flatten all asked; waiting for the answer.
    confirm: bool,
    asked: Vec<Asked>,
    watching: Vec<Watching>,
    /// A refusal, a "press again" or what couldn't close, until the next press.
    note: Option<String>,
    /// The pane's width as last drawn, for the columns that fit.
    width: f32,
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
    /// only, never the market's. And the second, so a P&L going stale is said.
    fn revision(&self) -> u64 {
        let mut r = crate::trading::accounts_epoch().wrapping_mul(1_000_003);
        if let Some(c) = &self.cache {
            r = r.wrapping_add(c.any_revision());
        }
        let second = (lines_feed::now_utc().0 / 1_000) as u64;
        r.wrapping_add(pnl::display_epoch() << 44)
            .wrapping_add(second.wrapping_mul(7_919))
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
            accounts: Vec::new(),
            filter: None,
            filter_open: false,
            confirm: false,
            asked: Vec::new(),
            watching: Vec::new(),
            note: None,
            width: 0.0,
        }
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Nothing here takes typed text.
    pub fn takes_text(&self) -> bool {
        false
    }

    /// The rows from the trading state, and what the Flattens sent from here came to; busy
    /// this instant: last time's.
    fn refresh(&mut self, cx: &gpui::App) -> bool {
        let Some(t) = Trading::try_global(cx) else {
            self.rows.clear();
            return true;
        };
        self.accounts = t
            .accounts()
            .into_iter()
            .map(|a| (a.id, a.badge, a.name))
            .collect();
        if self
            .filter
            .as_ref()
            .is_some_and(|f| !self.accounts.iter().any(|(a, _, _)| a == f))
        {
            self.filter = None;
        }
        let Some(st) = t.cache.try_read() else {
            return false;
        };
        self.rows = model::rows(&st, &self.accounts, self.filter.as_ref());
        let mut notes = Vec::new();
        let mut watch = Vec::new();
        self.asked.retain(|a| match st.result(a.id) {
            Some(Err(e)) => {
                notes.push(format!("{}: {}", a.label, e.message));
                false
            }
            Some(Ok(ids)) => {
                for id in ids {
                    if let Some(o) = st.order(*id).filter(|o| o.role == Role::Flatten) {
                        watch.push(Watching {
                            order: *id,
                            label: a.label.clone(),
                            account: o.account.clone(),
                            contract: o.contract,
                        });
                    }
                }
                false
            }
            None => true,
        });
        self.watching.extend(watch);
        // Accepted isn't closed: the broker can reject the market order, or it can end with
        // nothing to fill on. Said as the chain says it, with the broker's own words.
        self.watching.retain(|w| {
            let Some(o) = st.order(w.order) else {
                return true;
            };
            match &o.state {
                OrderState::Rejected { reason } => {
                    notes.push(format!("{}: couldn't close: {reason}", w.label));
                    false
                }
                OrderState::Cancelled => {
                    let open = st
                        .position(&w.account, w.contract)
                        .is_some_and(|p| p.qty != 0);
                    let replaced = st
                        .open_orders(&w.account, w.contract)
                        .any(|x| x.role == Role::Flatten && x.id != w.order);
                    if open && !replaced {
                        notes.push(format!(
                            "{}: Flatten canceled: the position is still open",
                            w.label
                        ));
                    }
                    false
                }
                s => !s.is_terminal(),
            }
        });
        if !notes.is_empty() {
            self.note = Some(notes.join("; "));
        }
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

    /// `ESZ6 · Paper`: what a note names.
    fn label(row: &Row) -> String {
        let product = lines_feed::catalog::get(row.contract.product);
        let name = product.map_or_else(
            || format!("{:?}", row.contract),
            |p| row.contract.label(&p).as_str().to_owned(),
        );
        format!("{name} · {}", row.name)
    }

    fn flatten(&mut self, rows: &[Row], cx: &mut Context<Self>) {
        let Some(t) = Trading::try_global(cx) else {
            self.note = Some("trading isn't running".to_owned());
            return;
        };
        for (r, cmd) in rows.iter().zip(model::flatten_all(rows)) {
            let id = t.submit(Source::Dom, Instant::now(), cmd);
            self.asked.push(Asked {
                id,
                label: Self::label(r),
            });
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

    /// Flatten all, or its question: outlined in the warning color, as the DOM's Flatten.
    fn flatten_all(&self, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let amber = |b: gpui::Stateful<gpui::Div>| b.border_color(t.warn);
        if !self.confirm {
            return crate::popover::always_live(
                amber(self.button("positions-flatten-all".into(), "Flatten all")).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if this.guarded(ev) && !this.rows.is_empty() {
                            this.confirm = true;
                        }
                        cx.notify();
                    }),
                ),
            )
            .into_any_element();
        }
        div()
            .flex()
            .flex_row()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(model::flatten_prompt(count)),
            )
            .child(crate::popover::always_live(
                amber(self.button("positions-flatten-yes".into(), "Flatten")).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if this.guarded(ev) {
                            this.confirm = false;
                            crate::popover::close_any(cx);
                            let rows = this.rows.clone();
                            this.flatten(&rows, cx);
                        }
                        cx.notify();
                    }),
                ),
            ))
            .child(
                self.button("positions-flatten-no".into(), "Keep")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.confirm = false;
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The account filter: `All accounts ▾`, or one account's name.
    fn filter_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let word = match &self.filter {
            Some(f) => self
                .accounts
                .iter()
                .find(|(a, _, _)| a == f)
                .map_or_else(|| f.0.clone(), |(_, _, n)| n.clone()),
            None => "All accounts".to_owned(),
        };
        let trigger = div()
            .id("positions-filter")
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(t.control_edge)
            .cursor_pointer()
            .child(format!("{word} ▾"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.filter_open = !this.filter_open;
                    cx.notify();
                }),
            );
        let list = self.filter_open.then(|| {
            let choice = |id: String, name: String, pick: Option<AccountId>, on: bool| {
                div()
                    .id(SharedString::from(id))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .when(on, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .hover(|s| s.bg(t.panel))
                    .child(name)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.filter = pick.clone();
                            this.filter_open = false;
                            this.confirm = false;
                            this.seen_revision = u64::MAX;
                            cx.notify();
                        }),
                    )
            };
            let mut list = div()
                .flex()
                .flex_col()
                .py_1()
                .bg(t.bg)
                .border_1()
                .border_color(t.control_edge)
                .rounded_sm()
                .text_size(px(12.0))
                .text_color(t.text_main)
                .child(choice(
                    "positions-filter-all".into(),
                    "All accounts".into(),
                    None,
                    self.filter.is_none(),
                ));
            for (i, (a, _, name)) in self.accounts.iter().enumerate() {
                let on = self.filter.as_ref() == Some(a);
                list = list.child(choice(
                    format!("positions-filter-{i}"),
                    name.clone(),
                    Some(a.clone()),
                    on,
                ));
            }
            list.into_any_element()
        });
        let dismiss: crate::popover::OnDismiss =
            Box::new(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.filter_open = false;
                cx.notify();
            }));
        crate::dropdown::dropdown(trigger, list)
            .on_dismiss(dismiss)
            .into_any_element()
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let count = self.rows.len();
        let badge = model::header_badge(&self.accounts, self.filter.as_ref());
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
                            .children(badge.map(|b| {
                                div()
                                    .flex_shrink_0()
                                    .px_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(t.control_edge)
                                    .text_size(px(10.0))
                                    .child(b)
                            }))
                            .child(self.filter_chip(cx))
                            .children(self.note.clone().map(|n| {
                                div().min_w_0().truncate().text_color(t.text_main).child(n)
                            })),
                    )
                    // Present while anything is listed, so it never appears under a pointer
                    // mid-run of presses that had nothing to act on.
                    .when(count > 0 || self.confirm, |d| {
                        d.child(self.flatten_all(count, cx))
                    }),
            )
    }

    fn cell(col: Col) -> gpui::Div {
        let d = div()
            .flex_shrink_0()
            .w(px(col.width()))
            .px_1()
            .overflow_hidden()
            .whitespace_nowrap();
        if col.right() {
            d.flex().justify_end()
        } else {
            d
        }
    }

    fn titles(&self, cols: &[Col]) -> impl IntoElement {
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
        for &c in cols {
            row = row.child(Self::cell(c).child(c.title()));
        }
        row
    }

    /// A P&L as text, `stale` after it when its price is old: never shown frozen as live.
    fn pnl_cell(&self, col: Col, pnl: lines_trading::Money, stale: bool, money: bool) -> gpui::Div {
        let t = self.theme;
        let mut s = String::new();
        if money {
            let _ = pnl::write_money(&mut s, pnl);
        }
        let color = if stale || !money {
            t.text_dim
        } else if pnl.0 < 0 {
            t.down
        } else {
            t.text_main
        };
        Self::cell(col)
            .gap_1()
            .text_color(color)
            .when(stale && money, |d| d.child("stale"))
            .child(s)
    }

    fn exits_cell(&self, r: &Row) -> gpui::Div {
        let t = self.theme;
        let product = lines_feed::catalog::get(r.contract.product);
        let underlying = r
            .contract
            .option_key()
            .and_then(|_| lines_feed::catalog::get(r.contract.product));
        let text = |e| {
            let mut s = String::new();
            let _ = model::write_exit(&mut s, e, product.as_ref(), underlying.as_ref());
            s
        };
        let part = |e: model::Exit, color| {
            if e == model::Exit::None {
                div().text_color(t.text_dim).child("none")
            } else {
                div().text_color(color).child(text(e))
            }
        };
        if r.stop == model::Exit::None && r.target == model::Exit::None {
            return Self::cell(Col::Exits).text_color(t.text_dim).child("none");
        }
        Self::cell(Col::Exits)
            .flex()
            .flex_row()
            .gap_1()
            .child(part(r.stop, t.down))
            .child(div().text_color(t.text_dim).child("/"))
            .child(part(r.target, t.order_target))
    }

    fn row_el(
        &self,
        n: usize,
        cols: &[Col],
        money: bool,
        now: Option<lines_core::UtcMillis>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.theme;
        let r = self.rows[n].clone();
        let product = lines_feed::catalog::get(r.contract.product);
        let price = |v: Option<f64>| {
            let mut s = String::new();
            if let (Some(v), Some(p)) = (v, &product) {
                let _ = crate::trade_bar::write_avg(&mut s, p, v);
            }
            s
        };
        let stale = now.is_some_and(|now| model::stale(&r, now));
        let mut el = div()
            .id(SharedString::from(format!("position-{n}")))
            .flex()
            .flex_row()
            .h(px(ROW_H))
            .items_center()
            .px_1()
            .border_b_1()
            .border_color(t.grid)
            .cursor_pointer()
            .hover(|s| s.bg(t.panel));
        for &c in cols {
            el = el.child(match c {
                Col::Symbol => {
                    let mut name = String::new();
                    if let Some(p) = &product {
                        name.push_str(r.contract.label(p).as_str());
                    }
                    Self::cell(c).child(name)
                }
                Col::Account => Self::cell(c).text_color(t.text_dim).child(r.name.clone()),
                Col::Side => Self::cell(c)
                    .text_color(if r.qty > 0 { t.up } else { t.down })
                    .child(model::side_word(r.qty)),
                Col::Qty => Self::cell(c).child(r.qty.abs().to_string()),
                Col::Avg => Self::cell(c).child(price(r.avg)),
                Col::Last => Self::cell(c).child(price(r.last)),
                Col::Exits => self.exits_cell(&r),
                Col::Pnl => self.pnl_cell(c, r.pnl, stale, money),
                Col::Flatten => {
                    let row = r.clone();
                    Self::cell(c)
                        .flex()
                        .justify_end()
                        .child(crate::popover::always_live(
                            self.button(
                                SharedString::from(format!("position-flatten-{n}")),
                                "Flatten",
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    if this.guarded(ev) {
                                        this.flatten(std::slice::from_ref(&row), cx);
                                    }
                                    cx.notify();
                                }),
                            ),
                        ))
                }
            });
        }
        let row = r.clone();
        el.on_mouse_down(
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

    /// `3 positions` and their P&L together, under the P&L column.
    fn total_row(
        &self,
        cols: &[Col],
        money: bool,
        now: Option<lines_core::UtcMillis>,
    ) -> impl IntoElement {
        let t = self.theme;
        let (count, total) = model::totals(&self.rows);
        let stale = now.is_some_and(|now| self.rows.iter().any(|r| model::stale(r, now)));
        let mut el = div()
            .flex()
            .flex_row()
            .h(px(ROW_H))
            .items_center()
            .px_1()
            .font_weight(FontWeight::SEMIBOLD);
        for (i, &c) in cols.iter().enumerate() {
            el = el.child(match c {
                _ if i == 0 => Self::cell(c).child(model::count_words(count)),
                Col::Pnl => self.pnl_cell(c, total, stale, money),
                _ => Self::cell(c),
            });
        }
        el.text_color(t.text_main)
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
        // The market clock (a replay's), else the wall's.
        let now = Trading::try_global(cx)
            .and_then(|tr| tr.now(&self.store))
            .or_else(|| Some(lines_feed::now_utc()));
        if self.rows.is_empty() {
            self.confirm = false;
        }
        // Not measured yet (the first frame): every column, until the width is known.
        let cols = if self.width > 0.0 {
            model::shown(self.width - 16.0)
        } else {
            Col::ALL.to_vec()
        };
        let body = if self.rows.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.text_dim)
                .child("Flat")
                .into_any_element()
        } else {
            let rows: Vec<_> = (0..self.rows.len())
                .map(|n| self.row_el(n, &cols, money, now, cx).into_any_element())
                .collect();
            div()
                .id("positions-table")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .track_focus(&self.focus)
                .child(self.titles(&cols))
                .children(rows)
                .child(self.total_row(&cols, money, now))
                .into_any_element()
        };
        // The pane's width, for the columns that fit.
        let measure = {
            let entity = cx.entity();
            canvas(
                move |bounds, window, cx| {
                    let w = f32::from(bounds.size.width);
                    entity.update(cx, |v, cx| {
                        if (v.width - w).abs() > 0.5 {
                            let before = v.width;
                            v.width = w;
                            if before <= 0.0
                                || model::shown(before - 16.0) != model::shown(w - 16.0)
                            {
                                // Other columns fit: drawn again with them.
                                v.seen_revision = u64::MAX;
                                cx.notify();
                                window.on_next_frame(|window, _| window.refresh());
                            }
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_size(px(12.0))
            .text_color(t.text_main)
            .child(measure)
            .child(self.header(cx))
            .child(body)
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
