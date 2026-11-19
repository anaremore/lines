//! The economic calendar pane (roadmap 8b): a pane like the journal, in a tab or beside a
//! chart, resizable, hideable, popped out, saved in the workspace (`CalendarState`). Its
//! header says where the calendar comes from and how fresh it is (a click opens Settings ›
//! Connections, where its refresh is set up), the focused account's blackout rule, and the
//! range (today, week, month). The table (`crate::econ_table` builds the rows) has resizable
//! columns (`pane_columns`, the journal's): time in the display zone, release, impact, the
//! account's blackout window, and where the line comes from. The next release is outlined,
//! past ones dimmed, one inside its window now says so in words.
//!
//! Rows are rebuilt at render when something they read moved (the calendar, its refresh, the
//! account and its rule, the range, the minute); paint only reads them.

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, Styled, Window, canvas,
    div, fill, point, prelude::*, px, size,
};
use lines_core::UtcMillis;
use lines_feed::store::ChartStore;
use lines_trading::AccountId;

use crate::econ_table::{self, Range, Row, RowState};
use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pane_columns::{
    self, CalCol, ColumnCmd, ColumnMenu, Columns, HeaderHit, Table, header_row_h,
};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::trading::{TradeCache, Trading};
use crate::wsdoc::{CalendarState, PaneDoc};

const ROW_H_100: f32 = 18.0;
const TEXT_PX: f32 = 12.0;
const PAD_100: f32 = 6.0;

#[inline]
fn row_h() -> f32 {
    crate::zoom::f(ROW_H_100)
}

#[inline]
fn pad() -> f32 {
    crate::zoom::f(PAD_100)
}

/// What the rows were built from: a change of any rebuilds them.
#[derive(Clone, PartialEq)]
struct Built {
    schedule: usize,
    epoch: u64,
    account: Option<AccountId>,
    /// The rule in force and the account's name, as read.
    rule: Option<lines_trading::news::NewsBlackout>,
    name: Option<String>,
    range: Range,
    minute: i64,
    zone: u64,
}

pub struct EconView {
    feed: PaneFeed,
    common: PaneCommon,
    focus: FocusHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    cache: Option<Arc<TradeCache>>,
    seen_revision: u64,
    range: Range,
    rows: Vec<Row>,
    /// The header's words: freshness (and whether it is a problem), the account's rule.
    fresh: (String, bool),
    rule: String,
    built: Option<Built>,
    scroll: usize,
    /// Scroll to the next release at the next rebuild (opening, a new range).
    reveal: bool,
    glyphs: Glyphs,
    cols: Columns,
    bounds: Bounds<Pixels>,
}

impl EventEmitter<FeedChanged> for EconView {}
impl EventEmitter<ColumnMenu> for EconView {}

impl Pane for EconView {
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
        let columns = self.cols.save(&CalCol::ALL);
        doc.calendar = (!self.range.is_default() || !columns.is_empty()).then_some(CalendarState {
            range: self.range,
            columns,
        });
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    /// The calendar's and its refresh's epoch, the trading cache's (an account's rule may have
    /// changed: the rows rebuild only if it did), the display zone's, and the minute on the
    /// pane's clock (a release passes, a window opens): atomics only, never the feed's.
    fn revision(&self) -> u64 {
        let clock = self
            .store
            .market_clock()
            .unwrap_or_else(lines_feed::now_utc);
        let minute = (clock.0 / 60_000) as u64;
        let trading = self.cache.as_ref().map_or(0, |c| c.any_revision());
        crate::econ_ui::epoch()
            .wrapping_mul(1_000_003)
            .wrapping_add(trading.wrapping_mul(7_919))
            .wrapping_add(lines_core::display_tz::epoch() << 40)
            .wrapping_add(minute)
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.store = feed.store().clone();
        self.feed = feed;
        self.built = None;
        cx.notify();
    }
}

impl EconView {
    pub fn new(feed: PaneFeed, theme: Theme, cx: &mut Context<Self>) -> Self {
        let store = feed.store().clone();
        pane::watch_store(cx);
        EconView {
            feed,
            common: PaneCommon::default(),
            focus: cx.focus_handle(),
            store,
            theme,
            cache: Trading::try_global(cx).map(|t| t.cache.clone()),
            seen_revision: u64::MAX,
            range: Range::default(),
            rows: Vec::new(),
            fresh: (String::new(), false),
            rule: String::new(),
            built: None,
            scroll: 0,
            reveal: true,
            glyphs: Glyphs::new(TEXT_PX),
            cols: Columns::new(Table::Calendar),
            bounds: Bounds::default(),
        }
    }

