//! The option chain pane (decision 0028): an underlying's listed options for one expiry,
//! calls on the left and puts on the right of the strike column, centered on the money.
//!
//! - **Columns**, mirrored around the strike: open interest, volume, IV, delta, last, bid,
//!   ask | strike | bid, ask, last, delta, IV, volume, open interest. A narrow pane leaves out
//!   whole columns from the outside in (open interest first), never part of a number; bid,
//!   ask and the strike always show.
//! - **The underlying's last price** is a row of its own between the strikes it falls
//!   between: a dashed line labelled with the symbol and price. Each side's in-the-money cells are shaded (calls
//!   below the price, puts above it); the line says where that changes, so the shading isn't
//!   the only cue.
//! - **Missing** values read `—`; the feed's state ("ended", "connecting", …) is a word in the
//!   header, as on the chart, and a stale feed dims the pane like every other.
//! - **Expiry chips** (the soonest 8) are whole: those that don't fit drop off the end
//!   entirely (onto a hidden second line), never cut.
//! - **Keys** (the pane focused): Up/Down move by strike, Page Up/Down by a page, Home goes
//!   back to the money, Left/Right change the expiry, Tab switches calls and puts. Enter will
//!   open an order ticket with paper options (decision 0028, phase 3); until then it says so.
//!   Clicking a cell selects its strike and side; clicking an expiry chip selects it. Typing
//!   a symbol (or Ctrl+K) opens the symbol picker, as in the other panes.
//!
//! Paint reads the store's chains under a `try_lock` and formats every cell into a stack
//! string: no allocation. The pane repaints on the store's revision (chain quotes don't move
//! the chart revision).

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Render, Styled, Window, canvas, div, fill, point,
    prelude::*, px, size,
};
use lines_core::{CivilDate, FixedStr, OptionChain, OptionQuote, Price, Product, Right};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;

use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::wsdoc::PaneDoc;

const ROW_H: f32 = 18.0;
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 8.0;
/// The CALLS / PUTS row and the column titles.
const HEAD_H: f32 = 2.0 * ROW_H;
/// Expiry chips in the header.
const MAX_EXPIRIES: usize = 8;

/// One table column: a quote field of one side, or the strike.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Col {
    Oi,
    Vol,
    Iv,
    Delta,
    Last,
    Bid,
    Ask,
}

impl Col {
    /// Calls, read left to right toward the strike (puts mirror it).
    pub const CALLS: [Col; 7] = [
        Col::Oi,
        Col::Vol,
        Col::Iv,
        Col::Delta,
        Col::Last,
        Col::Bid,
        Col::Ask,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Col::Oi => "OI",
            Col::Vol => "Vol",
            Col::Iv => "IV",
            Col::Delta => "Δ",
            Col::Last => "Last",
            Col::Bid => "Bid",
            Col::Ask => "Ask",
        }
    }

    /// Left out first when the pane is narrow (higher = earlier). Bid and ask never are.
    fn drop_rank(self) -> Option<u8> {
        match self {
            Col::Oi => Some(5),
            Col::Vol => Some(4),
            Col::Iv => Some(3),
            Col::Delta => Some(2),
            Col::Last => Some(1),
            Col::Bid | Col::Ask => None,
        }
    }

    /// The widest text the column shows, for its width.
    fn sample(self) -> &'static str {
        match self {
            Col::Oi | Col::Vol => "8,888,888",
            Col::Iv => "888.8%",
            Col::Delta => "-0.88",
            Col::Last | Col::Bid | Col::Ask => "8888.88",
        }
    }
}

/// Which columns fit `width` (each side's columns in [`Col::CALLS`] order): every column's
/// width is `cols[i]` on each side plus the strike's; the outermost go first, both sides
/// alike, so the table stays symmetric.
pub fn fit_columns(width: f32, strike: f32, cols: [f32; 7]) -> [bool; 7] {
    let mut shown = [true; 7];
    let used = |shown: &[bool; 7]| {
        strike
            + 2.0
                * cols
                    .iter()
                    .zip(shown)
                    .filter(|(_, s)| **s)
                    .map(|(w, _)| w)
                    .sum::<f32>()
    };
    for rank in (1..=5).rev() {
        if used(&shown) <= width {
            break;
        }
        for (i, c) in Col::CALLS.iter().enumerate() {
            if c.drop_rank() == Some(rank) {
                shown[i] = false;
            }
        }
    }
    shown
}

