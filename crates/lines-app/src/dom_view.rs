//! The DOM pane (roadmap 5; order entry roadmap 7): a price ladder of fixed rows indexed by
//! integer ticks.
//!
//! Columns, left to right: the trader's own orders, bid size, price, ask size, traded volume.
//!
//! Trading (`crate::dom_trade` decides, `crate::dom_orders` draws, `crate::trade_bar` is the
//! header): in an **armed** DOM a click in the bid column buys and in the ask column sells, at
//! the row's displayed price (limit; Shift: stop market). Working orders sit in the orders
//! column at their price: drag one to another row to move it (a ghost shows until the broker
//! confirms), right-click it to cancel. Moving and cancelling don't need arming (they never
//! add exposure). Every action is a command sent to the trading core with the click's time;
//! the DOM never waits for an answer. Sizes are numbers with a
//! subtle depth bar behind them; numbers use tabular digits, right-aligned. A slim header row
//! names the columns; its dividers resize them (drag, double-click to fit) and right-clicking
//! it shows or hides them (`crate::pane_columns`). A column is never narrower than its widest
//! current number.
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
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render,
    ScrollEvent, Styled, Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::ladder::{
    self, GROUPING_PRESETS, Inside, LadderRow, LadderStats, LadderView, build_ladder, inside_row,
};
use lines_core::{FixedStr, Product};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;

use crate::dom_orders::{DragOrder, OrdersLayer};
use crate::dom_trade::{self, DomColumn, EntryCtx, Ghost, Refusal};
use crate::dom_trail::DomTrail;
use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pane_columns::{
    self, ColumnCmd, ColumnMenu, Columns, DomCol, HEADER_ROW_H, HeaderHit, Table,
};
use crate::picker_view::{PickerKey, PickerUi};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::trade_bar::{self, PaneTrade, TradeDoc, TradePane};
use crate::trading::Trading;
use crate::wsdoc::{DomState, PaneDoc};

/// Most rows a ladder shows (the row buffer is allocated once at this size).
const MAX_ROWS: usize = 256;
const ROW_H: f32 = 18.0;
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 6.0;

#[derive(Clone, Debug)]
pub struct DomOptions {
    /// Ticks per row.
    pub grouping: u32,
    /// Start with the price trail on (`--dom-trail`; a DOM's saved `trail` wins).
    pub trail: bool,
    /// `--dom-grouping` was given: it wins over Settings › DOM for this run.
    pub grouping_from_cli: bool,
    /// `--dom-trail` was given.
    pub trail_from_cli: bool,
}

impl Default for DomOptions {
    fn default() -> Self {
        DomOptions {
            grouping: 1,
            trail: false,
            grouping_from_cli: false,
            trail_from_cli: false,
        }
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
    /// Column widths and visibility, and this frame's layout.
    cols: Columns,
    /// Account, quantity, bracket, arming (roadmap 7).
    trade: PaneTrade,
    /// This frame's own orders, position and rejections; drags and their ghosts.
    orders: OrdersLayer,
    /// Whether the trading header was last rendered with its narrow labels.
    header_narrow: Option<bool>,
    /// Where price just traded (`crate::dom_trail`), off by default.
    trail: DomTrail,
}

impl EventEmitter<FeedChanged> for DomView {}
impl EventEmitter<ColumnMenu> for DomView {}

impl TradePane for DomView {
    fn trade(&mut self) -> &mut PaneTrade {
        &mut self.trade
    }
    fn trade_ref(&self) -> &PaneTrade {
        &self.trade
    }
    fn trade_store(&self) -> &Arc<ChartStore> {
        &self.store
    }
    fn trade_unavailable(&self) -> bool {
        self.feed.unavailable.is_some()
    }
}

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
            columns: self.cols.save(&DomCol::ALL),
            trail: self.trail.doc(),
        });
        doc.trade = self.trade.doc();
    }

    fn changed(&mut self) -> bool {
        let shown = self.cols.set.is_shown(DomCol::Trail.index());
        self.trade.changed() | self.trail.wants_frame(&self.store, shown)
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
        let follow = feed.sub.is_some();
        self.orders = OrdersLayer::default();
        self.feed = feed;
        self.trade.feed_changed(&self.store, follow, cx);
        self.view = LadderView::default();
        self.rows.clear();
        self.stats = LadderStats::default();
        self.inside = Inside::Empty;
        self.seen_revision = u64::MAX;
        self.trail.reset();
        cx.notify();
    }
}

