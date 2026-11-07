//! The journal pane (roadmap 8c, phase 1): every closed trade, newest first, with a stats
//! strip for the current trade date (or all dates) on top.
//!
//! - **Table**: time, contract, side, quantity, average entry and exit, net P&L, ticks,
//!   duration, MAE/MFE, tags; resizable, hideable columns (`lines_core::columns`, saved with
//!   the pane). P&L is signed text (`+310.02`), coloured up/down: never colour alone.
//! - **Stats strip**: count, win rate, net, average win and loss, expectancy, profit factor,
//!   max drawdown, streak; click it (or the palette) to switch between the current trade date
//!   and all dates.
//! - **Editing**: click a row (or Up/Down) to select it, Enter to edit its tags and note
//!   inline (Tab switches field, Enter saves, Esc cancels). Edits are saved as journal records.
//!
//! The pane is fed by the trading thread through `lines_trading::JournalShared`: it polls the
//! journal's lock-free revision (like a store's) and re-formats its rows only when it moved,
//! so a frame paints cached strings. It has a feed like every pane (link groups, plumbing) but
//! shows every account's trades whatever its market.

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, Styled,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::{CivilDate, UtcMillis};
use lines_feed::store::ChartStore;
use lines_trading::journal::stats::Stats;
use lines_trading::journal::{Edit, Trade, price_value};
use lines_trading::{JournalShared, Money, Side};

use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pane_columns::{
    self, ColumnCmd, ColumnMenu, Columns, HEADER_ROW_H, HeaderHit, JournalCol, Table,
};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::trading::Trading;
use crate::wsdoc::{JournalState, PaneDoc};

const ROW_H: f32 = 18.0;
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 6.0;

/// One trade as the table shows it (formatted once per journal change).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    /// Indexed by [`JournalCol`].
    pub cells: [String; 11],
    /// Net P&L sign: colours the P&L cell (its text is signed too).
    pub sign: i8,
    pub note: String,
    pub tags: Vec<String>,
}

/// Format a trade for the table.
pub fn row(t: &Trade) -> Row {
    let product = lines_feed::catalog::get(t.contract.product);
    let tick = product.as_ref().map_or(1, |p| p.price.tick);
    let price = |v: Option<f64>| match (v, &product) {
        (Some(v), Some(p)) => decimal(price_value(p, v), p.price.scale.den()),
        _ => "—".to_owned(),
    };
    let qty = match &product {
        Some(p) => {
            let mut q = String::new();
            let _ = p.qty.write(lines_core::Qty(t.max_qty), &mut q);
            q
        }
        None => t.max_qty.to_string(),
    };
    let contract = product.as_ref().map_or_else(
        || format!("{:?}", t.contract.product),
        |p| t.contract.label(p).as_str().to_owned(),
    );
    let mut time = clock_text(t.entry_time);
    if t.incomplete {
        time.push_str(" ≈");
    }
    let signed_ticks = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| format!("{v:+.2}"));
    let ticks = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| format!("{v:.1}"));
    let net = t.net();
    Row {
        id: t.id.clone(),
        cells: [
            time,
            contract,
            if t.side == Side::Buy { "Long" } else { "Short" }.to_owned(),
            qty,
            price(t.avg_entry()),
            price(t.avg_exit()),
            signed_money(net),
            signed_ticks(t.ticks(tick)),
            duration_text(t.duration_ms()),
            format!(
                "{} / {}  ({}/{} t)",
                signed_money(t.mae),
                signed_money(t.mfe),
                ticks(t.mae_ticks(tick)),
                ticks(t.mfe_ticks(tick))
            ),
            t.tags.join(", "),
        ],
        sign: if net > Money::ZERO {
            1
        } else if net < Money::ZERO {
            -1
        } else {
            0
        },
        note: t.note.clone(),
        tags: t.tags.clone(),
    }
}

/// `+310.02`, `-150.00`, `0.00`.
pub fn signed_money(m: Money) -> String {
    if m > Money::ZERO {
        format!("+{m}")
    } else {
        m.to_string()
    }
}