/// The chain row at visual row `v` (`None`: the underlying's price row), with the window
/// starting at chain row `start` and the price row at visual `line_at`.
pub fn row_at(v: usize, start: usize, line_at: Option<usize>) -> Option<usize> {
    match line_at {
        Some(l) if v == l => None,
        Some(l) if v > l => Some(start + v - 1),
        _ => Some(start + v),
    }
}

/// The visual row of chain row `n` of the window: one lower from the price row on.
pub fn visual_row(n: usize, line_at: Option<usize>) -> usize {
    n + usize::from(line_at.is_some_and(|l| n >= l))
}

/// The rows to show: `fit` rows of `n` around `center` (as centered as the ends allow).
pub fn window(n: usize, fit: usize, center: usize) -> std::ops::Range<usize> {
    if n <= fit {
        return 0..n;
    }
    let start = center.saturating_sub(fit / 2).min(n - fit);
    start..start + fit
}

/// A count with thousands separators (`18,331`).
pub fn write_count(out: &mut impl std::fmt::Write, n: i64) -> std::fmt::Result {
    if n < 0 {
        out.write_char('-')?;
    }
    let mut digits = FixedStr::<24>::new();
    let _ = std::fmt::Write::write_fmt(&mut digits, format_args!("{}", n.unsigned_abs()));
    let d = digits.as_str();
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.write_char(',')?;
        }
        out.write_char(c)?;
    }
    Ok(())
}

/// IV in hundredths of a percent as `14.8%`.
pub fn write_iv(out: &mut impl std::fmt::Write, bp: i32) -> std::fmt::Result {
    let tenths = (i64::from(bp) + if bp >= 0 { 5 } else { -5 }) / 10;
    let sign = if tenths < 0 { "-" } else { "" };
    let a = tenths.abs();
    write!(out, "{sign}{}.{}%", a / 10, a % 10)
}

/// Delta in ten-thousandths as `0.51` / `-0.49` (signed: never color alone).
pub fn write_delta(out: &mut impl std::fmt::Write, e4: i32) -> std::fmt::Result {
    let hundredths = (i64::from(e4) + if e4 >= 0 { 50 } else { -50 }) / 100;
    let sign = if hundredths < 0 { "-" } else { "" };
    let a = hundredths.abs();
    write!(out, "{sign}{}.{:02}", a / 100, a % 100)
}

/// Days from `today` to `expiry` as a chip's tag: `0DTE`, `1d`, `12d`.
pub fn write_dte(
    out: &mut impl std::fmt::Write,
    today: CivilDate,
    expiry: CivilDate,
) -> std::fmt::Result {
    let days = expiry.days() - today.days();
    if days < 0 {
        out.write_str("expired")
    } else if days == 0 {
        out.write_str("0DTE")
    } else {
        write!(out, "{days}d")
    }
}

/// `Nov 9`: an expiry as its chip and the title name it.
fn chip_date(e: CivilDate) -> String {
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][usize::from(e.month.clamp(1, 12)) - 1];
    format!("{month} {}", e.day)
}

/// A cell's text for a quote field (`—` when missing).
fn cell(out: &mut FixedStr<24>, product: &Product, q: Option<&OptionQuote>, col: Col) {
    out.clear();
    let dash = |o: &mut FixedStr<24>| {
        let _ = std::fmt::Write::write_str(o, "—");
    };
    let Some(q) = q else {
        return dash(out);
    };
    let price = |o: &mut FixedStr<24>, p: Option<Price>| match p {
        Some(p) => {
            let _ = product.price.write(p, o);
        }
        None => dash(o),
    };
    match col {
        Col::Bid => price(out, q.bid),
        Col::Ask => price(out, q.ask),
        Col::Last => price(out, q.last),
        Col::Vol => {
            let _ = write_count(out, q.volume.0);
        }
        Col::Oi => {
            let _ = write_count(out, q.open_interest);
        }
        Col::Iv => match q.iv_bp {
            Some(v) => {
                let _ = write_iv(out, v);
            }
            None => dash(out),
        },
        Col::Delta => match q.delta_e4 {
            Some(v) => {
                let _ = write_delta(out, v);
            }
            None => dash(out),
        },
    }
}