impl DomView {
    /// A new look (the Settings window's appearance).
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }
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
        if feed.sub.is_some()
            && let Some(t) = Trading::try_global(cx)
        {
            t.follow(&store);
        }
        let trade = PaneTrade::new(None, cx);
        let mut trail = DomTrail::new(i64::from(opts.grouping.max(1)) * product.price.tick);
        trail.set_enabled(opts.trail);
        let mut cols = Columns::new(Table::Dom);
        sync_trail_column(&mut cols, &trail, false);
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
            cols,
            trade,
            orders: OrdersLayer::default(),
            header_narrow: None,
            trail,
        }
    }

    /// Restore the pane's trading choices (account, quantity, bracket, the trading switch:
    /// sticky, as saved). The screenshot fixture's `--enable-trading` enables it too.
    pub fn restore_trade(&mut self, doc: Option<&TradeDoc>, cx: &mut Context<Self>) {
        self.trade = PaneTrade::new(doc, cx);
        if let Some(t) = Trading::try_global(cx) {
            let (on, editor) = (t.start_armed, t.start_editor);
            if on && !self.trade.arming.is_armed() {
                self.trade.arm_event(crate::dom_trade::ArmEvent::Toggle, cx);
            }
            if editor {
                trade_bar::run(
                    self,
                    crate::commands::TradeCmd::EditBrackets,
                    std::time::Instant::now(),
                    cx,
                );
            }
        }
    }

    /// Restore saved view state (price grouping, columns).
    pub fn restore(&mut self, state: &DomState) {
        self.set_grouping(state.grouping.clamp(1, 10_000));
        self.cols.load(&DomCol::ALL, &state.columns);
        if state.trail.is_some() {
            self.trail.restore(state.trail);
        }
        let hidden = state.columns.get(&DomCol::Trail).and_then(|c| c.shown) == Some(false);
        sync_trail_column(&mut self.cols, &self.trail, hidden);
    }

    /// Turn the price trail on or off (its column comes and goes with it).
    pub fn toggle_trail(&mut self, cx: &mut Context<Self>) -> bool {
        let on = !self.trail.enabled();
        self.trail.set_enabled(on);
        sync_trail_column(&mut self.cols, &self.trail, false);
        cx.notify();
        on
    }

    /// The next trail half-life preset; returns it (ms).
    pub fn cycle_trail_half_life(&mut self, cx: &mut Context<Self>) -> u32 {
        let ms = self.trail.cycle_half_life();
        cx.notify();
        ms
    }

    pub fn trail(&self) -> &DomTrail {
        &self.trail
    }

    /// A column command on this DOM (decision 0018).
    pub fn column_command(
        &mut self,
        cmd: ColumnCmd,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        // The trail column is the trail: showing it turns the trail on, hiding it off.
        if cmd == ColumnCmd::Toggle(pane_columns::Column::Dom(DomCol::Trail)) {
            self.toggle_trail(cx);
            return Ok(());
        }
        self.cols.apply(cmd)?;
        cx.notify();
        Ok(())
    }

    /// The columns (for the column menu).
    pub fn columns(&self) -> &Columns {
        &self.cols
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
        (((f(self.bounds.size.height) - HEADER_ROW_H) / ROW_H).floor() as usize).clamp(1, MAX_ROWS)
    }

    /// A window position in table coordinates (from the header row's top-left).
    fn table_xy(&self, p: Point<Pixels>) -> (f32, f32) {
        (f(p.x - self.bounds.origin.x), f(p.y - self.bounds.origin.y))
    }

    // ---- input ---------------------------------------------------------------------

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A click that closes a list (the account list, the picker) does only that: never
        // also an order on the ladder under it. The account list (a popover) takes no focus
        // either; it normally takes the press on its backdrop (`crate::popover`), this is the
        // fallback.
        if self.trade.account_menu {
            trade_bar::close_account_menu(self, cx);
            cx.stop_propagation();
            return;
        }
        self.focus.focus(window, cx);
        if self.picker.close() {
            cx.stop_propagation();
            cx.notify();
            return;
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
            HeaderHit::Row => {}
            HeaderHit::Outside => self.body_down(ev, cx),
        }
    }

    /// The ladder row under a window point: its displayed price.
    fn row_at(&self, p: Point<Pixels>) -> Option<lines_core::Price> {
        let (_, y) = self.table_xy(p);
        let y = y - HEADER_ROW_H;
        if y < 0.0 {
            return None;
        }
        self.rows.get((y / ROW_H) as usize).map(|r| r.price)
    }

    /// The column under a window point.
    fn column_at(&self, p: Point<Pixels>) -> Option<DomColumn> {
        let (x, ox) = (f(p.x), f(self.bounds.origin.x));
        [
            (DomCol::Orders, DomColumn::Orders),
            (DomCol::Bid, DomColumn::Bid),
            (DomCol::Price, DomColumn::Price),
            (DomCol::Ask, DomColumn::Ask),
            (DomCol::Volume, DomColumn::Volume),
        ]
        .into_iter()
        .find(|(c, _)| {
            self.cols
                .cell(c.index(), ox)
                .is_some_and(|(cx, w)| x >= cx && x < cx + w)
        })
        .map(|(_, d)| d)
    }

    /// What a click needs (`trade_bar::entry_ctx`, shared with the header's buttons).
    fn entry_ctx(&self, cx: &App) -> Result<EntryCtx, &'static str> {
        trade_bar::entry_ctx(self, cx)
    }

    /// A left press in the ladder: place an order (bid/ask columns, armed), or grab one of
    /// the trader's orders to move it (orders column).
    fn body_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let issued = std::time::Instant::now();
        let (Some(price), Some(col)) = (self.row_at(ev.position), self.column_at(ev.position))
        else {
            return;
        };
        if col == DomColumn::Orders {
            if let Some(m) = self.orders.order_at(price, self.row_units()) {
                self.orders.drag = Some(DragOrder {
                    order: m.id,
                    to: price,
                });
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        if !matches!(col, DomColumn::Bid | DomColumn::Ask) {
            return;
        }
        match self.entry_ctx(cx) {
            Err(why) => self.trade.flash(why),
            Ok(ctx) => {
                match dom_trade::click(col, price, ev.modifiers.shift, self.trade.arming, &ctx) {
                    Ok(cmd) => {
                        if let Some(t) = Trading::try_global(cx) {
                            t.submit(lines_trading::Source::Dom, issued, cmd);
                        }
                    }
                    Err(Refusal(why)) => self.trade.flash(why),
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Release after dragging an order: move it to the row under the pointer (unless that is
    /// its own row), with a ghost until the broker confirms.
    fn drop_order(&mut self, d: DragOrder, cx: &mut Context<Self>) {
        let issued = std::time::Instant::now();
        let units = self.row_units();
        let Some(t) = Trading::try_global(cx) else {
            return;
        };
        let order = t.cache.read().order(d.order).cloned();
        let Some(o) = order else {
            return;
        };
        let Some(from) = dom_trade::order_price(&o) else {
            return;
        };
        if dom_trade::row_of(from.0, units) == dom_trade::row_of(d.to.0, units) {
            return;
        }
        if let Some(cmd) = dom_trade::drag_command(&o, d.to) {
            let id = t.submit(lines_trading::Source::Dom, issued, cmd);
            self.orders.ghosts.push(Ghost {
                order: o.id,
                price: d.to,
                command: id,
            });
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.orders.drag {
            if ev.pressed_button != Some(MouseButton::Left) {
                self.orders.drag = None;
            } else if let Some(to) = self.row_at(ev.position)
                && to != d.to
            {
                self.orders.drag = Some(DragOrder { to, ..d });
            } else {
                return;
            }
            cx.notify();
            return;
        }
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
        if let Some(d) = self.orders.drag.take() {
            self.drop_order(d, cx);
            cx.notify();
        }
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
            return;
        }
        // Right-click one of the trader's orders: cancel it (never needs arming).
        if self.column_at(ev.position) == Some(DomColumn::Orders)
            && let Some(price) = self.row_at(ev.position)
            && let Some(m) = self.orders.order_at(price, self.row_units())
            && let (Some(account), Some(t)) = (self.trade.account.clone(), Trading::try_global(cx))
        {
            t.submit(
                lines_trading::Source::Dom,
                std::time::Instant::now(),
                lines_trading::Command::CancelOrder {
                    account,
                    order: m.id,
                },
            );
            cx.stop_propagation();
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
        if !self.picker.is_open() && pane::unavailable_key(self, ev, cx) {
            return;
        }
        let k = &ev.keystroke;
        if self.trade.account_menu && crate::popover::closes_on_key(&k.key) {
            trade_bar::close_account_menu(self, cx);
            cx.stop_propagation();
            return;
        }
        if trade_bar::editor_key(self, ev, cx) {
            cx.stop_propagation();
            return;
        }
        if trade_bar::qty_key(self, k.key.as_str(), cx) {
            cx.stop_propagation();
            return;
        }
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

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        // The header's labels depend on the pane's width, known only after layout: if this
        // frame's width calls for the other label set, render the header again.
        let narrow = trade_bar::is_narrow(f(bounds.size.width));
        if self.header_narrow != Some(narrow) {
            self.header_narrow = Some(narrow);
            cx.notify();
        }
        self.common.paints += 1;
        self.bounds = bounds;
        let rows = self.visible_rows();
        // Hit-tested here, after layout, from the pointer's actual position: nothing the
        // trader aims at moves while the pointer is over the ladder.
        let hovered =
            !self.frozen && window.is_window_hovered() && bounds.contains(&window.mouse_position());
        let units = self.row_units();
        if let Some(d) = self.store.try_read() {
            self.trail.prepare(&d, units, self.store.feed_clock());
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
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let w = f(bounds.size.width);
        let h = f(bounds.size.height);
        let rect =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        window.paint_quad(fill(bounds, t.bg));
        let fmt = self.product.price;
        let qf = self.product.qty;
        let mut s = FixedStr::<48>::new();

        // What each column must hold this frame: its title and its widest number (bold is
        // the wider face). Columns are laid out from these: never narrower.
        self.cols.measure_headers(&mut self.glyphs, &t, PAD, window);
        for i in 0..self.rows.len() {
            let r = self.rows[i];
            s.clear();
            let _ = fmt.write(r.price, &mut s);
            let pw = self.glyphs.width(s.as_str(), t.text_main, true, window);
            self.cols.need(DomCol::Price.index(), pw, PAD);
            for (q, c) in [
                (r.bid, DomCol::Bid),
                (r.ask, DomCol::Ask),
                (r.volume, DomCol::Volume),
            ] {
                if q.0 > 0 {
                    s.clear();
                    let _ = qf.write(q, &mut s);
                    let qw = self.glyphs.width(s.as_str(), t.text_main, true, window);
                    self.cols.need(c.index(), qw, PAD);
                }
            }
        }
        // The orders column holds two orders per row, each `S10+` or `10/25` at most.
        let ow = self.glyphs.width("S88+", t.text_main, true, window);
        self.cols
            .need(DomCol::Orders.index(), 2.0 * (ow + 6.0), PAD);
        // Narrow: volume, then the trail give way, then the rest narrow to their numbers;
        // only then working orders (the panel still counts them; Cancel all reaches them):
        // bid, price and ask stay whole.
        self.cols.layout_fitting(
            w,
            &[DomCol::Volume.index(), DomCol::Trail.index()],
            &[DomCol::Orders.index()],
        );
        self.cols
            .paint_header(&mut self.glyphs, &t, bounds, PAD, &[false; 5], window, cx);
        let cell = |c: DomCol| self.cols.cell(c.index(), ox);
        let (orders, bid, price, ask, vol) = (
            cell(DomCol::Orders),
            cell(DomCol::Bid),
            cell(DomCol::Price),
            cell(DomCol::Ask),
            cell(DomCol::Volume),
        );
        let oy = top + HEADER_ROW_H;
        let body_h = h - HEADER_ROW_H;
        if let Some((x, cw)) = price {
            crate::layout_rec::reserved(window, (x, oy, cw, body_h), "dom-price");
        }
        if let Some((x, cw)) = orders {
            window.paint_quad(fill(rect(x, oy, cw, body_h), t.panel));
        }
        // The ladder's own columns (everything but the orders slot).
        let table_l = [bid, price, ask, vol]
            .into_iter()
            .flatten()
            .map(|c| c.0)
            .fold(f32::INFINITY, f32::min)
            .min(ox + w);
        let body = Bounds::new(point(px(ox), px(oy)), size(px(w), px(body_h)));

        if self.rows.is_empty() {
            let msg = if self.has_depth {
                "no depth yet"
            } else {
                "no depth from this source"
            };
            // The ladder's own empty state, across its (empty) price column.
            crate::layout_rec::set_owner("dom-price");
            self.glyphs.paint(
                msg,
                table_l + PAD,
                oy + PAD,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            crate::layout_rec::set_owner("");
            return;
        }

        let (max_size, max_vol) = (self.stats.max_size.0.max(1), self.stats.max_volume.0.max(1));
        let flagged = matches!(self.inside, Inside::Locked | Inside::Crossed);
        // A locked or crossed book outlines the inside rows across bid, price and ask.
        let span = match (bid.or(price).or(ask), ask.or(price).or(bid)) {
            (Some(l), Some(r)) => Some((l.0, r.0 + r.1)),
            _ => None,
        };
        for i in 0..self.rows.len() {
            let r = self.rows[i];
            let y = oy + i as f32 * ROW_H;
            if y + ROW_H > top + h + 0.5 {
                break;
            }
            // Bid: inside tint, then a bar growing from the price side outwards.
            if let Some((x, cw)) = bid {
                if r.best_bid {
                    window.paint_quad(fill(rect(x, y, cw, ROW_H), t.inside_bid));
                }
                if r.bid.0 > 0 {
                    let bw = (cw * (r.bid.0 as f32 / max_size as f32)).max(hair);
                    window.paint_quad(fill(rect(x + cw - bw, y + 1.0, bw, ROW_H - 2.0), t.bid_bar));
                    s.clear();
                    let _ = qf.write(r.bid, &mut s);
                    self.glyphs.paint_right(
                        s.as_str(),
                        x + cw - PAD,
                        y,
                        ROW_H,
                        t.text_main,
                        r.best_bid,
                        window,
                        cx,
                    );
                }
            }
            if let Some((x, cw)) = ask {
                if r.best_ask {
                    window.paint_quad(fill(rect(x, y, cw, ROW_H), t.inside_ask));
                }
                if r.ask.0 > 0 {
                    let aw = (cw * (r.ask.0 as f32 / max_size as f32)).max(hair);
                    window.paint_quad(fill(rect(x, y + 1.0, aw, ROW_H - 2.0), t.ask_bar));
                    s.clear();
                    let _ = qf.write(r.ask, &mut s);
                    self.glyphs.paint_right(
                        s.as_str(),
                        x + cw - PAD,
                        y,
                        ROW_H,
                        t.text_main,
                        r.best_ask,
                        window,
                        cx,
                    );
                }
            }
            // Price: the last trade's row is tagged like the chart's last-price tag. Only the last trade's row is emphasized (one last-price
            // treatment, as on the chart); the inside market is marked in the bid and ask
            // columns.
            if let Some((x, cw)) = price {
                let (pfg, bold) = if r.last {
                    window.paint_quad(fill(rect(x, y, cw, ROW_H), t.last));
                    (t.on_last, true)
                } else {
                    (t.text_main, false)
                };
                s.clear();
                let _ = fmt.write(r.price, &mut s);
                crate::layout_rec::set_owner("dom-price");
                self.glyphs
                    .paint_right(s.as_str(), x + cw - PAD, y, ROW_H, pfg, bold, window, cx);
                crate::layout_rec::set_owner("");
            }
            if flagged
                && (r.best_bid || r.best_ask)
                && let Some((l, rr)) = span
            {
                window.paint_quad(fill(rect(l, y, rr - l, hair), t.text_main));
                window.paint_quad(fill(rect(l, y + ROW_H - hair, rr - l, hair), t.text_main));
            }
            // Traded volume.
            if let Some((x, cw)) = vol
                && r.volume.0 > 0
            {
                let vw = ((cw - PAD).max(1.0) * (r.volume.0 as f32 / max_vol as f32)).max(hair);
                window.paint_quad(fill(rect(x, y + 2.0, vw, ROW_H - 4.0), t.volume_bar));
                s.clear();
                let _ = qf.write(r.volume, &mut s);
                self.glyphs.paint_right(
                    s.as_str(),
                    x + cw - PAD,
                    y,
                    ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            window.paint_quad(fill(
                rect(table_l, y + ROW_H - hair, ox + w - table_l, hair),
                t.grid,
            ));
        }
        self.trail.paint(
            &self.rows,
            cell(DomCol::Trail),
            oy,
            ROW_H,
            top + h,
            &t,
            window,
        );
        // The trader's orders, the position marker and rejections, over the rows.
        if let Some(tr) = Trading::try_global(cx) {
            let contract = trade_bar::contract_of(&self.store);
            for (p, why) in self
                .orders
                .prepare(&tr.cache, self.trade.account.as_ref(), contract)
            {
                self.trade
                    .flash(format!("{}: {why}", fmt.format::<32>(p).as_str()));
            }
            let units = self.row_units();
            self.orders.paint(
                &mut self.glyphs,
                &t,
                &self.rows,
                units,
                self.product.price.tick,
                orders,
                price,
                oy,
                ROW_H,
                hair,
                window,
                cx,
            );
        }
        self.cols.paint_rules(&t, body, window);
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
        if let Some(n) = pane::adopted_note(&self.feed) {
            let _ = write!(right, "{n}  ·  ");
        }
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
            .pr(px(pane::controls_gutter(crate::layout::PaneKind::Dom)))
            .py_1()
            .bg(t.panel)
            .border_b_1()
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
                            .children(pane::link_chip(self.common.link, &t))
                            .child(
                                div()
                                    .text_color(t.text_main)
                                    .relative()
                                    .children(crate::layout_rec::text_marker(&left))
                                    .child(left),
                            )
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
                            // Cut with "…" when the pane is narrow, never mid-word.
                            .child(div().min_w_0().truncate().child(right)),
                    ),
            )
    }
}

/// The trail column is shown exactly while the trail is on (unless a saved file hid it).
fn sync_trail_column(cols: &mut Columns, trail: &DomTrail, hidden: bool) {
    let i = DomCol::Trail.index();
    let specs = cols.specs();
    let _ = cols.set.set_shown(specs, i, trail.enabled() && !hidden);
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

impl DomView {
    /// Taking typed text now (the symbol picker): the workspace leaves plain and Alt keys
    /// to it (`commands::runs_while_typing`).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open()
    }
}

impl Render for DomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let t = self.theme;
        let focus = self.focus.clone();
        PaneTrade::watch_focus(self, &focus, window, cx);
        // The switch is sticky: impossible trading (no contract, feed gone) shows on it as a
        // badge and denies orders; it never flips the switch.
        let width = f(self.bounds.size.width);
        self.header_narrow = Some(trade_bar::is_narrow(width));
        let trade = trade_bar::dom_bar(self, &self.product, width, &t, cx);
        let today = pane::today(self.frozen, &self.store);
        let overlay =
            self.picker
                .overlay(&t, &self.feed.availability, today, Self::picker_click, cx);
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
                        entity.update(cx, |view, cx| view.paint(bounds, window, cx));
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
            .child(self.header())
            .children(trade)
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