    pub fn restore(&mut self, s: &CalendarState) {
        self.range = s.range;
        self.cols.load(&CalCol::ALL, &s.columns);
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub fn column_command(
        &mut self,
        cmd: ColumnCmd,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        self.cols.apply(cmd)?;
        cx.notify();
        Ok(())
    }

    pub fn columns(&self) -> &Columns {
        &self.cols
    }

    /// It never takes typed text.
    pub fn takes_text(&self) -> bool {
        false
    }

    pub fn set_range(&mut self, r: Range, cx: &mut Context<Self>) {
        if r != self.range {
            self.range = r;
            self.scroll = 0;
            self.reveal = true;
            self.built = None;
            cx.notify();
        }
    }

    /// The time the rows read: the trading clock on this pane's data (a replay's time in a
    /// replay), else the wall's.
    fn now(&self, cx: &App) -> UtcMillis {
        Trading::try_global(cx)
            .and_then(|t| t.now(&self.store))
            .unwrap_or_else(lines_feed::now_utc)
    }

    /// Rebuild the rows and header words if anything they read moved. The trading cache busy
    /// this instant: nothing is rebuilt (never "no blackout" for want of a read), and the next
    /// poll tries again.
    fn rebuild(&mut self, cx: &App) {
        let now = self.now(cx);
        let schedule = crate::econ_ui::schedule();
        let trading = Trading::try_global(cx);
        let account = crate::econ_ui::focused_account()
            .filter(|a| trading.is_some_and(|t| t.account(a).is_some()))
            .or_else(|| trading.and_then(Trading::default_account));
        let rule = match (account.as_ref(), trading) {
            (Some(a), Some(t)) => match t.cache.try_read() {
                Some(state) => state
                    .account(a)
                    .and_then(|v| crate::econ_ui::rule_of(&v.limits)),
                None => {
                    self.seen_revision = u64::MAX;
                    return;
                }
            },
            _ => None,
        };
        let name = account
            .as_ref()
            .and_then(|a| trading.and_then(|t| t.account(a)).map(|i| i.name));
        let key = Built {
            schedule: Arc::as_ptr(&schedule) as usize,
            epoch: crate::econ_ui::epoch(),
            account: account.clone(),
            rule,
            name: name.clone(),
            range: self.range,
            minute: now.0.div_euclid(60_000),
            zone: lines_core::display_tz::epoch(),
        };
        if self.built.as_ref() == Some(&key) {
            return;
        }
        let z = lines_core::display_tz::current();
        self.rows = econ_table::rows(
            &schedule,
            &crate::econ_ui::base(),
            rule.as_ref(),
            now,
            self.range,
            &z,
        );
        self.fresh = econ_table::freshness(
            &crate::econ_ui::refresh(),
            &schedule,
            !Arc::ptr_eq(&schedule, &crate::econ_ui::base()),
            now,
            &z,
        );
        self.rule = econ_table::rule_text(rule.as_ref(), name.as_deref());
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
        self.built = Some(key);
    }

    fn table_xy(&self, p: Point<Pixels>) -> (f32, f32) {
        (f(p.x - self.bounds.origin.x), f(p.y - self.bounds.origin.y))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
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

    fn on_right_down(&mut self, ev: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = self.table_xy(ev.position);
        if self.cols.hit(x, y, 1) != HeaderHit::Outside {
            cx.emit(ColumnMenu(ev.position));
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, ev: &gpui::ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dy = f(ev.delta.pixel_delta(window.line_height()).y);
        let rows = (dy / row_h()).round() as i64;
        if rows != 0 {
            let max = self.rows.len().saturating_sub(1) as i64;
            self.scroll = (self.scroll as i64 - rows).clamp(0, max) as usize;
            cx.notify();
        }
    }

    fn visible_rows(&self) -> usize {
        (((f(self.bounds.size.height) - header_row_h()) / row_h()).floor() as usize).max(1)
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        // Not before the pane has its height (a first frame can be laid out at none).
        if self.reveal
            && !self.rows.is_empty()
            && f(bounds.size.height) > header_row_h() + 3.0 * row_h()
        {
            // The next release (or the one in its window) in view, a row above it when it
            // has to scroll: past releases stay above it.
            let n = self.visible_rows();
            if let Some(i) = self
                .rows
                .iter()
                .position(|r| matches!(r.state, RowState::Next | RowState::Inside))
                && i >= self.scroll + n
            {
                self.scroll = i.saturating_sub(1);
            }
            self.reveal = false;
        }
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        window.paint_quad(fill(bounds, t.bg));
        if let Some(first) = self.rows.first() {
            self.cols.name_zone(first.at);
        }
        self.cols
            .measure_headers(&mut self.glyphs, &t, pad(), window);
        let end = (self.scroll + self.visible_rows()).min(self.rows.len());
        for i in self.scroll..end {
            for (c, col) in CalCol::ALL.iter().enumerate() {
                if *col == CalCol::Release {
                    continue; // fills; never widens the table
                }
                let tw = self
                    .glyphs
                    .width(&self.rows[i].cells[c], t.text_main, false, window);
                self.cols.need(c, tw, pad());
            }
        }
        self.cols.layout_calendar(w);
        let left = [true; 5];
        self.cols
            .paint_header(&mut self.glyphs, &t, bounds, pad(), &left, window, cx);
        let oy = top + header_row_h();
        if self.rows.is_empty() {
            self.glyphs.paint(
                "No releases in this range",
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
        for (n, i) in (self.scroll..end).enumerate() {
            let y = oy + n as f32 * row_h();
            if y + row_h() > top + h + 0.5 {
                break;
            }
            let r = &self.rows[i];
            let past = r.state == RowState::Past;
            for (c, col) in CalCol::ALL.iter().enumerate() {
                let Some((x, cw)) = self.cols.cell(c, ox) else {
                    continue;
                };
                let color = if past {
                    t.text_dim
                } else {
                    match col {
                        CalCol::Source => t.text_dim,
                        _ => t.text_main,
                    }
                };
                let bold = !past
                    && match col {
                        CalCol::Impact => r.high,
                        CalCol::Window => r.state == RowState::Inside,
                        _ => false,
                    };
                let cell = Bounds::new(point(px(x), px(y)), size(px(cw), px(row_h())));
                let text = r.cells[c].as_str();
                window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                    self.glyphs
                        .paint(text, x + pad(), y, row_h(), color, bold, window, cx);
                });
            }
            // The next release: an accent outline and edge bar (no fill: text contrast is
            // untouched). Inside its window now: the same in the warning color, and the window
            // cell says "now" in words.
            let mark = match r.state {
                RowState::Next => Some(t.accent),
                RowState::Inside => Some(t.warn),
                _ => None,
            };
            if let Some(c) = mark {
                for yy in [y, y + row_h() - hair * 2.0] {
                    window.paint_quad(fill(
                        Bounds::new(point(px(ox), px(yy)), size(px(w), px(hair * 2.0))),
                        c,
                    ));
                }
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(y)), size(px(3.0), px(row_h()))),
                    c,
                ));
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + row_h() - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        let body = Bounds::new(point(px(ox), px(oy)), size(px(w), px(h - header_row_h())));
        self.cols.paint_rules(&t, body, window);
    }

    fn range_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .gap_2()
            .children(Range::ALL.into_iter().map(|r| {
                let on = r == self.range;
                div()
                    .id(r.label())
                    .px_1()
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(if on {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if on { t.text_main } else { t.text_dim })
                    .when(on, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                    .child(r.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.set_range(r, cx);
                        }),
                    )
            }))
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let (fresh, bad) = self.fresh.clone();
        div()
            .flex()
            .flex_row()
            .justify_between()
            .gap_2()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(pane::controls_gutter(crate::layout::PaneKind::Calendar)))
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
                            .min_w_0()
                            .overflow_hidden()
                            .children(pane::link_chip(self.common.link, &t))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(t.text_main)
                                    .child("Economic calendar"),
                            )
                            .child(self.range_tabs(cx))
                            .child(
                                div()
                                    .id("econ-fresh")
                                    .min_w_0()
                                    .truncate()
                                    .cursor_pointer()
                                    .text_color(if bad { t.text_main } else { t.text_dim })
                                    .when(bad, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                                    .child(fresh)
                                    // Where the refresh is set up (the FRED key).
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _: &MouseDownEvent, _, cx| {
                                            cx.stop_propagation();
                                            cx.defer(|cx| {
                                                crate::settings_view::open_section(
                                                    crate::settings_schema::Section::Connections,
                                                    cx,
                                                )
                                            });
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .min_w(crate::zoom::ui(96.0))
                            .flex_shrink(1.0)
                            .truncate()
                            .text_color(t.text_main)
                            .child(self.rule.clone()),
                    ),
            )
    }
}

impl Render for EconView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebuild(cx);
        let entity = cx.entity();
        let t = self.theme;
        let e = entity.clone();
        let body = canvas(
            move |bounds, window, cx| e.read(cx).cols.divider_hitboxes(bounds, window),
            move |bounds, hitboxes, window, cx| {
                pane_columns::set_divider_cursors(&hitboxes, window);
                entity.update(cx, |view, cx| view.paint(bounds, window, cx));
            },
        )
        .size_full();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll(cx.listener(Self::on_scroll))
                    .child(body),
            )
    }
}