pub struct ChainView {
    feed: PaneFeed,
    common: PaneCommon,
    picker: PickerUi,
    focus: FocusHandle,
    frozen: bool,
    store: Arc<ChartStore>,
    theme: Theme,
    seen_revision: u64,
    product: Product,
    /// The expiry asked for (`None`: the soonest).
    expiry: Option<CivilDate>,
    /// The expiries shown in the header, as of the last render.
    expiries: Vec<CivilDate>,
    /// The selected strike and side (`None`: none yet; the view centers on the money).
    selected: Option<Price>,
    side: Right,
    /// Rows shown last frame: Page Up/Down move by it.
    page: usize,
    /// A one-line message in the footer (Enter before tickets exist).
    note: Option<&'static str>,
    glyphs: Glyphs,
    bounds: Bounds<Pixels>,
    /// Column x ranges last frame, for clicks: (call cell start, put cell start, row top).
    layout: Layout,
    frame: Frame,
}

/// Rows kept per frame (a pane taller than this shows this many).
const MAX_ROWS: usize = 256;

/// The visible part of the chain, copied out of the store (so the lock is held only for the
/// copy, and a busy store this instant redraws the last frame instead of going blank).
#[derive(Debug, Default)]
struct Frame {
    /// The chain rows in view (capacity [`MAX_ROWS`]: refreshing never allocates).
    rows: Vec<lines_core::ChainRow>,
    first_row: usize,
    line_at: Option<usize>,
    last: Option<Price>,
    has_chain: bool,
}

impl Frame {
    fn refresh(
        &mut self,
        chain: Option<&OptionChain>,
        last: Option<Price>,
        selected: Option<Price>,
        fit: usize,
    ) {
        self.last = last;
        self.rows.clear();
        let Some(chain) = chain else {
            self.has_chain = false;
            return;
        };
        self.has_chain = true;
        let center = selected
            .and_then(|s| chain.find(s).ok())
            .or_else(|| last.and_then(|l| chain.atm(l)))
            .unwrap_or(chain.rows.len() / 2);
        // One row is the underlying's price when it falls in view.
        let range = window_rows(chain, fit.saturating_sub(1).clamp(1, MAX_ROWS), center);
        self.first_row = range.start;
        self.line_at = last
            .map(|l| chain.split_at(l))
            .filter(|si| *si >= range.start && *si <= range.end)
            .map(|si| si - range.start);
        self.rows.extend_from_slice(&chain.rows[range]);
    }
}

/// Where last frame's columns were (for clicks).
#[derive(Copy, Clone, Debug, Default)]
struct Layout {
    strike_x: (f32, f32),
    first_row: usize,
    /// Visual rows drawn (chain rows plus the price row).
    rows: usize,
    /// The price row's visual index.
    line_at: Option<usize>,
}

impl EventEmitter<FeedChanged> for ChainView {}

impl Pane for ChainView {
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

    /// Nothing to save: an expiry passes, and the view starts on the money.
    fn save_state(&self, _doc: &mut PaneDoc) {}

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.picker.close();
        self.store = feed.store().clone();
        self.product = self.store.read().product.clone();
        self.feed = feed;
        self.expiry = None;
        self.selected = None;
        self.seen_revision = u64::MAX;
        cx.notify();
    }
}

impl ChainView {
    /// A new look (the Settings window's appearance).
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub fn new(feed: PaneFeed, theme: Theme, frozen: bool, cx: &mut Context<Self>) -> Self {
        let store = feed.store().clone();
        let product = store.read().product.clone();
        pane::watch_store(cx);
        ChainView {
            feed,
            common: PaneCommon::default(),
            picker: PickerUi::new(None, cx),
            focus: cx.focus_handle(),
            frozen,
            store,
            theme,
            seen_revision: u64::MAX,
            product,
            expiry: None,
            expiries: Vec::with_capacity(MAX_EXPIRIES),
            selected: None,
            side: Right::Call,
            page: 10,
            note: None,
            glyphs: Glyphs::new(TEXT_PX),
            bounds: Bounds::default(),
            layout: Layout::default(),
            frame: Frame {
                rows: Vec::with_capacity(MAX_ROWS),
                ..Frame::default()
            },
        }
    }