/// A price with the decimals its scale implies (`den` 100 → at least 2), more if the value
/// has them (averages), at most 6, without trailing zeros past the minimum.
fn decimal(v: f64, den: i64) -> String {
    let min = (den.max(1) as f64).log10().ceil() as usize;
    let s = format!("{v:.6}");
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut frac = frac.trim_end_matches('0').to_owned();
    while frac.len() < min {
        frac.push('0');
    }
    if frac.is_empty() {
        int.to_owned()
    } else {
        format!("{int}.{frac}")
    }
}

/// `11-05 14:31:07` (UTC, like the chart's axis).
pub fn clock_text(t: UtcMillis) -> String {
    let d = CivilDate::from_utc(t);
    let s = t.0.rem_euclid(86_400_000) / 1000;
    format!(
        "{:02}-{:02} {:02}:{:02}:{:02}",
        d.month,
        d.day,
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `45s`, `2m 05s`, `1h 03m`.
pub fn duration_text(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

/// The stats strip: `Today 2026-11-05: 4 trades · win 75% · net +412.50 · …`.
pub fn stats_text(label: &str, s: &Stats, open: usize) -> String {
    let mut out = format!(
        "{label}: {} trade{}",
        s.count,
        if s.count == 1 { "" } else { "s" }
    );
    if s.count > 0 {
        let money = |m: Option<Money>| m.map_or_else(|| "—".to_owned(), signed_money);
        out.push_str(&format!(
            "  ·  win {:.0}%  ·  net {}  ·  avg win {}  ·  avg loss {}  ·  expectancy {}  ·  PF {}  ·  max DD {}  ·  streak {}",
            s.win_rate().unwrap_or(0.0) * 100.0,
            signed_money(s.net),
            money(s.avg_win()),
            money(s.avg_loss()),
            money(s.expectancy()),
            s.profit_factor()
                .map_or_else(|| "—".to_owned(), |p| format!("{p:.2}")),
            s.max_drawdown,
            match s.streak {
                0 => "—".to_owned(),
                n if n > 0 => format!("W{n}"),
                n => format!("L{}", -n),
            },
        ));
    }
    if open > 0 {
        out.push_str(&format!("  ·  {open} open"));
    }
    out
}

/// Which field of the inline editor has the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Tags,
    Note,
}

/// The inline tags/note editor (pure: keys in, an [`Edit`] out).
#[derive(Clone, Debug, PartialEq)]
pub struct Editor {
    pub id: String,
    pub tags: String,
    pub note: String,
    pub field: Field,
}

/// What a key did to the editor.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorKey {
    /// Still editing.
    Typing,
    Save(Edit),
    Cancel,
    /// Not the editor's key.
    Ignored,
}

impl Editor {
    pub fn new(r: &Row) -> Editor {
        Editor {
            id: r.id.clone(),
            tags: r.tags.join(", "),
            note: r.note.clone(),
            field: Field::Tags,
        }
    }

    fn text(&mut self) -> &mut String {
        match self.field {
            Field::Tags => &mut self.tags,
            Field::Note => &mut self.note,
        }
    }

    /// A key (`key` as GPUI names it; `typed`: the character it types, if any).
    pub fn key(&mut self, key: &str, typed: Option<&str>) -> EditorKey {
        match key {
            "enter" => EditorKey::Save(self.edit()),
            "escape" => EditorKey::Cancel,
            "tab" => {
                self.field = match self.field {
                    Field::Tags => Field::Note,
                    Field::Note => Field::Tags,
                };
                EditorKey::Typing
            }
            "backspace" => {
                self.text().pop();
                EditorKey::Typing
            }
            _ => match typed.filter(|c| !c.chars().any(char::is_control)) {
                Some(c) => {
                    let t = self.text();
                    if t.chars().count() < 400 {
                        t.push_str(c);
                    }
                    EditorKey::Typing
                }
                None => EditorKey::Ignored,
            },
        }
    }

    /// Tags split at commas (the journal trims and de-duplicates them).
    pub fn edit(&self) -> Edit {
        Edit {
            id: self.id.clone(),
            tags: self.tags.split(',').map(str::to_owned).collect(),
            note: self.note.trim().to_owned(),
        }
    }
}

pub struct JournalView {
    feed: PaneFeed,
    common: PaneCommon,
    focus: FocusHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    frozen: bool,
    journal: Option<Arc<JournalShared>>,
    seen_revision: u64,
    /// The journal revision `rows` and the stats were built from.
    built: Option<u64>,
    rows: Vec<Row>,
    stats_today: String,
    stats_all: String,
    all_dates: bool,
    selected: Option<String>,
    /// First row shown.
    scroll: usize,
    editor: Option<Editor>,
    glyphs: Glyphs,
    cols: Columns,
    bounds: Bounds<Pixels>,
    /// Where the stats strip is (a click toggles all dates).
    strip: Bounds<Pixels>,
}

impl EventEmitter<FeedChanged> for JournalView {}
impl EventEmitter<ColumnMenu> for JournalView {}

impl Pane for JournalView {
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
        let columns = self.cols.save(&JournalCol::ALL);
        doc.journal = (self.all_dates || !columns.is_empty()).then_some(JournalState {
            all_dates: self.all_dates,
            columns,
        });
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    /// The journal's revision, not the market's: the pane repaints when a trade changes.
    fn revision(&self) -> u64 {
        self.journal.as_ref().map_or(0, |j| j.revision())
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.store = feed.store().clone();
        self.feed = feed;
        self.built = None;
        cx.notify();
    }
}

impl JournalView {
    pub fn new(feed: PaneFeed, theme: Theme, frozen: bool, cx: &mut Context<Self>) -> Self {
        let store = feed.store().clone();
        pane::watch_store(cx);
        JournalView {
            feed,
            common: PaneCommon::default(),
            focus: cx.focus_handle(),
            store,
            theme,
            frozen,
            journal: Trading::try_global(cx).map(|t| t.journal.clone()),
            seen_revision: u64::MAX,
            built: None,
            rows: Vec::new(),
            stats_today: String::new(),
            stats_all: String::new(),
            all_dates: false,
            selected: None,
            scroll: 0,
            editor: None,
            glyphs: Glyphs::new(TEXT_PX),
            cols: Columns::new(Table::Journal),
            bounds: Bounds::default(),
            strip: Bounds::default(),
        }
    }

    pub fn restore(&mut self, state: &JournalState) {
        self.all_dates = state.all_dates;
        self.cols.load(&JournalCol::ALL, &state.columns);
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

    pub fn all_dates(&self) -> bool {
        self.all_dates
    }

    /// Stats over all dates, or the current trade date.
    pub fn toggle_all_dates(&mut self, cx: &mut Context<Self>) {
        self.all_dates = !self.all_dates;
        cx.notify();
    }

    /// Edit the selected trade's tags and note (else the newest trade's).
    pub fn start_edit(&mut self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        let i = self
            .selected_index()
            .or((!self.rows.is_empty()).then_some(0));
        let r = i
            .and_then(|i| self.rows.get(i))
            .ok_or("no trades to edit yet")?;
        self.selected = Some(r.id.clone());
        self.editor = Some(Editor::new(r));
        cx.notify();
        Ok(())
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.rows.iter().position(|r| &r.id == id)
    }

    /// The current trade date of the pane's market: on a replay's (or a screenshot's)
    /// market time, else the wall clock.
    fn today(&self) -> CivilDate {
        let replay =
            self.frozen || matches!(self.feed.config.source, lines_feed::Source::Replay { .. });
        let d = self.store.try_read();
        let now = d
            .as_ref()
            .and_then(|d| d.market_time)
            .filter(|_| replay)
            .unwrap_or_else(lines_feed::now_utc);
        match d {
            Some(d) => lines_feed::disk::partition::trade_date(&d.product, now),
            None => CivilDate::from_utc(now),
        }
    }

    /// Rebuild rows and stats if the journal moved. Returns false if the journal was busy
    /// this instant (try again on the next poll).
    fn rebuild(&mut self) -> bool {
        let Some(j) = self.journal.clone() else {
            return true;
        };
        let rev = j.revision();
        if self.built == Some(rev) {
            return true;
        }
        let Some(journal) = j.try_read() else {
            return false;
        };
        self.rows = journal.trades().iter().rev().map(row).collect();
        let today = self.today();
        let open = journal.open_trades().count();
        self.stats_today = stats_text(
            &format!("Today {today}"),
            &journal.stats(|t| t.trade_date == today),
            open,
        );
        self.stats_all = stats_text("All dates", &journal.stats(|_| true), open);
        drop(journal);
        self.built = Some(rev);
        true
    }

    fn table_xy(&self, p: Point<Pixels>) -> (f32, f32) {
        (f(p.x - self.bounds.origin.x), f(p.y - self.bounds.origin.y))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.strip.contains(&ev.position) {
            self.toggle_all_dates(cx);
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
            HeaderHit::Outside => {
                let i = ((y - HEADER_ROW_H) / ROW_H).floor();
                if i >= 0.0
                    && let Some(r) = self.rows.get(self.scroll + i as usize)
                {
                    self.selected = Some(r.id.clone());
                    if ev.click_count >= 2 {
                        self.editor = Some(Editor::new(r));
                    }
                    cx.notify();
                }
            }
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

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if let Some(ed) = &mut self.editor {
            if m.control || m.alt || m.platform {
                return; // workspace commands still work while editing
            }
            match ed.key(k.key.as_str(), k.key_char.as_deref()) {
                EditorKey::Typing => {}
                EditorKey::Cancel => self.editor = None,
                EditorKey::Save(e) => {
                    self.editor = None;
                    if let Some(j) = &self.journal {
                        j.edit(e);
                    }
                }
                EditorKey::Ignored => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.control || m.alt || m.platform {
            return;
        }
        let n = self.rows.len();
        let i = self.selected_index();
        let next = match k.key.as_str() {
            "down" => Some(i.map_or(0, |i| (i + 1).min(n.saturating_sub(1)))),
            "up" => Some(i.map_or(0, |i| i.saturating_sub(1))),
            "enter" => {
                let _ = self.start_edit(cx);
                cx.stop_propagation();
                return;
            }
            _ => None,
        };
        let Some(next) = next.filter(|_| n > 0) else {
            return;
        };
        self.selected = Some(self.rows[next].id.clone());
        let visible = self.visible_rows();
        if next < self.scroll {
            self.scroll = next;
        } else if next >= self.scroll + visible {
            self.scroll = next + 1 - visible;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &gpui::ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dy = f(ev.delta.pixel_delta(window.line_height()).y);
        let rows = (dy / ROW_H).round() as i64;
        if rows != 0 {
            let max = self.rows.len().saturating_sub(1) as i64;
            self.scroll = (self.scroll as i64 - rows).clamp(0, max) as usize;
            cx.notify();
        }
    }

    fn visible_rows(&self) -> usize {
        (((f(self.bounds.size.height) - HEADER_ROW_H) / ROW_H).floor() as usize).max(1)
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        window.paint_quad(fill(bounds, t.bg));
        self.cols.measure_headers(&mut self.glyphs, &t, PAD, window);
        let rows = self.visible_rows();
        let end = (self.scroll + rows).min(self.rows.len());
        for i in self.scroll..end {
            for (c, col) in JournalCol::ALL.iter().enumerate() {
                if *col == JournalCol::Tags {
                    continue; // fills; never widens the table
                }
                let tw = self
                    .glyphs
                    .width(&self.rows[i].cells[c], t.text_main, false, window);
                self.cols.need(c, tw, PAD);
            }
        }
        self.cols.layout(w);
        let left: [bool; 11] = JournalCol::ALL.map(JournalCol::left_aligned);
        self.cols
            .paint_header(&mut self.glyphs, &t, bounds, PAD, &left, window, cx);
        let oy = top + HEADER_ROW_H;
        if self.rows.is_empty() {
            let msg = if self.journal.is_some() {
                "no closed trades yet: trades appear here when a position goes back to flat"
            } else {
                "trading isn't running"
            };
            self.glyphs.paint(
                msg,
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
        let selected = self.selected_index();
        for (n, i) in (self.scroll..end).enumerate() {
            let y = oy + n as f32 * ROW_H;
            if y + ROW_H > top + h + 0.5 {
                break;
            }
            let r = &self.rows[i];
            for (c, col) in JournalCol::ALL.iter().enumerate() {
                let Some((x, cw)) = self.cols.cell(c, ox) else {
                    continue;
                };
                let color = match col {
                    JournalCol::Pnl if r.sign > 0 => t.up,
                    JournalCol::Pnl if r.sign < 0 => t.down,
                    JournalCol::Time | JournalCol::Duration | JournalCol::MaeMfe => t.text_dim,
                    _ => t.text_main,
                };
                let text = r.cells[c].as_str();
                let bold = *col == JournalCol::Pnl;
                let cell = Bounds::new(point(px(x), px(y)), size(px(cw), px(ROW_H)));
                window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                    if col.left_aligned() {
                        self.glyphs
                            .paint(text, x + PAD, y, ROW_H, color, bold, window, cx);
                    } else {
                        self.glyphs.paint_right(
                            text,
                            x + cw - PAD,
                            y,
                            ROW_H,
                            color,
                            bold,
                            window,
                            cx,
                        );
                    }
                });
            }
            if Some(i) == selected {
                // Selection: an accent outline (no fill, so text contrast is untouched).
                for (yy, hh) in [(y, hair * 2.0), (y + ROW_H - hair * 2.0, hair * 2.0)] {
                    window.paint_quad(fill(
                        Bounds::new(point(px(ox), px(yy)), size(px(w), px(hh))),
                        t.accent,
                    ));
                }
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(y)), size(px(3.0), px(ROW_H))),
                    t.accent,
                ));
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + ROW_H - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        let body = Bounds::new(point(px(ox), px(oy)), size(px(w), px(h - HEADER_ROW_H)));
        self.cols.paint_rules(&t, body, window);
    }

    fn header(&self) -> impl IntoElement {
        let t = self.theme;
        let right = match &self.journal {
            Some(j) if j.persistent() => "saved",
            Some(_) => "this session only",
            None => "",
        };
        div()
            .flex()
            .flex_row()
            .justify_between()
            .gap_2()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(pane::controls_gutter(crate::layout::PaneKind::Journal)))
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
                            .children(pane::link_chip(self.common.link, &t))
                            .child(div().text_color(t.text_main).child("Journal")),
                    )
                    .child(div().text_color(t.text_dim).child(right)),
            )
    }

    fn strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let text = if self.all_dates {
            self.stats_all.clone()
        } else {
            self.stats_today.clone()
        };
        let entity = cx.entity();
        div()
            .relative()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .text_color(t.text_main)
            .whitespace_nowrap()
            .overflow_hidden()
            .child(text)
            .child(
                canvas(
                    move |bounds, _, cx| entity.update(cx, |v, _| v.strip = bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }

    fn editor_bar(&self) -> Option<impl IntoElement> {
        let ed = self.editor.as_ref()?;
        let t = self.theme;
        let field = |label: &'static str, text: &str, active: bool| {
            div()
                .flex()
                .flex_row()
                .gap_1()
                .child(div().text_color(t.text_dim).child(label))
                .child(
                    div()
                        .px_1()
                        .min_w(px(80.0))
                        .border_1()
                        .border_color(if active { t.accent } else { t.axis_line })
                        .text_color(t.text_main)
                        .child(if active {
                            format!("{text}▏")
                        } else {
                            text.to_owned()
                        }),
                )
        };
        Some(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .items_center()
                .px_2()
                .py_1()
                .bg(t.panel)
                .border_t_1()
                .border_color(t.axis_line)
                .text_size(px(12.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .child(field("Tags", &ed.tags, ed.field == Field::Tags))
                .child(field("Note", &ed.note, ed.field == Field::Note))
                .child(
                    div()
                        .text_color(t.text_dim)
                        .child("Tab: next field  ·  Enter: save  ·  Esc: cancel"),
                ),
        )
    }
}

impl Render for JournalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.rebuild() {
            // The trading thread held the journal this instant: draw again on the next poll.
            self.seen_revision = u64::MAX;
        }
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
            .child(self.header())
            .child(self.strip(cx))
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
                    .child(body),
            )
            .children(self.editor_bar())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{ContractKey, Month, Price, Qty};
    use lines_feed::catalog;
    use lines_trading::journal::{Journal, Record};
    use lines_trading::{AccountId, Event, FillEvent, OrderId};

    /// Long 4 ES in three fills, out in two: net +310.02 after $2.48 of fees.
    fn trade() -> Trade {
        let es = ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap());
        let t0 = UtcMillis::parse_iso("2026-11-05T14:31:07Z").unwrap().0;
        let mut j = Journal::new();
        let mut out = Vec::new();
        for (i, (side, p, q, fee_cents, s)) in [
            (Side::Buy, 600_000, 2, 62, 0),
            (Side::Buy, 600_025, 1, 31, 1),
            (Side::Buy, 599_950, 1, 31, 30),
            (Side::Sell, 600_200, 2, 62, 90),
            (Side::Sell, 600_100, 2, 62, 120),
        ]
        .into_iter()
        .enumerate()
        {
            out.extend(j.on_event(&Event::Fill(FillEvent {
                account: AccountId::new("paper"),
                order: OrderId(1),
                contract: es,
                side,
                price: Price(p),
                qty: Qty(q),
                fee: Money::cents(fee_cents),
                realised: Money::ZERO,
                trade_id: format!("f{i}"),
                reconciled: false,
                at: UtcMillis(t0 + s * 1000),
            })));
        }
        let mut t = out.pop().expect("one round trip");
        t.tags = vec!["breakout".into(), "A+".into()];
        t
    }

    #[test]
    fn rows_are_signed_and_readable() {
        let r = row(&trade());
        let c = |col: JournalCol| r.cells[col.index()].as_str();
        assert_eq!(c(JournalCol::Time), "11-05 14:31:07");
        assert_eq!(c(JournalCol::Contract), "ESZ26");
        assert_eq!(c(JournalCol::Side), "Long");
        assert_eq!(c(JournalCol::Qty), "4");
        assert_eq!(c(JournalCol::Entry), "5999.9375");
        assert_eq!(c(JournalCol::Exit), "6001.50");
        assert_eq!(c(JournalCol::Pnl), "+310.02");
        assert_eq!(c(JournalCol::Ticks), "+6.25");
        assert_eq!(c(JournalCol::Duration), "2m 00s");
        assert!(
            c(JournalCol::MaeMfe).starts_with("-87.50 / +412.50  ("),
            "{}",
            c(JournalCol::MaeMfe)
        );
        assert_eq!(c(JournalCol::Tags), "breakout, A+");
        assert_eq!(r.sign, 1);
        assert_eq!(duration_text(45_000), "45s");
        assert_eq!(duration_text(3_780_000), "1h 03m");
        assert_eq!(signed_money(Money::ZERO), "0.00");
        assert_eq!(decimal(6001.0, 100), "6001.00");
        assert_eq!(decimal(0.5, 1), "0.5");
    }

    #[test]
    fn stats_strip_reads_like_a_sentence() {
        let j = Journal::from_records([Record::Trade(Box::new(trade()))]);
        let s = j.stats(|_| true);
        assert_eq!(
            stats_text("Today 2026-11-05", &s, 1),
            "Today 2026-11-05: 1 trade  ·  win 100%  ·  net +310.02  ·  avg win +310.02  ·  avg loss —  ·  expectancy +310.02  ·  PF —  ·  max DD 0.00  ·  streak W1  ·  1 open"
        );
        assert_eq!(
            stats_text("All dates", &Stats::default(), 0),
            "All dates: 0 trades"
        );
    }

    #[test]
    fn editor_types_switches_saves_and_cancels() {
        let r = row(&trade());
        let mut ed = Editor::new(&r);
        assert_eq!(ed.tags, "breakout, A+");
        assert_eq!(ed.key("backspace", None), EditorKey::Typing);
        assert_eq!(ed.key("backspace", None), EditorKey::Typing);
        for c in [",", " ", "o", "p", "e", "n"] {
            ed.key(c, Some(c));
        }
        assert_eq!(ed.key("tab", None), EditorKey::Typing);
        for c in ["g", "o", "o", "d"] {
            ed.key(c, Some(c));
        }
        assert_eq!(ed.key("f5", None), EditorKey::Ignored);
        assert_eq!(
            ed.key("escape-ish", Some("\u{1b}")),
            EditorKey::Ignored,
            "control characters never type"
        );
        let EditorKey::Save(e) = ed.key("enter", None) else {
            panic!("saves")
        };
        // "breakout, A+" less two characters, then ", open": an empty tag in the middle.
        assert_eq!(e.tags, ["breakout", " ", " open"]);
        assert_eq!(e.note, "good");
        // The journal tidies the tags.
        let mut j = Journal::from_records([Record::Trade(Box::new(trade()))]);
        assert_eq!(j.edit(e).unwrap().tags, ["breakout", "open"]);
        assert_eq!(Editor::new(&r).key("escape", None), EditorKey::Cancel);
    }
}