    /// Taking typed text now (the symbol picker).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open()
    }

    /// Move the selection by `delta` strikes (from the money if nothing is selected yet).
    fn move_strike(&mut self, delta: i64) {
        let Some(d) = self.store.try_read() else {
            return;
        };
        let Some(exp) = self.shown_expiry(&d.chains) else {
            return;
        };
        let Some(chain) = d.chains.chain(exp) else {
            return;
        };
        let from = match self.selected.and_then(|s| chain.find(s).ok()) {
            Some(i) => i,
            None => {
                // The first press selects the money (the middle row before any trade).
                let i = d
                    .last_trade
                    .and_then(|l| chain.atm(l))
                    .unwrap_or(chain.rows.len() / 2);
                if let Some(r) = chain.rows.get(i) {
                    self.selected = Some(r.strike);
                }
                return;
            }
        };
        let to = (from as i64 + delta).clamp(0, chain.rows.len() as i64 - 1) as usize;
        self.selected = Some(chain.rows[to].strike);
    }

    fn shown_expiry(&self, chains: &lines_core::OptionChains) -> Option<CivilDate> {
        match self.expiry {
            Some(e) if chains.chain(e).is_some() => Some(e),
            _ => chains.expiries().next(),
        }
    }

    /// The next (`+1`) or previous (`-1`) expiry.
    fn step_expiry(&mut self, delta: i64) {
        let Some(d) = self.store.try_read() else {
            return;
        };
        let all: Vec<CivilDate> = d.chains.expiries().collect();
        let Some(cur) = self.shown_expiry(&d.chains) else {
            return;
        };
        let i = all.iter().position(|e| *e == cur).unwrap_or(0) as i64;
        let j = (i + delta).clamp(0, all.len() as i64 - 1) as usize;
        if all[j] != cur {
            self.expiry = Some(all[j]);
            self.selected = None; // that strike may not exist there: back to the money
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
            let outcome = self.picker.key(ev, today, &self.feed.availability, cx);
            self.picked(outcome, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.control && !m.alt && k.key == "k" {
            self.picker.open("", cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.control || m.alt || m.platform {
            return;
        }
        self.note = None;
        match k.key.as_str() {
            "up" => self.move_strike(-1),
            "down" => self.move_strike(1),
            "pageup" => self.move_strike(-(self.page.max(1) as i64)),
            "pagedown" => self.move_strike(self.page.max(1) as i64),
            "home" => self.selected = None,
            "left" => self.step_expiry(-1),
            "right" => self.step_expiry(1),
            "tab" => {
                self.side = match self.side {
                    Right::Call => Right::Put,
                    Right::Put => Right::Call,
                }
            }
            "enter" => {
                self.note = Some("Order tickets come with paper options trading (not yet)");
            }
            _ => {
                if let Some(letter) = k
                    .key_char
                    .as_deref()
                    .filter(|c| c.len() == 1 && c.as_bytes()[0].is_ascii_alphabetic())
                {
                    self.picker.open(letter, cx);
                } else {
                    return;
                }
            }
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

    /// A click in the table: select the row's strike, and the side by which half was hit.
    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.picker.close() {
            cx.notify();
        }
        let x = f(ev.position.x);
        let y = f(ev.position.y - self.bounds.origin.y);
        if y < HEAD_H {
            return;
        }
        let i = ((y - HEAD_H) / ROW_H).floor() as usize;
        if i >= self.layout.rows {
            return;
        }
        let Some(d) = self.store.try_read() else {
            return;
        };
        let Some(chain) = self.shown_expiry(&d.chains).and_then(|e| d.chains.chain(e)) else {
            return;
        };
        let Some(ri) = row_at(i, self.layout.first_row, self.layout.line_at) else {
            return;
        };
        if let Some(r) = chain.rows.get(ri) {
            self.selected = Some(r.strike);
            if x < self.layout.strike_x.0 {
                self.side = Right::Call;
            } else if x > self.layout.strike_x.1 {
                self.side = Right::Put;
            }
            cx.notify();
        }
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        let t = self.theme;
        window.paint_quad(fill(bounds, t.bg));
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        if w < 1.0 || h < 1.0 {
            return; // not laid out yet (a first frame can be empty and far off)
        }
        let hair = 1.0 / window.scale_factor();
        let fit = (((h - HEAD_H) / ROW_H).floor().max(1.0)) as usize;
        self.page = fit.saturating_sub(1).max(1);
        // Copy what's in view and let go of the store; busy this instant: last frame's rows.
        match self.store.try_read() {
            Some(d) => {
                let chain = self.shown_expiry(&d.chains).and_then(|e| d.chains.chain(e));
                self.frame.refresh(chain, d.last_trade, self.selected, fit);
            }
            None => self.seen_revision = u64::MAX,
        }
        if !self.frame.has_chain {
            let msg = if self.product.options().is_some() {
                "No option chain from this source yet."
            } else {
                "This product lists no options."
            };
            self.glyphs.paint(
                msg,
                ox + PAD,
                top + 6.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }
        let last = self.frame.last;
        // Column widths from their titles and widest values.
        let mut cols = [0.0f32; 7];
        for (i, c) in Col::CALLS.iter().enumerate() {
            let a = self.glyphs.width(c.title(), t.text_dim, false, window);
            let b = self.glyphs.width(c.sample(), t.text_main, false, window);
            cols[i] = a.max(b) + 2.0 * PAD;
        }
        let strike_w = self.glyphs.width("8888.88", t.text_main, true, window) + 2.0 * PAD;
        let shown = fit_columns(w, strike_w, cols);
        let table_w = strike_w
            + 2.0
                * cols
                    .iter()
                    .zip(&shown)
                    .filter(|(_, s)| **s)
                    .map(|(c, _)| c)
                    .sum::<f32>();
        let x0 = ox + ((w - table_w) / 2.0).max(0.0);
        // Calls: left to right in CALLS order; strike; puts: CALLS reversed.
        let mut call_x = [0.0f32; 7];
        let mut x = x0;
        for i in 0..7 {
            if shown[i] {
                call_x[i] = x;
                x += cols[i];
            }
        }
        let strike_x = x;
        x += strike_w;
        // Puts read outward from the strike: bid, ask, last, delta, IV, volume, OI.
        let mut put_x = [0.0f32; 7];
        for i in [5, 6, 4, 3, 2, 1, 0] {
            if shown[i] {
                put_x[i] = x;
                x += cols[i];
            }
        }
        let calls_end = strike_x;
        let puts_start = strike_x + strike_w;
        // Header rows: CALLS / PUTS, then the titles.
        window.paint_quad(fill(
            Bounds::new(point(px(ox), px(top)), size(px(w), px(HEAD_H))),
            t.panel,
        ));
        self.glyphs.paint_right(
            "CALLS",
            calls_end - PAD,
            top,
            ROW_H,
            t.text_main,
            true,
            window,
            cx,
        );
        self.glyphs.paint(
            "PUTS",
            puts_start + PAD,
            top,
            ROW_H,
            t.text_main,
            true,
            window,
            cx,
        );
        let title_y = top + ROW_H;
        for i in 0..7 {
            if !shown[i] {
                continue;
            }
            let c = Col::CALLS[i];
            for cx0 in [call_x[i], put_x[i]] {
                self.glyphs.paint_right(
                    c.title(),
                    cx0 + cols[i] - PAD,
                    title_y,
                    ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
        }
        self.glyphs.paint_right(
            "Strike",
            strike_x + strike_w - PAD,
            title_y,
            ROW_H,
            t.text_dim,
            false,
            window,
            cx,
        );
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox), px(top + HEAD_H - hair)),
                size(px(w), px(hair)),
            ),
            t.axis_line,
        ));
        // Rows: around the selection, else the money (copied in `frame`).
        let line_at = self.frame.line_at;
        self.layout = Layout {
            strike_x: (calls_end, puts_start),
            first_row: self.frame.first_row,
            rows: self.frame.rows.len() + usize::from(line_at.is_some()),
            line_at,
        };
        let mut s = FixedStr::<24>::new();
        for n in 0..self.frame.rows.len() {
            let row = self.frame.rows[n];
            let row = &row;
            let v = visual_row(n, line_at);
            let y = top + HEAD_H + v as f32 * ROW_H;
            // In the money: calls below the underlying's price, puts above it.
            let (call_itm, put_itm) = match last {
                Some(l) => (row.strike < l, row.strike > l),
                None => (false, false),
            };
            if call_itm {
                window.paint_quad(fill(
                    Bounds::new(point(px(x0), px(y)), size(px(calls_end - x0), px(ROW_H))),
                    t.grid,
                ));
            }
            if put_itm {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(puts_start), px(y)),
                        size(px(x - puts_start), px(ROW_H)),
                    ),
                    t.grid,
                ));
            }
            for i in 0..7 {
                if !shown[i] {
                    continue;
                }
                let c = Col::CALLS[i];
                for (side_x, q) in [(call_x[i], row.call.as_ref()), (put_x[i], row.put.as_ref())] {
                    cell(&mut s, &self.product, q, c);
                    self.glyphs.paint_right(
                        s.as_str(),
                        side_x + cols[i] - PAD,
                        y,
                        ROW_H,
                        t.text_main,
                        false,
                        window,
                        cx,
                    );
                }
            }
            s.clear();
            let _ = self.product.price.write(row.strike, &mut s);
            self.glyphs.paint_right(
                s.as_str(),
                strike_x + strike_w - PAD,
                y,
                ROW_H,
                t.text_main,
                true,
                window,
                cx,
            );
            window.paint_quad(fill(
                Bounds::new(
                    point(px(x0), px(y + ROW_H - hair)),
                    size(px(x - x0), px(hair)),
                ),
                t.grid,
            ));
            if self.selected == Some(row.strike) {
                // The selected strike: an accent outline; the selected side: a bar at its
                // outer edge.
                for (yy, hh) in [(y, 2.0 * hair), (y + ROW_H - 2.0 * hair, 2.0 * hair)] {
                    window.paint_quad(fill(
                        Bounds::new(point(px(x0), px(yy)), size(px(x - x0), px(hh))),
                        t.accent,
                    ));
                }
                let bar_x = match self.side {
                    Right::Call => x0,
                    Right::Put => x - 4.0,
                };
                window.paint_quad(fill(
                    Bounds::new(point(px(bar_x), px(y)), size(px(4.0), px(ROW_H))),
                    t.accent,
                ));
            }
        }
        // The underlying's price line between the strikes it falls between.
        if let (Some(l), Some(v)) = (last, line_at) {
            let y = top + HEAD_H + v as f32 * ROW_H + ROW_H / 2.0;
            // Dashes by count, never by adding to a float (which a far-off origin can make
            // stand still).
            let dashes = ((x - x0) / 10.0).ceil().clamp(0.0, 1_000.0) as usize;
            for k in 0..dashes {
                let dx = x0 + k as f32 * 10.0;
                window.paint_quad(fill(
                    Bounds::new(point(px(dx), px(y - hair)), size(px(6.0), px(2.0 * hair))),
                    t.last,
                ));
            }
            s.clear();
            let _ = std::fmt::Write::write_str(&mut s, self.product.symbol);
            let _ = std::fmt::Write::write_char(&mut s, ' ');
            let _ = self.product.price.write(l, &mut s);
            let tw = self.glyphs.width(s.as_str(), t.on_last, true, window) + 2.0 * PAD;
            let tx = strike_x + (strike_w - tw) / 2.0;
            let tag_h = 15.0;
            window.paint_quad(fill(
                Bounds::new(point(px(tx), px(y - tag_h / 2.0)), size(px(tw), px(tag_h))),
                t.last,
            ));
            self.glyphs.paint(
                s.as_str(),
                tx + PAD,
                y - tag_h / 2.0,
                tag_h,
                t.on_last,
                true,
                window,
                cx,
            );
        }
        pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
    }

    fn header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(48);
        let today = pane::today(self.frozen, &self.store);
        self.expiries.clear();
        let mut shown = None;
        let mut word = None;
        if let Some(title) = pane::unavailable_title(&self.feed) {
            left.push_str(&title);
        } else if let Some(d) = self.store.try_read() {
            left.push_str(self.product.symbol);
            if let Some(l) = d.last_trade {
                left.push(' ');
                let mut s = FixedStr::<24>::new();
                let _ = self.product.price.write(l, &mut s);
                left.push_str(s.as_str());
            }
            shown = self.shown_expiry(&d.chains);
            // The chips: the soonest few, shifted so the shown expiry is always among them.
            let at = shown
                .and_then(|s| d.chains.expiries().position(|e| e == s))
                .unwrap_or(0);
            let skip = (at + 1).saturating_sub(MAX_EXPIRIES);
            self.expiries
                .extend(d.chains.expiries().skip(skip).take(MAX_EXPIRIES));
            // Every state but live is a word that never gives way (as on the chart).
            word = pane::status_word(&d.status);
        }
        left.push_str("  Option chain");
        // The shown expiry, named even where its chip doesn't fit.
        if let Some(e) = shown {
            left.push_str(&format!("  {}", chip_date(e)));
        }
        let chips = self.expiries.clone().into_iter().map(|e| {
            let on = Some(e) == shown;
            let mut label = chip_date(e);
            label.push_str(" · ");
            let _ = write_dte(&mut label, today, e);
            div()
                .id(gpui::SharedString::from(format!("expiry-{e}")))
                .flex_shrink_0()
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
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if this.expiry != Some(e) {
                            this.expiry = Some(e);
                            this.selected = None;
                        }
                        cx.notify();
                    }),
                )
        });
        div()
            .flex()
            .flex_row()
            .justify_between()
            .gap_2()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(pane::controls_gutter(crate::layout::PaneKind::Chain)))
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(
                pane::header_content().child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .items_center()
                        .children(pane::link_chip(self.common.link, &t))
                        .child(div().flex_shrink_0().text_color(t.text_main).child(left))
                        .children(
                            word.map(|w| div().flex_shrink_0().text_color(t.text_main).child(w)),
                        )
                        .children(pane::stale_badge(&self.store, &t))
                        .child(
                            // Whole chips only: those that don't fit wrap onto a second line,
                            // which the fixed height hides.
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap_2()
                                .h(px(18.0))
                                .overflow_hidden()
                                .min_w_0()
                                .children(chips),
                        ),
                ),
            )
    }

    fn footer(&self) -> impl IntoElement {
        let t = self.theme;
        div()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_color(t.text_dim)
            .child(self.note.unwrap_or(
                "Up/Down: strike  ·  Left/Right: expiry  ·  Tab: calls / puts  ·  Home: the money  ·  Shaded: in the money",
            ))
    }
}

/// [`window`] over a chain's rows.
fn window_rows(chain: &OptionChain, fit: usize, center: usize) -> std::ops::Range<usize> {
    window(chain.rows.len(), fit, center)
}

impl Render for ChainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let t = self.theme;
        let today = pane::today(self.frozen, &self.store);
        let overlay =
            self.picker
                .overlay(&t, &self.feed.availability, today, Self::picker_click, cx);
        let offer = self.common.offer_switch;
        let body = match pane::unavailable_body(&self.feed, &t, offer, cx) {
            Some(b) => b.into_any_element(),
            None => canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
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
            .child(self.header(cx))
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
            .child(self.footer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(f: impl Fn(&mut String) -> std::fmt::Result) -> String {
        let mut out = String::new();
        f(&mut out).unwrap();
        out
    }

    #[test]
    fn cells_read_like_the_mock() {
        assert_eq!(s(|o| write_count(o, 18_331)), "18,331");
        assert_eq!(s(|o| write_count(o, 999)), "999");
        assert_eq!(s(|o| write_count(o, 1_234_567)), "1,234,567");
        assert_eq!(s(|o| write_count(o, 0)), "0");
        assert_eq!(s(|o| write_iv(o, 1_480)), "14.8%");
        assert_eq!(s(|o| write_iv(o, 1_296)), "13.0%");
        assert_eq!(s(|o| write_delta(o, 5_100)), "0.51");
        assert_eq!(s(|o| write_delta(o, -4_900)), "-0.49");
        assert_eq!(
            s(|o| write_delta(o, -40)),
            "0.00",
            "rounds to zero: no sign"
        );
        assert_eq!(s(|o| write_delta(o, 10_000)), "1.00");
        let d = |y, m, dd| CivilDate::new(y, m, dd).unwrap();
        assert_eq!(s(|o| write_dte(o, d(2026, 11, 9), d(2026, 11, 9))), "0DTE");
        assert_eq!(s(|o| write_dte(o, d(2026, 11, 9), d(2026, 11, 13))), "4d");
        assert_eq!(s(|o| write_dte(o, d(2026, 11, 9), d(2026, 12, 18))), "39d");
        // Missing values read as a dash; prices in the product's format.
        let spy = lines_feed::catalog::get(lines_feed::catalog::SPY).unwrap();
        let mut c = FixedStr::<24>::new();
        cell(&mut c, &spy, None, Col::Bid);
        assert_eq!(c.as_str(), "—");
        let q = OptionQuote {
            bid: Some(Price(164)),
            ..Default::default()
        };
        cell(&mut c, &spy, Some(&q), Col::Bid);
        assert_eq!(c.as_str(), "1.64");
        cell(&mut c, &spy, Some(&q), Col::Ask);
        assert_eq!(c.as_str(), "—");
        cell(&mut c, &spy, Some(&q), Col::Iv);
        assert_eq!(c.as_str(), "—");
    }

    /// A narrow pane leaves out whole columns from the outside in, both sides alike; bid,
    /// ask and the strike always stay.
    #[test]
    fn narrow_panes_drop_outer_columns_first() {
        let cols = [60.0; 7];
        assert_eq!(fit_columns(2_000.0, 70.0, cols), [true; 7]);
        let w_all = 70.0 + 2.0 * 7.0 * 60.0;
        assert_eq!(
            fit_columns(w_all - 1.0, 70.0, cols),
            [false, true, true, true, true, true, true],
            "OI first"
        );
        assert_eq!(
            fit_columns(70.0 + 2.0 * 3.0 * 60.0, 70.0, cols),
            [false, false, false, false, true, true, true]
        );
        assert_eq!(
            fit_columns(10.0, 70.0, cols),
            [false, false, false, false, false, true, true],
            "bid and ask always"
        );
    }

    #[test]
    fn every_cell_and_edge_case() {
        let spy = lines_feed::catalog::get(lines_feed::catalog::SPY).unwrap();
        let q = OptionQuote {
            last: Some(Price(1_234)),
            volume: lines_core::Qty(18_331),
            open_interest: 1_234_567,
            iv_bp: Some(1_480),
            delta_e4: Some(-4_900),
            ..Default::default()
        };
        let mut c = FixedStr::<24>::new();
        for (col, want) in [
            (Col::Last, "12.34"),
            (Col::Vol, "18,331"),
            (Col::Oi, "1,234,567"),
            (Col::Iv, "14.8%"),
            (Col::Delta, "-0.49"),
        ] {
            cell(&mut c, &spy, Some(&q), col);
            assert_eq!(c.as_str(), want, "{col:?}");
        }
        assert_eq!(s(|o| write_count(o, -1_234)), "-1,234");
        assert_eq!(s(|o| write_iv(o, -55)), "-0.6%");
        let d = |y, m, dd| CivilDate::new(y, m, dd).unwrap();
        assert_eq!(
            s(|o| write_dte(o, d(2026, 11, 10), d(2026, 11, 9))),
            "expired"
        );
        assert_eq!(chip_date(d(2026, 12, 18)), "Dec 18");
        // Unequal widths: delta and last go before bid and ask, still outside in.
        let cols = [50.0, 60.0, 55.0, 45.0, 60.0, 60.0, 60.0];
        let w = 70.0 + 2.0 * (60.0 + 60.0 + 60.0 + 45.0);
        assert_eq!(
            fit_columns(w, 70.0, cols),
            [false, false, false, true, true, true, true]
        );
    }

    /// The window, the price row and the rows drawn agree: every chain row in view maps back
    /// from its visual row, and the price row maps to none.
    #[test]
    fn layout_and_clicks_agree() {
        let mut c = lines_core::OptionChains::default();
        let exp = CivilDate::new(2026, 11, 9).unwrap();
        for k in 664..=704 {
            c.apply(
                lines_core::OptionKey {
                    expiry: exp,
                    strike: Price(k * 100),
                    right: Right::Call,
                },
                OptionQuote::default(),
            );
        }
        let chain = c.chain(exp).unwrap();
        for (last, fit, selected) in [
            (Some(Price(68_269)), 20, None),
            (Some(Price(66_000)), 20, None),
            (Some(Price(71_000)), 20, None),
            (Some(Price(68_269)), 1, None),
            (Some(Price(68_269)), 60, None),
            (None, 10, Some(Price(70_000))),
        ] {
            let mut f = Frame::default();
            f.refresh(Some(chain), last, selected, fit);
            assert!(f.rows.len() + usize::from(f.line_at.is_some()) <= fit.max(2));
            for n in 0..f.rows.len() {
                let v = visual_row(n, f.line_at);
                assert_eq!(row_at(v, f.first_row, f.line_at), Some(f.first_row + n));
                assert_eq!(chain.rows[f.first_row + n], f.rows[n]);
            }
            if let Some(l) = f.line_at {
                assert_eq!(row_at(l, f.first_row, f.line_at), None);
                // The price falls between the rows around its row.
                let p = last.unwrap();
                if l > 0 {
                    assert!(f.rows[l - 1].strike <= p);
                }
                if l < f.rows.len() {
                    assert!(f.rows[l].strike > p);
                }
            }
            if let Some(s) = selected {
                assert!(
                    f.rows.iter().any(|r| r.strike == s),
                    "the selection stays in view"
                );
            }
        }
        let mut f = Frame::default();
        f.refresh(None, None, None, 10);
        assert!(!f.has_chain && f.rows.is_empty());
    }

    #[test]
    fn the_price_row_sits_between_strikes() {
        // Window from chain row 10, the price row at visual 3.
        assert_eq!(row_at(0, 10, Some(3)), Some(10));
        assert_eq!(row_at(2, 10, Some(3)), Some(12));
        assert_eq!(row_at(3, 10, Some(3)), None);
        assert_eq!(row_at(4, 10, Some(3)), Some(13));
        assert_eq!(row_at(4, 10, None), Some(14));
    }

    #[test]
    fn rows_center_on_the_money_within_the_ends() {
        assert_eq!(window(41, 20, 20), 10..30);
        assert_eq!(window(41, 20, 2), 0..20);
        assert_eq!(window(41, 20, 40), 21..41);
        assert_eq!(window(5, 20, 3), 0..5);
        assert_eq!(window(41, 1, 7), 7..8);
    }
}
