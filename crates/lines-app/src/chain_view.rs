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
//!   back to the money, Left/Right change the expiry, Tab switches calls and puts, Shift+Tab
//!   the selected cell's bid and ask, Enter opens the ticket on it. Enter always opens the ticket, and a held Enter sends nothing.
//!   Clicking a cell selects its
//!   strike and side; clicking an expiry chip selects it. Typing a symbol (or Ctrl+K) opens the
//!   symbol picker, as in the other panes.
//! - **The ticket** (decision 0028, `chain_ticket.rs`): a click on an ask buys and on a bid
//!   sells, docked at the bottom. Up/Down move the price by tick, Shift+Up/Down the quantity,
//!   digits type a price, Tab closes the position instead, Enter sends, Esc leaves. What the
//!   guard would refuse says so there, in the core's words, and Enter sends nothing.
//! - **Positions and working orders** sit on their strike rows, in a Pos column at each outer
//!   edge, with Close and Cancel; a narrow pane leaves the column out and marks the strike. The
//!   selected series lists its own above the ticket, at any width, in a one-line slot that is
//!   always there. An expiry holding any carries a dot on its chip.
//! - **Flatten and Cancel** (decision 0028, `docs/design.md` §3), shown while the account has
//!   something on the underlying's options: the DOM's commands and keys, never gated by the
//!   feed, the quote or One-click. A DOM's act on its contract; these act on every series of the
//!   underlying, every expiry, and their labels say so. In a reserved slot in the header, or the
//!   footer when narrow, so they move nothing and never appear under a pointer.
//! - **One-click** (off in every new pane, never saved, off again on another symbol or account):
//!   a single click in the focused pane sends a limit at the clicked side, still under the guard,
//!   for the size the switch shows (Shift+Up/Down, or the ticket's chips). While it's on the same
//!   strikes stay in the same places, and keys move only among them; Home or another expiry let
//!   go.
//!
//! - **The option chart** (decision 0029; the Chart chip or the palette, saved with the pane): a
//!   mode of this pane, so the ticket, One-click, positions and exits are this pane's and there is
//!   one trading path. The underlying's chart sits beside the chain, and a strike's row sits at
//!   that strike's price on the chart's own scale (`chain_lock`). While the pointer is on the
//!   chain, or One-click is on, new data doesn't move the scale, so no row moves under the
//!   pointer; the trader's own zoom and pan move both together. The chart shows no orders and
//!   sends none. Too narrow for both, the pane is the plain chain and says so.
//!
//! Paint reads the store's chains under a `try_lock` and formats every cell into a stack
//! string: no allocation. The pane repaints on the store's revision (chain quotes don't move
//! the chart revision).

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Pixels, Render, Styled, Window, canvas, div, fill,
    point, prelude::*, px, size,
};
use lines_core::{
    CivilDate, ContractKey, FixedStr, OptionChain, OptionKey, OptionQuote, Price, Product, Qty,
    Right,
};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;
use lines_trading::trigger::{TriggerKind, TriggerView};
use lines_trading::{AccountId, CommandId, OrderId, Side};

use crate::chain_ticket::{self as ticket, Cell, ShotTicket, Ticket};
use crate::feeds::{Feeds, PaneFeed};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::trade_bar::{self, PaneTrade, TradeDoc};
use crate::trading::Trading;
use crate::wsdoc::PaneDoc;

const ROW_H: f32 = 18.0;
/// Below this the option chart shows only the chain (the rows need the chart to read by).
const CHART_MIN_W: f32 = 760.0;
/// Strikes the chart's auto-fit keeps in view beside the chain, so there are rows to choose from
/// even when the bars span less than one strike.
const CHART_STRIKES: f64 = 10.0;
/// The chain's share of the pane beside the chart.
const CHART_CHAIN_SHARE: f32 = 0.45;
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 8.0;
/// The CALLS / PUTS row and the column titles.
const HEAD_H: f32 = 2.0 * ROW_H;
/// The strips at the top and foot of the option chart's rows, for what the account has past them.
/// Always kept clear of rows, so a marker appearing never covers one or takes its press.
const MARKER_H: f32 = MIN_HIT;
/// How far from a line a press takes hold of it.
const LINE_GRAB: f32 = 4.0;
/// A line's tag.
const TAG_H: f32 = MIN_HIT;
/// The armed rows' most height before they scroll.
const ARMED_MAX_H: f32 = 200.0;
/// Expiry chips in the header.
const MAX_EXPIRIES: usize = 8;
/// The widest thing the Pos column's text shows.
const POS_SAMPLE: &str = "+10 @ 13.10 · S 10 +9";
/// A narrow row's mark beside the quotes: `+10`, `S10`.
const MARK_W: f32 = 22.0;
/// Below this the header leaves out its title, so the account and One-click stay.
const NARROW_HEADER: f32 = 560.0;
/// Below this the exits sit in the footer: the header can't hold them with its other words.
const EXITS_IN_HEADER: f32 = 940.0;
/// Their slot in the header: both buttons, for any listed symbol.
const EXITS_SLOT: f32 = 244.0;
/// The ticket's slots, so its buttons hold still as prices and modes change.
const QTY_SLOT: f32 = 158.0;
const PRICE_CHIP: f32 = 72.0;
const SEND_W: f32 = 126.0;
/// Commands whose answers a pane waits for.
const MAX_ASKED: usize = 64;
/// How long to wait for the core's answer before saying it hasn't come.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// The selected series' strip: one line of controls, always the same.
const STRIP_H: f32 = MIN_HIT + 9.0;
/// A control's smallest height (`docs/design.md`).
const MIN_HIT: f32 = 20.0;

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
fn write_chip_date(out: &mut impl std::fmt::Write, e: CivilDate) -> std::fmt::Result {
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][usize::from(e.month.clamp(1, 12)) - 1];
    write!(out, "{month} {}", e.day)
}

fn chip_date(e: CivilDate) -> String {
    let mut s = String::new();
    let _ = write_chip_date(&mut s, e);
    s
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
    /// `expiry` is One-click's pin, not the trader's choice.
    expiry_pinned: bool,
    /// The expiries shown in the header, as of the last render.
    expiries: Vec<CivilDate>,
    /// The selected strike and side (`None`: none yet; the view centers on the money).
    selected: Option<Price>,
    side: Right,
    /// The selected cell of that side: Enter takes it.
    cell: Cell,
    /// Rows shown last frame: Page Up/Down move by it.
    page: usize,
    /// A one-line message in the footer (a refusal of a press with no ticket open).
    note: Option<String>,
    /// The account, the trade cache's revision (`PaneTrade`; quantity is `chain_ticket::size_key`).
    trade: PaneTrade,
    ticket: Option<Ticket>,
    /// A click on a bid or ask sends: off in every new pane, never saved.
    one_click: bool,
    /// Commands waiting for the core's answer (the newest [`MAX_ASKED`]).
    asked: Vec<CommandId>,
    /// Focused in an active window as of the last frame: what a click found. Input's capture
    /// phase moves the focus before the pane hears the click, so now can't say.
    aimed: bool,
    /// The market clock as of the last frame.
    now: Option<lines_core::UtcMillis>,
    /// No quote in view is fresh.
    quotes_stale: bool,
    /// When the ticket's order went.
    sent_at: Option<std::time::Instant>,
    /// Orders the core accepted: the broker can still reject them on arrival.
    watching: Vec<OrderId>,
    /// Screenshot fixture, until the chain has data.
    shot: Option<ShotTicket>,
    glyphs: Glyphs,
    bounds: Bounds<Pixels>,
    /// Column x ranges last frame, for clicks: (call cell start, put cell start, row top).
    layout: Layout,
    frame: Frame,
    /// The option chart's underlying (decision 0029), while that mode is on.
    chart: Option<gpui::Entity<crate::chart_view::ChartView>>,
    _chart_feed: Option<gpui::Subscription>,
    /// The pane's width as of the last frame.
    pane_w: f32,
    /// The chart plot's width when its lines were last painted.
    plot_w: f32,
    /// P&L to close (decision 0029), and what it's of, refreshed each render. Not saved: it's off
    /// each start.
    pnl_on: bool,
    /// The P&L display shows money: without it neither this nor the summary shows.
    show_money: bool,
    traded: Option<Traded>,
    /// The position the Risk / Reward summary is about.
    summary: Option<Traded>,
    /// The main area's top in the window, for the chart's line handles.
    main_top: f32,
    /// Lines on the underlying's price (decision 0029), refreshed each render (capacity kept), and
    /// where each was painted last frame: its y and its tag on the chart.
    lines: Vec<crate::chain_lines::Line>,
    line_ys: [f32; crate::chain_lines::MAX_LINES],
    tag_boxes: [(f32, f32, f32); crate::chain_lines::MAX_LINES],
    /// Stops and Targets not armed yet.
    drafts: Vec<crate::chain_lines::Line>,
    line_drag: Option<LineDrag>,
    /// How many triggers had ended when last looked, for the reasons of those since.
    seen_ended: Option<u64>,
    /// Currency per price unit of one contract, for a tag's estimated P&L; `None`: don't show it.
    tag_money: Option<f64>,
    /// A press on the armed rows waits for the pointer after they change.
    rows_guard: crate::chain_lines::RowsGuard,
    /// Where the armed rows were, so a press knows when they've moved.
    armed_tops: Vec<f32>,
    /// The triggers as last seen, so a busy instant redraws them rather than dropping the rows.
    armed_views: Vec<TriggerView>,
    /// Screenshot fixtures.
    shot_drag: Option<(String, Price)>,
    shot_restored: bool,
    /// The top of what's under the rows (strip, ticket, footer), this frame, beside a chart.
    foot_top: f32,
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
    expiry: Option<CivilDate>,
    /// The account's positions and working orders on this expiry (capacity kept).
    marks: Vec<Mark>,
    /// One-click is on: a press sends, so the rows don't move at all.
    hold: bool,
    /// Holding `held` now.
    holding: bool,
    /// The strikes held in their places (capacity kept), and the price row's slot.
    held: Vec<Price>,
    held_line: Option<usize>,
    /// The selection last frame.
    last_selected: Option<Price>,
    /// Rows sit at their prices on the chart's scale (`ys` holds their tops).
    locked: bool,
    ys: Vec<f32>,
    /// The scale `ys` were placed for.
    locked_key: Option<(f64, f64, f32, f64)>,
    /// The window's bounds when last looked at.
    window_at: Option<(f32, f32, f32, f32)>,
    /// Where the table was last frame: position and width.
    table_at: Option<(f32, f32, f32)>,
    /// Where the chart's plot was last frame, if the rows are on its scale.
    locked_geometry: Option<(f32, f64)>,
    candidates: Vec<crate::chain_lock::Candidate>,
    /// The band the rows may use, in window coordinates.
    area: (f32, f32),
    /// The side P&L to close leaves out.
    hidden_side: Option<Right>,
    /// The markers as painted.
    markers: [Option<crate::chain_lock::MarkerHit>; 2],
    picked: Vec<crate::chain_lock::Candidate>,
}

/// The series P&L to close and the summary are about, and what they start from. Display only.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Traded {
    contract: ContractKey,
    model: Option<crate::option_estimate::Model>,
    /// The position, or 0: then it reads as if `qty` (negative: sold) traded at `basis`.
    position: i64,
    qty: i64,
    /// What it cost, in price units: the position's average, the ticket's price, or the ask.
    basis: Option<f64>,
    /// Currency per price unit of one contract.
    per_unit: f64,
}

impl Traded {
    /// What's held, or what it reads as held: negative is short.
    fn signed(&self) -> i64 {
        if self.position != 0 {
            self.position
        } else {
            self.qty
        }
    }

    fn pnl_at(
        &self,
        model: &crate::option_estimate::Model,
        under: f64,
        at: i64,
    ) -> Option<crate::option_estimate::PnlEstimate> {
        Some(
            model
                .value_at(under, at)
                .pnl(self.signed(), self.basis?, self.per_unit),
        )
    }
}

/// A line being dragged: nothing is sent until it's released on the plot, and only if it can go.
#[derive(Copy, Clone, Debug, PartialEq)]
struct LineDrag {
    line: crate::chain_lines::Line,
    to: Price,
    refused: Option<&'static str>,
    /// Where it was taken hold of.
    press_y: f32,
}

/// What the account has on one option series.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Mark {
    strike: Price,
    right: Right,
    position: i64,
    avg: Option<f64>,
    order: Option<(OrderId, Side, i64, Option<Price>)>,
    /// Working orders on it.
    orders: u32,
}

impl Frame {
    fn mark(&self, strike: Price, right: Right) -> Option<&Mark> {
        self.marks
            .iter()
            .find(|m| m.strike == strike && m.right == right)
    }

    fn refresh_marks(
        &mut self,
        st: &crate::trading::TradeState,
        account: &AccountId,
        product: lines_core::ProductId,
    ) {
        self.marks.clear();
        let Some(expiry) = self.expiry else {
            return;
        };
        for p in st.positions_of(account) {
            if p.contract.product != product {
                continue;
            }
            if let Some(k) = p.contract.option_key().filter(|k| k.expiry == expiry) {
                self.marks.push(Mark {
                    strike: k.strike,
                    right: k.right,
                    position: p.qty,
                    avg: p.avg_price,
                    order: None,
                    orders: 0,
                });
            }
        }
        for o in st.open_orders_of(account) {
            if o.contract.product != product {
                continue;
            }
            let Some(k) = o.contract.option_key().filter(|k| k.expiry == expiry) else {
                continue;
            };
            let first = (o.id, o.side, o.qty.0 - o.filled.0, o.kind.prices().next());
            match self
                .marks
                .iter_mut()
                .find(|m| m.strike == k.strike && m.right == k.right)
            {
                Some(m) => {
                    m.order = m.order.or(Some(first));
                    m.orders += 1;
                }
                None => self.marks.push(Mark {
                    strike: k.strike,
                    right: k.right,
                    position: 0,
                    avg: None,
                    order: Some(first),
                    orders: 1,
                }),
            }
        }
    }
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
        // The rows on screen, before they're replaced: they stay put while the selection is
        // among them, so a press never lands on a strike that moved under the pointer.
        // By the strike on top, not its index: a strike listed since moves the indices, not them.
        let top = self.rows.first().map(|r| r.strike);
        let shown = self.rows.len();
        self.rows.clear();
        let Some(chain) = chain else {
            self.has_chain = false;
            return;
        };
        self.has_chain = true;
        let fit_rows = fit.saturating_sub(1).clamp(1, MAX_ROWS);
        // One-click: the same strikes in the same places, whatever is listed or priced, until the
        // trader moves the selection off them.
        let keep = self.hold
            && self.holding
            && (selected == self.last_selected
                || selected
                    .is_none_or(|s| self.held[..fit_rows.min(self.held.len())].contains(&s)));
        self.last_selected = selected;
        if keep {
            // More room: the next strikes go below, moving nothing.
            while self.held.len() < fit_rows {
                let after = self.held.last().copied();
                let next = chain
                    .rows
                    .iter()
                    .find(|r| after.is_none_or(|a| r.strike > a));
                match next {
                    Some(r) => self.held.push(r.strike),
                    None => break,
                }
            }
            for &strike in self.held.iter().take(fit_rows) {
                self.rows.push(match chain.find(strike) {
                    Ok(i) => chain.rows[i],
                    Err(_) => lines_core::ChainRow {
                        strike,
                        call: None,
                        put: None,
                    },
                });
            }
            // The price row keeps its slot (its tag and the in-the-money shading read the live
            // price); kept apart, so a frame with less room doesn't lose it.
            self.line_at = self.held_line.filter(|l| *l <= self.rows.len());
            return;
        }
        let center = selected
            .and_then(|s| chain.find(s).ok())
            .or_else(|| last.and_then(|l| chain.atm(l)))
            .unwrap_or(chain.rows.len() / 2);
        let was = top
            .and_then(|t| chain.find(t).ok())
            .map_or(0..0, |start| start..start + shown);
        // One row is the underlying's price when it falls in view.
        let selected_at = selected.and_then(|s| chain.find(s).ok());
        let range = window_held(chain.rows.len(), fit_rows, center, selected_at, was);
        self.first_row = range.start;
        self.line_at = last
            .map(|l| chain.split_at(l))
            .filter(|si| *si >= range.start && *si <= range.end)
            .map(|si| si - range.start);
        self.rows.extend_from_slice(&chain.rows[range]);
        self.holding = self.hold;
        if self.hold {
            self.held.clear();
            self.held.extend(self.rows.iter().map(|r| r.strike));
            self.held_line = self.line_at;
        }
    }

    /// The rows at their prices on the chart's scale (`chain_lock`), in `[top, scale.bottom())`.
    /// Held: the same strikes in the same places, until the scale itself changes (only the
    /// trader's own zoom or pan can then).
    fn refresh_locked(
        &mut self,
        chain: Option<&OptionChain>,
        last: Option<Price>,
        selected: Option<Price>,
        scale: &crate::chain_lock::LockedScale,
        top: f32,
        bottom: f32,
    ) {
        use crate::chain_lock::{Candidate, pick_rows};
        self.last = last;
        self.first_row = 0;
        self.line_at = None;
        self.rows.clear();
        let Some(chain) = chain else {
            self.has_chain = false;
            self.ys.clear();
            return;
        };
        self.has_chain = true;
        let key = scale.key();
        if self.hold && self.holding && self.locked_key == Some(key) {
            for &strike in &self.held {
                self.rows.push(match chain.find(strike) {
                    Ok(i) => chain.rows[i],
                    Err(_) => lines_core::ChainRow {
                        strike,
                        call: None,
                        put: None,
                    },
                });
            }
            return;
        }
        let money = last.map(|l| scale.y(l));
        self.candidates.clear();
        for (i, r) in chain.rows.iter().enumerate() {
            let y = scale.y(r.strike);
            let row_top = y - ROW_H / 2.0;
            if row_top < top || row_top + ROW_H > scale.bottom().min(bottom) {
                continue;
            }
            if self.candidates.len() == self.candidates.capacity() {
                break;
            }
            self.candidates.push(Candidate {
                top: row_top,
                index: i as u32,
                priority: selected == Some(r.strike)
                    || self.marks.iter().any(|m| m.strike == r.strike),
                distance: money.map_or(0.0, |m| (y - m).abs()),
            });
        }
        pick_rows(&mut self.candidates, ROW_H, &mut self.picked);
        self.ys.clear();
        self.held.clear();
        for c in &self.picked {
            let row = chain.rows[c.index as usize];
            self.rows.push(row);
            self.ys.push(c.top);
            self.held.push(row.strike);
        }
        self.locked_key = Some(key);
        self.holding = self.hold;
    }

    /// What the account has that no row shows, as the rows now stand (a hold can keep a new
    /// strike out). Marks are per series, so a strike can count twice: two series.
    fn off_rows(
        &self,
        scale: &crate::chain_lock::LockedScale,
        foot_top: f32,
    ) -> crate::chain_lock::OffRows {
        let mut off = crate::chain_lock::OffRows::default();
        for m in &self.marks {
            let row = self.rows.iter().position(|r| r.strike == m.strike);
            let (held, working) = (m.position != 0, m.orders);
            match row.and_then(|n| self.ys.get(n)) {
                // Its row is on screen, and so is its side.
                Some(top) if top + ROW_H <= foot_top && self.hidden_side != Some(m.right) => {}
                // Its row is behind the foot, or its side isn't shown.
                Some(_) => {
                    off.between.held += u32::from(held);
                    off.between.working += working;
                }
                None => crate::chain_lock::tally(
                    &mut off,
                    m.strike,
                    scale.y(m.strike),
                    ROW_H,
                    self.area,
                    held,
                    working,
                ),
            }
        }
        off
    }

    /// The marker at this window point, unless a row is there (one never is: its press wins).
    fn marker_at(&self, x: f32, y: f32) -> Option<crate::chain_lock::MarkerHit> {
        if self.locked_at(y).is_some() {
            return None;
        }
        self.markers
            .iter()
            .flatten()
            .find(|h| h.contains(x, y))
            .copied()
    }

    /// The strike whose row is at window y, if any.
    fn locked_at(&self, y: f32) -> Option<Price> {
        self.ys
            .iter()
            .position(|t| y >= *t && y < *t + ROW_H)
            .and_then(|n| self.rows.get(n))
            .map(|r| r.strike)
    }

    /// The selection `delta` strikes on, among the rows on screen; the middle one to start.
    fn step_held(&self, selected: Option<Price>, delta: i64) -> Option<Price> {
        let last = self.rows.len().checked_sub(1)? as i64;
        let to = match selected.and_then(|s| self.rows.iter().position(|r| r.strike == s)) {
            Some(i) => (i as i64 + delta).clamp(0, last),
            None => last / 2,
        };
        Some(self.rows[to as usize].strike)
    }

    /// The trader moved the rows: nothing is held, and the next frame centers.
    fn release(&mut self) {
        self.rows.clear();
        self.holding = false;
    }
}

/// Where last frame's columns were (for clicks).
#[derive(Copy, Clone, Debug, Default)]
struct Layout {
    strike_x: (f32, f32),
    /// Each side's bid and ask cells: [start, end] (calls, then puts).
    calls: Quotes,
    puts: Quotes,
    /// The Pos cell's action word, per side (`None`: no Pos column).
    actions: Option<[(f32, f32); 2]>,
    first_row: usize,
    /// Visual rows drawn (chain rows plus the price row).
    rows: usize,
    /// The price row's visual index.
    line_at: Option<usize>,
}

/// Where one side's bid and ask cells are.
#[derive(Copy, Clone, Debug, Default)]
struct Quotes {
    bid: (f32, f32),
    ask: (f32, f32),
}

impl Quotes {
    /// The quote cell at `x`, if it's on one.
    fn at(&self, x: f32) -> Option<Cell> {
        if x >= self.bid.0 && x < self.bid.1 {
            Some(Cell::Bid)
        } else if x >= self.ask.0 && x < self.ask.1 {
            Some(Cell::Ask)
        } else {
            None
        }
    }
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

    /// The account, and whether the chart is beside the chain. No other view state: an expiry
    /// passes, and the view starts on the money.
    fn save_state(&self, doc: &mut PaneDoc) {
        doc.trade = self.trade.doc();
        doc.chain_chart = self.chart.is_some();
    }

    /// The account's orders and positions moved, or the core answered.
    fn changed(&mut self) -> bool {
        let moved = self.trade.changed();
        self.take_answers() || moved
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.picker.close();
        self.store = feed.store().clone();
        self.product = self.store.read().product.clone();
        let follow = feed.sub.is_some();
        self.feed = feed;
        self.expiry = None;
        self.expiry_pinned = false;
        self.selected = None;
        // A ticket or a One-click press belongs to the symbol it was opened on, and so do the
        // rows the last frame drew.
        self.leave_ticket();
        self.frame.release();
        self.frame.marks.clear();
        self.frame.has_chain = false;
        self.layout = Layout::default();
        self.quotes_stale = false;
        self.one_click = false;
        self.note = None;
        self.line_drag = None;
        self.drafts.clear();
        self.trade.feed_changed(&self.store, follow, cx);
        if let Some(chart) = self.chart.clone() {
            let next = self.feed.clone();
            chart.update(cx, |c, cx| {
                if !Arc::ptr_eq(c.feed().store(), next.store()) {
                    c.set_feed(next, cx);
                }
            });
        }
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
        // Paper fills see this pane's own feed, its option quotes included.
        if feed.sub.is_some()
            && let Some(t) = Trading::try_global(cx)
        {
            t.follow(&store);
        }
        let shot = Trading::try_global(cx).and_then(|t| t.start_chain_ticket.clone());
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
            expiry_pinned: false,
            expiries: Vec::with_capacity(MAX_EXPIRIES),
            selected: None,
            side: Right::Call,
            cell: Cell::Ask,
            page: 10,
            note: None,
            trade: PaneTrade::new(None, cx),
            ticket: None,
            one_click: false,
            asked: Vec::new(),
            aimed: false,
            now: None,
            quotes_stale: false,
            sent_at: None,
            watching: Vec::new(),
            shot,
            glyphs: Glyphs::new(TEXT_PX),
            bounds: Bounds::default(),
            layout: Layout::default(),
            chart: None,
            _chart_feed: None,
            pane_w: 0.0,
            plot_w: 0.0,
            pnl_on: false,
            show_money: true,
            traded: None,
            summary: None,
            main_top: 0.0,
            lines: Vec::with_capacity(crate::chain_lines::MAX_LINES),
            line_ys: [f32::NAN; crate::chain_lines::MAX_LINES],
            tag_boxes: [(0.0, 0.0, 0.0); crate::chain_lines::MAX_LINES],
            drafts: Vec::new(),
            line_drag: None,
            seen_ended: None,
            tag_money: None,
            rows_guard: crate::chain_lines::RowsGuard::default(),
            armed_tops: Vec::new(),
            armed_views: Vec::new(),
            shot_drag: None,
            shot_restored: false,
            foot_top: f32::INFINITY,
            frame: Frame {
                ys: Vec::with_capacity(MAX_ROWS),
                candidates: Vec::with_capacity(4 * MAX_ROWS),
                picked: Vec::with_capacity(MAX_ROWS),
                rows: Vec::with_capacity(MAX_ROWS),
                // Past any real count of series on one expiry (it would grow once, and keep it).
                marks: Vec::with_capacity(512),
                held: Vec::with_capacity(MAX_ROWS),
                ..Frame::default()
            },
        }
    }

    /// Taking typed text now (the symbol picker, the ticket's keys).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open() || self.ticket.is_some()
    }

    /// The chart beside the chain has the keys (a note, a timeframe being typed).
    pub fn chart_takes_text(&self, cx: &App) -> bool {
        self.chart.as_ref().is_some_and(|c| c.read(cx).takes_text())
    }

    /// The pane's saved account, as the other trading panes restore it.
    pub fn restore_trade(&mut self, doc: Option<&TradeDoc>, cx: &mut Context<Self>) {
        self.trade = PaneTrade::new(doc, cx);
    }

    /// The option chart (decision 0029): the underlying's chart beside the chain, on one price
    /// scale, or the plain chain again.
    pub fn toggle_chart(&mut self, cx: &mut Context<Self>) {
        self.rows_moved();
        self.end_line_state();
        self.frame.release();
        self.frame.locked = false;
        self.frame.locked_key = None;
        self.layout = Layout::default();
        if self.chart.take().is_some() {
            self._chart_feed = None;
            cx.notify();
            return;
        }
        let (feed, theme, frozen) = (self.feed.clone(), self.theme, self.frozen);
        let chart = cx.new(|cx| {
            let mut c = crate::chart_view::ChartView::new(
                feed,
                theme,
                frozen,
                crate::chart_view::Fixture::default(),
                cx,
            );
            c.set_view_only();
            c
        });
        // A symbol typed on the chart switches the pane: one symbol, one feed.
        self._chart_feed = Some(cx.subscribe(&chart, |this, _, ev: &FeedChanged, cx| {
            let feed = ev.0.clone();
            this.set_feed(feed.clone(), cx);
            cx.emit(FeedChanged(feed));
        }));
        self.chart = Some(chart);
        self.sync_chart_visible(cx);
        cx.notify();
    }

    /// The chart beside the chain is on and has room.
    fn chart_shown(&self) -> bool {
        self.chart.is_some() && self.pane_w >= CHART_MIN_W
    }

    /// P&L to close on or off (decision 0029). The chain narrows or widens, so every row and
    /// column moves: One-click goes off and says so, as with the chart.
    pub fn toggle_pnl(&mut self, cx: &mut Context<Self>) {
        let showing = self.pnl_shown();
        // On but out of sight (no chart, or no money shown): off, so it doesn't come back.
        if self.pnl_on && !showing {
            self.pnl_on = false;
            self.note = Some("P&L to close is off".to_owned());
            cx.notify();
            return;
        }
        if !showing {
            let why = if !self.chart_shown() {
                Some("P&L to close needs the chart beside the chain")
            } else if !self.show_money {
                Some("P&L to close needs the P&L display to show currency")
            } else {
                None
            };
            if let Some(why) = why {
                self.note = Some(why.to_owned());
                cx.notify();
                return;
            }
            self.pick_traded_side();
        }
        self.pnl_on = !showing;
        self.rows_moved();
        self.frame.release();
        self.frame.locked_key = None;
        self.layout = Layout::default();
        cx.notify();
    }

    /// The side worth keeping when the chain drops to one: the ticket's, else the selection's as
    /// it is, else a position's if none is on the side in view.
    fn pick_traded_side(&mut self) {
        if let Some(t) = &self.ticket {
            self.side = t.key.right;
            return;
        }
        if self.selected.is_some() {
            return;
        }
        let (Some(account), Some(st)) = (self.trade.account.as_ref(), self.trade.try_cache())
        else {
            return;
        };
        let expiry = self.frame.expiry;
        let rights = st
            .positions_of(account)
            .filter(|p| p.contract.product == self.product.id && p.qty != 0)
            .filter_map(|p| p.contract.option_key())
            .filter(|k| Some(k.expiry) == expiry)
            .map(|k| k.right);
        let mut first = None;
        for r in rights {
            if r == self.side {
                return;
            }
            first = first.or(Some(r));
        }
        drop(st);
        if let Some(r) = first {
            self.side = r;
        }
    }

    /// P&L to close is showing. It has the chart's scale to stand on, and money to show.
    fn pnl_shown(&self) -> bool {
        self.pnl_on && self.chart_shown() && self.show_money
    }

    /// The other side, with only one in view: every cell changes under a still pointer.
    fn flip_side(&mut self, cx: &mut Context<Self>) {
        self.leave_ticket();
        self.ticket = None;
        self.side = crate::chain_pnl::other_side(self.side);
        self.rows_moved();
        cx.notify();
    }

    /// The series P&L to close is about: the ticket's, the selection's, the position or line on
    /// the shown side, else the money. What it starts from: the position's average, the ticket's
    /// price, or the ask. The summary is about a position with exits: this one if it is, else the
    /// first. Busy this instant: last frame's, as the rest of the pane does.
    fn refresh_traded(&mut self, cx: &App) {
        self.show_money = crate::picker_view::settings(cx).pnl_display.money();
        // Before the store's lock, which this takes too and which isn't reentrant.
        let selected = self.selected_key();
        let size = self.size(cx);
        let now = Trading::try_global(cx).and_then(|t| t.now(&self.store));
        let (Some(st), Some(d)) = (self.trade.try_cache(), self.store.try_read()) else {
            return;
        };
        let expiry = self.shown_expiry(&d.chains);
        let on_side = |c: ContractKey| {
            c.option_key()
                .filter(|k| k.right == self.side && Some(k.expiry) == expiry)
        };
        let account = self.trade.account.as_ref();
        let held = account.and_then(|a| {
            st.positions_of(a)
                .filter(|p| p.contract.product == self.product.id && p.qty != 0)
                .find_map(|p| on_side(p.contract))
        });
        let lined = self.lines.iter().find_map(|l| on_side(l.contract));
        let money = expiry.and_then(|e| {
            let chain = d.chains.chain(e)?;
            let row = chain.rows.get(chain.atm(d.last_trade?)?)?;
            Some(OptionKey {
                expiry: e,
                strike: row.strike,
                right: self.side,
            })
        });
        let build = |key: OptionKey| {
            let contract = self.contract(key);
            let quote = d
                .chains
                .chain(key.expiry)
                .and_then(|c| c.find(key.strike).ok().map(|i| c.rows[i]))
                .and_then(|r| match key.right {
                    Right::Call => r.call,
                    Right::Put => r.put,
                });
            let position = account.and_then(|a| st.position(a, contract));
            let ticket = self.ticket.as_ref().filter(|t| t.key == key);
            // A position starts from its own average or nothing: no quote passes for what it
            // cost. A what-if starts from the ticket's price, else the side it would trade.
            let sold = ticket.is_some_and(|t| t.side == Side::Sell);
            let (bid, ask) = ticket::sides(quote.as_ref());
            let basis = match position.filter(|p| p.qty != 0) {
                Some(p) => p.avg_price,
                None => ticket
                    .and_then(|t| t.price)
                    .or(if sold { bid } else { ask })
                    .map(|p| p.0 as f64),
            };
            // No money to read it in: no estimate rather than +$0 everywhere.
            let per_unit = ticket::premium(&self.product, contract, 1, Price(1))
                .map(|m| m.0 as f64 / lines_trading::Money::UNITS_PER_WHOLE as f64);
            let model = per_unit.and_then(|_| {
                crate::option_estimate::Model::new(contract, quote.as_ref(), d.last_trade, now)
            });
            Traded {
                contract,
                model,
                position: position.map_or(0, |p| p.qty),
                qty: if sold { -size } else { size },
                basis,
                per_unit: per_unit.unwrap_or(0.0),
            }
        };
        let key = self
            .ticket
            .as_ref()
            .map(|t| t.key)
            .or(selected)
            .or(held)
            .or(lined)
            .or(money);
        let traded = key.map(&build);
        let has_exit = |c: ContractKey| {
            self.lines
                .iter()
                .any(|l| l.contract == c && l.kind.is_exit())
        };
        let summary = traded
            .filter(|t| t.position != 0 && has_exit(t.contract))
            .or_else(|| {
                self.lines
                    .iter()
                    .filter(|l| l.kind.is_exit())
                    .filter_map(|l| l.contract.option_key())
                    .map(&build)
                    .find(|t| t.position != 0)
            });
        drop(d);
        drop(st);
        self.traded = traded;
        self.summary = summary;
    }

    /// No chart beside the chain: what needs a line can't stay.
    fn end_line_state(&mut self) {
        self.line_drag = None;
        self.drafts.clear();
        let rules = self.rules();
        let q = self.ticket.as_ref().and_then(|t| self.live_quote(t.key));
        if let (Some(t), Some(rules)) = (self.ticket.as_mut(), rules)
            && t.when.is_some()
        {
            t.set_when(None, q.as_ref(), rules);
        }
    }

    /// A new look for the chart beside the chain.
    pub fn set_chart_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        if let Some(chart) = &self.chart {
            chart.update(cx, |c, cx| {
                c.set_theme(theme);
                cx.notify();
            });
        }
    }

    /// Strikes around the money set the chart's smallest range. In `render`, before the chart
    /// paints, so its fit this frame has it (a view notified later in a draw isn't redrawn). A
    /// new range waits while the scale is held: the fit does.
    fn sync_chart_span(&mut self, cx: &mut Context<Self>) {
        let Some(chart) = self.chart.clone().filter(|_| self.chart_shown()) else {
            return;
        };
        let Some(d) = self.store.try_read() else {
            return;
        };
        let Some(rows) = self.shown_expiry(&d.chains).and_then(|e| d.chains.chain(e)) else {
            return;
        };
        let Some(i) = d.last_trade.and_then(|l| rows.atm(l)) else {
            return;
        };
        let (a, b) = (
            i.saturating_sub(1),
            (i + 1).min(rows.rows.len().saturating_sub(1)),
        );
        if b > a {
            let step = (rows.rows[b].strike.0 - rows.rows[a].strike.0) as f64 / (b - a) as f64;
            let first = chart.update(cx, |c, cx| {
                let first = c.no_min_price_span();
                if c.set_min_price_span(step * CHART_STRIKES) {
                    cx.notify();
                }
                first
            });
            drop(d);
            // This one refits even under a hold, and every row moves.
            if first {
                self.rows_moved();
            }
        }
    }

    /// The chart polls only while it is on screen: a hidden pane does no work.
    pub fn sync_chart_visible(&mut self, cx: &mut Context<Self>) {
        let visible = self.common.visible && self.chart_shown();
        if let Some(chart) = &self.chart {
            chart.update(cx, |c, _| c.common().visible = visible);
        }
    }

    /// Every row just moved without the trader aiming at the new place: the next click must not
    /// send at once. Says so, since nothing else would.
    fn rows_moved(&mut self) {
        // A drag can't be aimed either: where it was is no longer that price.
        self.line_drag = None;
        if std::mem::take(&mut self.one_click) {
            // A message already showing may be a refusal: it stays, and the chip says off.
            self.note
                .get_or_insert_with(|| "One-click is off: the chain moved".to_owned());
            if std::mem::take(&mut self.expiry_pinned) {
                self.expiry = None;
            }
        }
    }

    /// One-click ends if the window has moved or changed size: the pointer can be over another
    /// cell. The size counts because on Linux a top-edge resize or vertical maximize doesn't report
    /// a new origin. A move needn't bring a paint, so a press looks too, before it can send.
    fn check_window_moved(&mut self, window: &Window) {
        let b = window.bounds();
        let at = (
            f(b.origin.x),
            f(b.origin.y),
            f(b.size.width),
            f(b.size.height),
        );
        if crate::chain_lock::table_moved(self.frame.window_at, at) {
            self.rows_moved();
        }
        self.frame.window_at = Some(at);
    }

    /// The One-click switch.
    pub fn toggle_one_click(&mut self, cx: &mut Context<Self>) {
        self.one_click = !self.one_click;
        self.note = note_after_one_click(self.one_click, self.note.take());
        // Its pin ends with it: the pane follows the soonest expiry again.
        if !self.one_click && std::mem::take(&mut self.expiry_pinned) {
            self.expiry = None;
        }
        cx.notify();
    }

    /// The next account: a ticket on the last one doesn't carry over.
    pub fn next_account(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = Trading::try_global(cx) {
            let next = t.next_account(self.trade.account.as_ref());
            self.trade.account = next;
            self.leave_ticket();
            self.one_click = false;
            self.line_drag = None;
            self.drafts.clear();
            self.note = self
                .trade
                .account
                .as_ref()
                .and_then(|a| t.account(a))
                .map(|i| format!("Account: {}", i.name));
            self.seen_revision = u64::MAX;
            cx.notify();
        }
    }

    /// Close the ticket; an order on its way still gets its answer shown (in the footer).
    fn leave_ticket(&mut self) {
        if let Some(id) = self.ticket.take().and_then(|t| t.sent) {
            self.remember(id);
        }
        self.sent_at = None;
    }

    /// Wait for a command's answer; the answers kept are finite, so only the newest are.
    fn remember(&mut self, id: CommandId) {
        if self.asked.len() >= MAX_ASKED {
            self.asked.remove(0);
        }
        self.asked.push(id);
    }

    /// The market or trading state is mid-update this instant: say so rather than act on a guess.
    fn busy(&mut self, cx: &App) -> bool {
        let trading = Trading::try_global(cx).is_some();
        let busy = self.store.try_read().is_none() || (trading && self.trade.try_cache().is_none());
        if busy {
            self.note = Some("Market data busy; try again".to_owned());
        }
        busy
    }

    /// Contracts of plain working orders on `side` of this series: a close doesn't touch them.
    fn other_working(&self, key: OptionKey, side: Side) -> i64 {
        self.working_on(key, side, false)
    }

    /// Contracts working on `side` of this series, a close's too if asked.
    fn working_on(&self, key: OptionKey, side: Side, closes: bool) -> i64 {
        let Some(a) = self.trade.account.as_ref() else {
            return 0;
        };
        self.trade.try_cache().map_or(0, |st| {
            st.open_orders(a, self.contract(key))
                .filter(|o| o.side == side && (closes || !o.reduce_only))
                .map(|o| o.qty.0 - o.filled.0)
                .sum()
        })
    }

    /// A close is already working on this series.
    fn closing(&self, account: &AccountId, key: OptionKey) -> bool {
        self.trade.try_cache().is_some_and(|st| {
            st.open_orders(account, self.contract(key))
                .any(|o| o.reduce_only)
        })
    }

    fn rules(&self) -> Option<lines_core::OptionRules> {
        self.product.options()
    }

    fn contract(&self, key: OptionKey) -> ContractKey {
        ContractKey::option(self.product.id, key.expiry, key.strike, key.right)
    }

    /// A series' quote in the store right now.
    fn quote(&self, key: OptionKey) -> Option<OptionQuote> {
        let d = self.store.try_read()?;
        let chain = d.chains.chain(key.expiry)?;
        let row = chain.rows.iter().find(|r| r.strike == key.strike)?;
        match key.right {
            Right::Call => row.call,
            Right::Put => row.put,
        }
    }

    /// The option order size of this underlying: shared by its chain panes, not its shares.
    fn size(&self, cx: &App) -> i64 {
        let s = crate::picker_view::settings(cx);
        crate::dom_trade::order_size(
            &s.order_sizes,
            &ticket::size_key(self.product.symbol),
            s.new_panes.default_qty,
        )
    }

    fn set_size(&mut self, q: i64, cx: &mut Context<Self>) {
        // An unavailable pane's product may be a stand-in: never change a real symbol's size.
        if pane::unavailable_title(&self.feed).is_some() {
            return;
        }
        let key = ticket::size_key(self.product.symbol);
        crate::picker_view::update_settings(cx, |s| {
            s.order_sizes.insert(key, q);
        });
        trade_bar::size_changed();
        if let Some(t) = self.ticket.as_mut() {
            t.refused = None;
        }
        cx.notify();
    }

    /// The position on a series.
    fn position(&self, key: OptionKey) -> i64 {
        let Some(a) = self.trade.account.as_ref() else {
            return 0;
        };
        self.trade
            .try_cache()
            .and_then(|st| st.position(a, self.contract(key)).map(|p| p.qty))
            .unwrap_or(0)
    }

    /// Why the core wouldn't take this limit on the pane's account, as far as the UI can tell.
    fn refusal_of(
        &self,
        key: OptionKey,
        side: Side,
        price: Option<Price>,
        close: bool,
        cx: &App,
    ) -> Option<String> {
        let Some(t) = Trading::try_global(cx) else {
            return Some("trading is not running".to_owned());
        };
        let Some(id) = self.trade.account.as_ref() else {
            return Some("no account".to_owned());
        };
        let info = t.account(id);
        // A venue account is never traded here in options: the core's words, before the press.
        let read_only = t.read_only_reason(id).or_else(|| {
            let venue = info.as_ref().filter(|i| !i.paper)?.venue?;
            Some(ticket::paper_only(&t.venue_status(venue)?.0))
        });
        let now = t.now(&self.store);
        let at_start = info.as_ref().is_some_and(|i| i.rules.is_some());
        // Only an order that opens: exits always pass a guardrail.
        // As the core counts it: the position less what's already working to close it.
        let opens = !close
            && ticket::order_opens(
                self.size(cx),
                self.position(key),
                side,
                self.working_on(key, side, true),
            );
        let (prop, locked) = match self.trade.try_cache() {
            Some(st) => (
                ticket::has_rules(&st, id, at_start),
                st.guard(id).filter(|_| opens).and_then(|g| {
                    let money = crate::picker_view::settings(cx).pnl_display.money();
                    trade_bar::guard_line(
                        g.state,
                        g.reason.as_deref().unwrap_or(""),
                        g.until,
                        now,
                        money,
                    )
                }),
            ),
            None => (at_start, None),
        };
        let guard = info
            .as_ref()
            .map(|i| i.option_guard.clone())
            .unwrap_or_default();
        let q = self.quote(key);
        ticket::refusal(
            &ticket::Check {
                product: &self.product,
                contract: self.contract(key),
                guard: &guard,
                quote: q.as_ref(),
                now,
                prop,
                closed: t.check_sessions
                    && now.is_some_and(|n| {
                        !lines_trading::paper::option_session_open(self.product.id, n)
                    }),
                locked: locked.as_deref(),
                read_only: read_only.as_deref(),
            },
            price,
            close.then_some(side),
        )
    }

    fn submit(&self, cmd: lines_trading::Command, cx: &App) -> Option<CommandId> {
        Trading::try_global(cx)
            .map(|t| t.submit(lines_trading::Source::Dom, std::time::Instant::now(), cmd))
    }

    /// The quote, unless it's past its expiry's limit: a stale one prices nothing.
    fn live_quote(&self, key: OptionKey) -> Option<OptionQuote> {
        let q = self.quote(key)?;
        let contract = self.contract(key);
        let stale = self
            .now
            .is_some_and(|n| ticket::quote_stale(&q, contract, n));
        (!stale).then_some(q)
    }

    /// Open the ticket on a cell of a series.
    fn open_ticket(&mut self, key: OptionKey, cell: Cell, cx: &App) {
        let Some(rules) = self.rules() else {
            return;
        };
        self.note = None;
        if self.busy(cx) {
            return;
        }
        self.leave_ticket();
        let q = self.live_quote(key);
        self.ticket = Some(Ticket::new(key, cell.side(), q.as_ref(), rules));
    }

    /// A click on a quote cell with One-click on: a limit at that side, at once.
    fn send_click(&mut self, key: OptionKey, cell: Cell, cx: &mut Context<Self>) {
        self.note = None;
        if self.busy(cx) {
            return;
        }
        {
            self.leave_ticket();
            let q = self.quote(key);
            let price = ticket::click_limit(cell, q.as_ref());
            if let Some(why) = self.refusal_of(key, cell.side(), price, false, cx) {
                self.note = Some(format!("Not sent: {why}"));
                return;
            }
            let Some(account) = self.trade.account.clone() else {
                return;
            };
            let qty = self.size(cx);
            let contract = self.contract(key);
            if let Some(cmd) = ticket::click_order(account, contract, cell, q.as_ref(), qty) {
                self.ask(cmd, cx);
            }
        }
    }

    /// Flatten or Cancel: for every series of the underlying the account has anything on. Only
    /// a read-only account refuses them: never the feed, the quote, One-click or the guard.
    pub fn run_exit(
        &mut self,
        flatten: bool,
        source: lines_trading::Source,
        cx: &mut Context<Self>,
    ) {
        let what = if flatten { "Flatten" } else { "Cancel all" };
        self.note = None;
        let Some(t) = Trading::try_global(cx) else {
            self.note = Some("trading is not running".to_owned());
            cx.notify();
            return;
        };
        let Some(account) = self.trade.account.clone() else {
            self.note = Some("no account".to_owned());
            cx.notify();
            return;
        };
        if let Some(why) = t.read_only_reason(&account) {
            self.note = Some(format!("{what} off: {why}"));
            cx.notify();
            return;
        }
        // The UI never blocks on the trading core: if its state is mid-update this instant, say
        // so and let the press be repeated (the lock is held only for an update).
        // The owner's ruling: Cancel all takes armed triggers and leaves the position. When that
        // takes a position's Stop, the screen says the position has none now.
        let Some((contracts, removed)) = self.trade.try_cache().map(|st| {
            let contracts = ticket::held_contracts(&st, &account, self.product.id);
            let removed =
                crate::chain_lines::stops_removed(st.triggers(), &account, &contracts, |c| {
                    st.position(&account, c).map_or(0, |p| p.qty)
                });
            (contracts, removed)
        }) else {
            self.note = Some(format!("{what}: trading state busy; press again"));
            cx.notify();
            return;
        };
        if contracts.is_empty() {
            self.note = Some(format!(
                "{what}: nothing on {} options",
                self.product.symbol
            ));
        }
        for cmd in ticket::exit_commands(&account, &contracts, flatten) {
            let id = t.submit(source, std::time::Instant::now(), cmd);
            self.remember(id);
        }
        if !flatten && let Some(m) = crate::chain_lines::no_stop_message(&removed) {
            self.note = Some(m);
        }
        self.line_drag = None;
        self.drafts.clear();
        cx.notify();
    }

    /// Flatten and Cancel for the header or footer, while there's anything to act on. Cancel to
    /// the left: it goes first, and Flatten doesn't move when it does.
    fn exit_buttons(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let t = self.theme;
        let account = self.trade.account.as_ref()?;
        let (orders, held) = {
            let st = self.trade.try_cache()?;
            let held = ticket::held_contracts(&st, account, self.product.id);
            let orders = held
                .iter()
                .any(|c| st.open_orders(account, *c).next().is_some())
                || st
                    .triggers()
                    .any(|t| &t.account == account && held.contains(&t.contract));
            (orders, !held.is_empty())
        };
        if !held {
            return None;
        }
        let symbol = self.product.symbol;
        let button = |flatten: bool, cx: &mut Context<Self>| {
            let label = ticket::exit_label(symbol, flatten);
            let cmd = if flatten {
                crate::commands::TradeCmd::Flatten
            } else {
                crate::commands::TradeCmd::CancelAll
            };
            let el = div()
                .id(if flatten {
                    "chain-flatten"
                } else {
                    "chain-cancel-all"
                })
                .flex_shrink_0()
                .cursor_pointer()
                .px_1()
                .h(px(MIN_HIT))
                .flex()
                .items_center()
                .border_1()
                .rounded_sm()
                .border_color(t.text_dim)
                .text_color(t.text_main)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(label.clone())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.run_exit(flatten, lines_trading::Source::Dom, cx);
                    }),
                );
            crate::tooltip::tip_div(el, &label, Some(crate::commands::Command::Trade(cmd)), &t)
        };
        let cancel = orders.then(|| button(false, cx));
        let flatten = button(true, cx);
        Some(
            div()
                .flex()
                .flex_row()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .children(cancel)
                .child(flatten),
        )
    }

    fn ask(&mut self, cmd: lines_trading::Command, cx: &App) {
        if let Some(id) = self.submit(cmd, cx) {
            self.remember(id);
        }
    }

    /// Enter on the ticket: send what the button says, once.
    fn send_ticket(&mut self, cx: &mut Context<Self>) {
        self.note = None;
        if self.busy(cx) {
            return;
        }
        let Some(t) = self.ticket.as_ref() else {
            return;
        };
        if t.sent.is_some() {
            return;
        }
        let (key, side, close, price) = (t.key, t.side, t.close, t.price);
        if t.when.is_some() {
            self.arm_ticket(cx);
            return;
        }
        if self.refusal_of(key, side, price, close, cx).is_some() {
            return; // already on the ticket, in the core's words
        }
        let Some(account) = self.trade.account.clone() else {
            return;
        };
        let built = self.ticket.as_ref().map(|t| {
            ticket::ticket_order(
                t,
                account.clone(),
                self.contract(key),
                self.size(cx),
                self.position(key),
                close && self.closing(&account, key),
            )
        });
        let cmd = match built {
            Some(Ok(cmd)) => cmd,
            Some(Err(why)) => {
                if let Some(t) = self.ticket.as_mut() {
                    t.refused = Some(why);
                }
                return;
            }
            None => return,
        };
        let id = self.submit(cmd, cx);
        self.sent_at = Some(std::time::Instant::now());
        if let Some(t) = self.ticket.as_mut() {
            t.sent = id;
            t.refused = None;
        }
    }

    /// The ticket's "When SPY": arm an entry. The core asks again, and asks every new-order check
    /// when it fires; an order goes out only then.
    fn arm_ticket(&mut self, cx: &mut Context<Self>) {
        if self.when_refusal().is_some() {
            return; // already on the ticket
        }
        let Some(account) = self.trade.account.clone() else {
            return;
        };
        let size = self.size(cx);
        let built = self
            .ticket
            .as_ref()
            .map(|t| ticket::when_trigger(t, account, self.contract(t.key), size));
        match built {
            Some(Ok(cmd)) => {
                let id = self.submit(cmd, cx);
                self.sent_at = Some(std::time::Instant::now());
                if let Some(t) = self.ticket.as_mut() {
                    t.sent = id;
                    t.refused = None;
                }
            }
            Some(Err(why)) => {
                if let Some(t) = self.ticket.as_mut() {
                    t.refused = Some(why);
                }
            }
            None => {}
        }
    }

    /// Why the ticket's level can't arm, in the words a drag would use.
    fn when_refusal(&self) -> Option<String> {
        let t = self.ticket.as_ref()?;
        let level = t.when?;
        let line = self.ticket_line()?;
        if let Some(why) = crate::chain_lines::refusal(&line, level, &self.around(&line)) {
            return Some(why.to_owned());
        }
        if t.price.is_none() {
            return Some("type a max price".to_owned());
        }
        None
    }

    /// The ticket's entry as a line, while it's "When SPY".
    fn ticket_line(&self) -> Option<crate::chain_lines::Line> {
        let t = self.ticket.as_ref()?;
        Some(crate::chain_lines::Line {
            kind: TriggerKind::Entry {
                qty: Qty(1),
                max_price: t.price.unwrap_or(Price(0)),
            },
            contract: self.contract(t.key),
            level: t.when?,
            source: crate::chain_lines::LineSource::Ticket,
            fires: None,
            working: false,
            stale: false,
        })
    }

    /// The ticket to "When SPY" or back.
    fn set_when(&mut self, on: bool, cx: &mut Context<Self>) {
        if self
            .ticket
            .as_ref()
            .is_some_and(|t| t.when.is_some() == on || t.sent.is_some())
        {
            return;
        }
        let (Some(rules), Some(key)) = (self.rules(), self.ticket.as_ref().map(|t| t.key)) else {
            return;
        };
        let q = self.live_quote(key);
        let last = self.store.try_read().and_then(|d| d.last_trade);
        let tick = self.product.price.tick;
        let level = match (on, last) {
            (true, Some(l)) => Some(crate::chain_lines::default_entry_level(key.right, l, tick)),
            (true, None) => {
                self.note = Some("Not yet: no price of the underlying".to_owned());
                cx.notify();
                return;
            }
            (false, _) => None,
        };
        if let Some(t) = self.ticket.as_mut() {
            t.set_when(level, q.as_ref(), rules);
        }
        cx.notify();
    }

    /// What a line's place is checked against: the market, the position and its series' others.
    fn around(&self, line: &crate::chain_lines::Line) -> crate::chain_lines::Around {
        let mut a = crate::chain_lines::Around {
            last: self.store.try_read().and_then(|d| d.last_trade),
            position: line.contract.option_key().map_or(0, |k| self.position(k)),
            ..Default::default()
        };
        for l in self.lines.iter().filter(|l| l.contract == line.contract) {
            if l.same(line) {
                continue;
            }
            match l.kind {
                TriggerKind::Stop => a.stop = Some(l.level),
                TriggerKind::Target => a.target = Some(l.level),
                TriggerKind::Entry { .. } => a.entry = Some(l.level),
            }
        }
        a
    }

    /// The lines as they stand, from the core's triggers, the ticket and the drafts.
    fn refresh_lines(&mut self, cx: &App) {
        use crate::chain_lines::{Line, MAX_LINES};
        // Busy this instant: last frame's lines and rows, as the rest of the pane does.
        let Some(st) = self.trade.try_cache() else {
            return;
        };
        self.lines.clear();
        self.armed_views.clear();
        let money = crate::picker_view::settings(cx).pnl_display.money();
        self.tag_money = None;
        let held = |c: ContractKey| {
            self.trade
                .account
                .as_ref()
                .and_then(|a| st.position(a, c))
                .map_or(0, |p| p.qty)
        };
        // A position gone leaves nothing for a draft to close.
        let kept: Vec<Line> = self
            .drafts
            .iter()
            .copied()
            .filter(|d| held(d.contract) != 0)
            .collect();
        if let Some(account) = self.trade.account.as_ref() {
            let views = st
                .triggers()
                .filter(|v| &v.account == account && v.contract.product == self.product.id);
            for v in views {
                if self.lines.len() < MAX_LINES {
                    self.lines.push(Line::of(v));
                }
                self.armed_views.push(v.clone());
            }
        }
        drop(st);
        self.drafts = kept;
        let drafts = self.drafts.len();
        self.rows_guard
            .see(self.armed_views.iter().map(|v| v.id), drafts);
        // What's being dragged has ended (it fired, say): there's nothing left to move.
        if let Some(d) = self.line_drag
            && matches!(d.line.source, crate::chain_lines::LineSource::Armed(_))
            && !self.lines.iter().any(|l| l.same(&d.line))
        {
            self.line_drag = None;
            self.note = Some(format!(
                "{} is no longer armed: nothing was moved",
                d.line.kind.word()
            ));
        }
        for d in &self.drafts {
            if self.lines.len() < MAX_LINES {
                self.lines.push(*d);
            }
        }
        if let Some(l) = self.ticket_line()
            && self.lines.len() < MAX_LINES
        {
            self.lines.push(l);
        }
        if money && let Some(l) = self.lines.first() {
            self.tag_money = ticket::premium(&self.product, l.contract, 1, Price(1))
                .map(|m| m.0 as f64 / lines_trading::Money::UNITS_PER_WHOLE as f64);
        }
    }

    /// New words about triggers that ended by themselves.
    fn take_ended(&mut self) {
        let (seen, note) = {
            let Some(st) = self.trade.try_cache() else {
                return;
            };
            let count = st.triggers_ended_count();
            let mut note = None;
            if let Some(since) = self.seen_ended
                && let Some(account) = self.trade.account.as_ref()
            {
                for v in st
                    .triggers_ended_since(since)
                    .filter(|v| &v.account == account && v.contract.product == self.product.id)
                {
                    if let Some(m) = crate::chain_lines::ended_message(v) {
                        note = Some(match note {
                            Some(before) => format!("{before}  ·  {m}"),
                            None => m,
                        });
                    }
                }
            }
            (count, note)
        };
        self.seen_ended = Some(seen);
        if note.is_some() {
            self.note = note;
        }
    }

    /// The chart's scale, while the rows are on it.
    fn chart_scale(&self, cx: &App) -> Option<crate::chain_lock::LockedScale> {
        self.chart
            .as_ref()
            .filter(|_| self.chart_shown())
            .and_then(|c| c.read(cx).locked_scale())
    }

    /// Take hold of a line.
    fn grab_line(&mut self, n: usize, press_y: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(line) = self.lines.get(n).copied().filter(|l| l.draggable()) else {
            return;
        };
        // What's on its way to arm is what the core has: it holds until the answer.
        if line.source == crate::chain_lines::LineSource::Ticket
            && self.ticket.as_ref().is_some_and(|t| t.sent.is_some())
        {
            return;
        }
        self.focus.focus(window, cx);
        self.note = None;
        self.line_drag = Some(LineDrag {
            line,
            to: line.level,
            refused: None,
            press_y,
        });
        cx.notify();
    }

    fn on_line_move(&mut self, ev: &gpui::MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.line_drag else {
            return;
        };
        // The button came up where we couldn't see it.
        if ev.pressed_button != Some(MouseButton::Left) {
            self.line_drag = None;
            cx.notify();
            return;
        }
        let drag = self.drag_now(drag);
        let Some(scale) = self.chart_scale(cx) else {
            return;
        };
        let at =
            crate::chain_lines::dragged_y(scale.y(drag.line.level), drag.press_y, f(ev.position.y));
        // Under the threshold it's a click so far: the line stays where it was.
        let (to, refused) = match at {
            None => (drag.line.level, None),
            Some(y) => {
                let y = f64::from(y - scale.plot_top).clamp(0.0, scale.vp.height);
                let to = crate::chain_lines::snap(scale.vp.y_to_price(y), self.product.price.tick);
                let refused = crate::chain_lines::refusal(&drag.line, to, &self.around(&drag.line));
                (to, refused)
            }
        };
        self.line_drag = Some(LineDrag {
            to,
            refused,
            ..drag
        });
        cx.notify();
    }

    /// A release on the plot puts the line there, if it can go there.
    fn on_line_up(&mut self, ev: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.line_drag.take() else {
            return;
        };
        let drag = self.drag_now(drag);
        cx.notify();
        let Some(scale) = self.chart_scale(cx) else {
            return;
        };
        let y = f(ev.position.y);
        if y < scale.plot_top || y > scale.bottom() {
            return; // off the plot: as on the DOM, it aborts
        }
        if drag.to == drag.line.level {
            return;
        }
        // Asked again: what's true at the release, not what was at the last move.
        if let Some(why) =
            crate::chain_lines::refusal(&drag.line, drag.to, &self.around(&drag.line))
        {
            self.note = Some(format!("Not moved: {why}"));
            return;
        }
        use crate::chain_lines::LineSource;
        match drag.line.source {
            LineSource::Ticket => {
                if let Some(t) = self.ticket.as_mut() {
                    t.set_level(drag.to);
                }
            }
            LineSource::Draft => {
                for d in self.drafts.iter_mut().filter(|d| d.same(&drag.line)) {
                    d.level = drag.to;
                }
            }
            LineSource::Armed(id) => {
                let Some(account) = self.trade.account.clone() else {
                    return;
                };
                let contract = drag.line.contract;
                self.ask(
                    crate::chain_lines::move_command(account, contract, id, drag.to),
                    cx,
                );
            }
        }
    }

    /// A drag with its line as the line is now: a feed can go stale while the button is down.
    fn drag_now(&self, mut drag: LineDrag) -> LineDrag {
        if let Some(line) = self.lines.iter().find(|l| l.same(&drag.line)) {
            drag.line = *line;
        }
        drag
    }

    /// A press on the armed rows counts, or says why it didn't.
    fn rows_guard_ok(&mut self, cx: &mut Context<Self>) -> bool {
        if self.rows_guard.may_press() {
            return true;
        }
        self.note = Some("The rows just changed: move onto the one you want and press".to_owned());
        cx.notify();
        false
    }

    /// A release off the pane: nothing happened.
    fn on_line_up_out(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.line_drag.take().is_some() {
            cx.notify();
        }
    }

    /// A new Stop or Target for the series, on its own side of the market, not armed yet.
    fn add_draft(&mut self, kind: TriggerKind, key: OptionKey, cx: &mut Context<Self>) {
        let position = self.position(key);
        let last = self.store.try_read().and_then(|d| d.last_trade);
        let Some(last) = last else {
            self.note = Some("Not yet: no price of the underlying".to_owned());
            cx.notify();
            return;
        };
        if position == 0 {
            return;
        }
        let contract = self.contract(key);
        self.drafts
            .retain(|d| d.contract != contract || d.kind != kind);
        let tick = self.product.price.tick;
        self.drafts.push(crate::chain_lines::Line {
            kind,
            contract,
            level: crate::chain_lines::default_exit_level(kind, key.right, position, last, tick),
            source: crate::chain_lines::LineSource::Draft,
            fires: None,
            working: false,
            stale: false,
        });
        cx.notify();
    }

    /// Arm a draft where it is. The core asks again, and its refusal shows in the footer.
    fn arm_draft(&mut self, n: usize, cx: &mut Context<Self>) {
        let Some(line) = self.drafts.get(n).copied() else {
            return;
        };
        if let Some(why) = crate::chain_lines::refusal(&line, line.level, &self.around(&line)) {
            self.note = Some(format!("Not armed: {why}"));
            cx.notify();
            return;
        }
        let Some(account) = self.trade.account.clone() else {
            return;
        };
        self.note = None;
        self.drafts.remove(n);
        self.ask(crate::chain_lines::arm(account, &line, line.level), cx);
        cx.notify();
    }

    /// A person's Cancel on one armed row. A position's Stop going says what that leaves.
    fn cancel_trigger(&mut self, view: &TriggerView, cx: &mut Context<Self>) {
        let Some(account) = self.trade.account.clone() else {
            return;
        };
        self.note = None;
        if view.kind == TriggerKind::Stop {
            let position = view.contract.option_key().map_or(0, |k| self.position(k));
            if position != 0 {
                self.note = crate::chain_lines::no_stop_message(&[view.contract]);
            }
        }
        self.ask(
            lines_trading::Command::CancelTrigger {
                account,
                contract: view.contract,
                id: view.id,
            },
            cx,
        );
        cx.notify();
    }

    /// The row's Close: reduce-only, the whole position, at the mid.
    fn press_close(&mut self, key: OptionKey, cx: &mut Context<Self>) {
        let (Some(rules), Some(account)) = (self.rules(), self.trade.account.clone()) else {
            return;
        };
        self.note = None;
        if self.busy(cx) {
            return;
        }
        let position = self.position(key);
        // A close already working: another would only sit behind it, so it says so.
        if self.closing(&account, key) {
            self.note = Some("Not sent: a close is already working; Cancel it first".to_owned());
            return;
        }
        let q = self.quote(key);
        let closing = if position > 0 { Side::Sell } else { Side::Buy };
        match ticket::close_command(account, self.contract(key), position, q.as_ref(), rules) {
            Some(cmd) => match self.refusal_of(key, closing, order_price(&cmd), true, cx) {
                Some(why) => self.note = Some(format!("Not sent: {why}")),
                None => {
                    let side = if position > 0 { Side::Sell } else { Side::Buy };
                    let others = self.other_working(key, side);
                    self.ask(cmd, cx);
                    if others > 0 {
                        self.note = Some(format!("Close sent. {}", leaves_working(others)));
                    }
                }
            },
            None if position != 0 => {
                let why = self
                    .refusal_of(key, closing, None, true, cx)
                    .unwrap_or_else(|| "no mid to close at".to_owned());
                self.note = Some(format!("Not sent: {why}"));
            }
            None => {}
        }
    }

    fn press_cancel(&mut self, order: OrderId, cx: &mut Context<Self>) {
        let Some(account) = self.trade.account.clone() else {
            return;
        };
        self.note = None;
        let cmd = lines_trading::Command::CancelOrder { account, order };
        self.ask(cmd, cx);
    }

    /// The core's answers to what this pane sent. Returns whether anything changed.
    fn take_answers(&mut self) -> bool {
        let sent = self.ticket.as_ref().and_then(|t| t.sent);
        if sent.is_none() && self.asked.is_empty() && self.watching.is_empty() {
            return false;
        }
        let Some(st) = self.trade.try_cache() else {
            return false;
        };
        let answer = sent.and_then(|id| st.result(id).cloned());
        let mut note = None;
        let mut accepted: Vec<OrderId> = Vec::new();
        let asked = &mut self.asked;
        let before = asked.len();
        asked.retain(|id| match st.result(*id) {
            Some(Err(e)) => {
                note = Some(format!("Refused: {}", e.message));
                false
            }
            Some(Ok(ids)) => {
                accepted.extend_from_slice(ids);
                false
            }
            None => true,
        });
        let mut changed = asked.len() != before;
        if let Some(Ok(ids)) = &answer {
            accepted.extend_from_slice(ids);
        }
        self.watching.extend(accepted);
        let excess = self.watching.len().saturating_sub(MAX_ASKED);
        self.watching.drain(..excess);
        // Accepted isn't filled: the broker can still reject an order on arrival, and a Flatten
        // has no fill on a stale quote or a missing side, so it says the position stayed.
        self.watching.retain(|id| {
            use lines_trading::{OrderState, Role};
            let Some(o) = st.order(*id) else {
                return true;
            };
            let flatten = o.role == Role::Flatten;
            match &o.state {
                OrderState::Rejected { reason } => {
                    note = Some(format!("Rejected: {reason}"));
                    false
                }
                OrderState::Cancelled if flatten => {
                    // Its exits are still working: only Cancel All takes those.
                    note = Some(
                        "Flatten found no usable quote on its side: the position is still open"
                            .to_owned(),
                    );
                    false
                }
                OrderState::Submitted => true,
                s => flatten && !s.is_terminal(),
            }
        });
        drop(st);
        if let Some(n) = note {
            self.note = Some(n);
            changed = true;
        }
        match answer {
            Some(Ok(_)) => {
                self.ticket = None;
                self.sent_at = None;
                changed = true;
            }
            Some(Err(e)) => {
                if let Some(t) = self.ticket.as_mut() {
                    t.sent = None;
                    t.refused = Some(e.message);
                }
                self.sent_at = None;
                changed = true;
            }
            // Still on its way, however long: forgetting it would let Enter send it twice. The
            // ticket says so after a while, and Esc leaves it with the answer still expected.
            None => {
                let late = self.sent_at.is_some_and(|at| at.elapsed() >= ANSWER_WAIT);
                if late && let Some(t) = self.ticket.as_mut() {
                    let waiting = "no answer yet from the trading core; Esc leaves it on its way";
                    if t.refused.as_deref() != Some(waiting) {
                        t.refused = Some(waiting.to_owned());
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Keys while the ticket is open: `true` when one was taken.
    fn ticket_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(rules) = self.rules() else {
            return false;
        };
        let k = &ev.keystroke;
        let shift = k.modifiers.shift;
        // What's on its way to arm is what the core has: the ticket holds until the answer.
        if self
            .ticket
            .as_ref()
            .is_some_and(|t| t.when.is_some() && t.sent.is_some())
            && k.key != "escape"
        {
            return true;
        }
        match k.key.as_str() {
            "escape" => self.leave_ticket(),
            "enter" if self.ticket.as_ref().is_some_and(|t| t.sent.is_some()) => {}
            // A held key never sends an order.
            "enter" if ev.is_held => {}
            "enter" => self.send_ticket(cx),
            "up" | "down" if shift => {
                let closing = self.ticket.as_ref().is_some_and(|t| t.close);
                if !closing {
                    let q = crate::dom_trade::step_qty(self.size(cx), k.key == "up");
                    self.set_size(q, cx);
                }
            }
            "up" | "down" => {
                let tick = self.product.price.tick;
                if let Some(t) = self.ticket.as_mut() {
                    if t.when.is_some() && t.on_level {
                        t.step_level(k.key == "up", 5 * tick.max(1));
                    } else {
                        t.step(k.key == "up", rules);
                    }
                }
            }
            "tab" if self.ticket.as_ref().is_some_and(|t| t.when.is_some()) => {
                if let Some(t) = self.ticket.as_mut() {
                    t.on_level = !t.on_level;
                }
            }
            "tab" => self.toggle_close(cx),
            "backspace" => {
                let product = self.product.clone();
                if let Some(t) = self.ticket.as_mut() {
                    t.backspace(&product);
                }
            }
            _ => {
                let c = k.key_char.as_deref().and_then(|c| c.chars().next());
                let product = self.product.clone();
                let took = match (c, self.ticket.as_mut()) {
                    (Some(c), Some(t)) => t.type_char(c, &product),
                    _ => false,
                };
                if !took {
                    return false;
                }
            }
        }
        true
    }

    /// The ticket closes the position instead (when its side is against one), or back.
    fn toggle_close(&mut self, cx: &mut Context<Self>) {
        let Some(rules) = self.rules() else {
            return;
        };
        let Some((key, side, close)) = self.ticket.as_ref().map(|t| (t.key, t.side, t.close))
        else {
            return;
        };
        if !close && ticket::closes(self.position(key), side).is_none() {
            return;
        }
        let q = self.live_quote(key);
        if let Some(t) = self.ticket.as_mut() {
            t.set_close(!close, q.as_ref(), rules);
        }
        cx.notify();
    }

    /// The screenshot fixture, once the chain has data.
    fn apply_shot(&mut self, cx: &mut Context<Self>) {
        let Some(shot) = self.shot.clone() else {
            return;
        };
        self.shot_restored = shot.restored;
        self.pnl_on |= shot.pnl;
        if let Some((kind, level)) = shot.drag.clone()
            && let Ok(units) = self.product.price.scale.parse_decimal(&level)
        {
            self.shot_drag = Some((kind, Price(units)));
        }
        let expiry = match self.store.try_read() {
            Some(d) => d.chains.expiries().next(),
            None => return,
        };
        let Some(expiry) = expiry else {
            return;
        };
        self.one_click = shot.one_click;
        let Some((cell, strike, right)) = shot.open.clone() else {
            self.shot = None;
            return;
        };
        let Ok(units) = self.product.price.scale.parse_decimal(&strike) else {
            return;
        };
        let key = OptionKey {
            expiry,
            strike: Price(units),
            right,
        };
        // A Close needs the scripted position, which arrives after the first frames.
        if (shot.close || shot.draft.is_some()) && self.position(key) == 0 {
            return;
        }
        self.shot = None;
        self.selected = Some(key.strike);
        self.side = right;
        self.cell = cell;
        self.open_ticket(key, cell, cx);
        if shot.close {
            self.toggle_close(cx);
        }
        if let Some(at) = shot.at
            && let Ok(units) = self.product.price.scale.parse_decimal(&at)
            && let Some(t) = self.ticket.as_mut()
        {
            t.set_price(Price(units));
        }
        if let Some(when) = shot.when
            && let Ok(units) = self.product.price.scale.parse_decimal(&when)
        {
            self.set_when(true, cx);
            if let Some(t) = self.ticket.as_mut() {
                t.set_level(Price(units));
            }
        }
        if let Some((kind, level)) = shot.draft
            && let Ok(units) = self.product.price.scale.parse_decimal(&level)
        {
            let kind = if kind == "stop" {
                TriggerKind::Stop
            } else {
                TriggerKind::Target
            };
            self.add_draft(kind, key, cx);
            for d in self.drafts.iter_mut().filter(|d| d.kind == kind) {
                d.level = Price(units);
            }
            // The fixture shows the draft, not the ticket that named the series.
            self.ticket = None;
        }
    }

    /// The screenshot fixture for a line being dragged, once that line exists.
    fn apply_shot_drag(&mut self) {
        let Some((kind, to)) = self.shot_drag.clone() else {
            return;
        };
        let found = self
            .lines
            .iter()
            .copied()
            .find(|l| l.kind.word().eq_ignore_ascii_case(&kind));
        // Kept: the first frames' layout counts as the plot moving, which ends a drag.
        if let Some(line) = found {
            let refused = crate::chain_lines::refusal(&line, to, &self.around(&line));
            self.line_drag = Some(LineDrag {
                line,
                to,
                refused,
                press_y: 0.0,
            });
        }
    }

    /// Move the selection by `delta` strikes (from the money if nothing is selected yet).
    fn move_strike(&mut self, delta: i64) {
        // One-click holds the rows: keys move only among the strikes on screen, never onto one
        // the hold keeps out or below them. On the chart's scale that is always so: the rows are
        // where the chart's prices put them.
        if self.frame.holding || self.frame.locked {
            if let Some(s) = self.frame.step_held(self.selected, delta) {
                self.selected = Some(s);
            }
            return;
        }
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
            drop(d);
            self.expiry = Some(all[j]);
            self.expiry_pinned = false;
            self.selected = None; // that strike may not exist there: back to the money
            self.frame.release();
            self.leave_ticket();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.picker.is_open() && pane::unavailable_key(self, ev, cx) {
            return;
        }
        let k = &ev.keystroke;
        let m = k.modifiers;
        // Esc aborts a line's drag, as an order's on the DOM. Plain keys do nothing during one;
        // Flatten and Cancel All go on past it, as they always do.
        if self.line_drag.is_some() {
            if m.control || m.alt || m.platform {
                return;
            }
            if k.key == "escape" {
                self.line_drag = None;
            }
            cx.stop_propagation();
            cx.notify();
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
        if m.control && !m.alt && k.key == "k" {
            self.picker.open("", cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.control || m.alt || m.platform {
            return;
        }
        if self.ticket.is_some() {
            if self.ticket_key(ev, cx) {
                cx.stop_propagation();
                cx.notify();
                return;
            }
            // Keys the ticket doesn't use do nothing behind it, but a letter still opens the
            // symbol picker, as in every pane.
            if let Some(letter) = k
                .key_char
                .as_deref()
                .filter(|c| c.len() == 1 && c.as_bytes()[0].is_ascii_alphabetic())
            {
                self.picker.open(letter, cx);
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        self.note = None;
        match k.key.as_str() {
            // With One-click on the size shows on its switch; otherwise it shows on the ticket.
            "up" | "down" if m.shift && self.one_click => {
                let q = crate::dom_trade::step_qty(self.size(cx), k.key == "up");
                self.set_size(q, cx);
            }
            "up" => self.move_strike(-1),
            "down" => self.move_strike(1),
            "pageup" => self.move_strike(-(self.page.max(1) as i64)),
            "pagedown" => self.move_strike(self.page.max(1) as i64),
            "home" => {
                self.selected = None;
                self.frame.release();
            }
            "left" => self.step_expiry(-1),
            "right" => self.step_expiry(1),
            "tab" if m.shift => self.cell = self.cell.other(),
            "tab" if self.pnl_shown() => self.flip_side(cx),
            "tab" => {
                self.side = match self.side {
                    Right::Call => Right::Put,
                    Right::Put => Right::Call,
                }
            }
            // A held key never opens or sends: one press, one ticket.
            "enter" if ev.is_held => {}
            "enter" => match self.selected_key() {
                Some(key) => self.open_ticket(key, self.cell, cx),
                // Nothing selected yet: the first press selects the money.
                None => self.move_strike(0),
            },
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
        // The click that only brings the pane forward aims at nothing: One-click waits for the
        // next. As of the last frame: the workspace has already focused the pane by now.
        let focused = self.aimed;
        self.focus.focus(window, cx);
        // A click that closes the picker does only that: never a press on what's under it.
        if self.picker.close() {
            cx.notify();
            return;
        }
        // Nothing is drawn there: an unavailable pane acts on nothing.
        if pane::unavailable_title(&self.feed).is_some() {
            return;
        }
        let armed = self.one_click;
        self.check_window_moved(window);
        if armed && !self.one_click {
            cx.notify();
        }
        let x = f(ev.position.x);
        let y = f(ev.position.y - self.bounds.origin.y);
        if y < HEAD_H {
            return;
        }
        if self.frame.locked
            && let Some(hit) = self.frame.marker_at(x, f(ev.position.y))
        {
            self.bring_into_view(hit, cx);
            return;
        }
        // A line, between rows: a row always keeps its press.
        if self.frame.locked && self.frame.locked_at(f(ev.position.y)).is_none() {
            let py = f(ev.position.y);
            let near = self
                .line_ys
                .iter()
                .take(self.lines.len())
                .position(|y| (y - py).abs() <= LINE_GRAB);
            if let Some(n) = near {
                self.grab_line(n, py, window, cx);
                return;
            }
        }
        // The row as painted: a series arriving since can't shift the press onto another strike.
        let strike = if self.frame.locked {
            self.frame.locked_at(f(ev.position.y))
        } else {
            let i = ((y - HEAD_H) / ROW_H).floor() as usize;
            (i < self.layout.rows)
                .then(|| row_at(i, self.layout.first_row, self.layout.line_at))
                .flatten()
                .and_then(|ri| ri.checked_sub(self.frame.first_row))
                .and_then(|n| self.frame.rows.get(n))
                .map(|r| r.strike)
        };
        let Some(strike) = strike else {
            return;
        };
        let Some(expiry) = self.frame.expiry else {
            return;
        };
        let right = if x < self.layout.strike_x.0 {
            Some(Right::Call)
        } else if x > self.layout.strike_x.1 {
            Some(Right::Put)
        } else {
            None
        };
        // The margin where a hidden side would be is nothing to press.
        if right.is_some() && right == self.frame.hidden_side {
            return;
        }
        self.selected = Some(strike);
        if let Some(r) = right {
            self.side = r;
        }
        if let Some(right) = right {
            let key = OptionKey {
                expiry,
                strike,
                right,
            };
            let (quotes, side) = match right {
                Right::Call => (self.layout.calls, 0),
                Right::Put => (self.layout.puts, 1),
            };
            if let Some(cell) = quotes.at(x) {
                self.cell = cell;
                if ticket::click_sends(self.one_click, focused, ev.click_count) {
                    self.send_click(key, cell, cx);
                } else if !self.one_click && ev.click_count <= 1 {
                    self.open_ticket(key, cell, cx);
                } else if !focused {
                    self.note = Some("Pane focused: click again to send".to_owned());
                }
            } else if let Some(a) = self.layout.actions.map(|a| a[side])
                && ev.click_count <= 1
                && x >= a.0
                && x < a.1
                && let Some(m) = self.frame.mark(strike, right).copied()
            {
                if m.position != 0 {
                    self.press_close(key, cx);
                } else if let Some((id, ..)) = m.order {
                    self.press_cancel(id, cx);
                }
            }
        }
        cx.notify();
    }

    /// The selected strike and side as a series on the shown expiry.
    fn selected_key(&self) -> Option<OptionKey> {
        let strike = self.selected?;
        let d = self.store.try_read()?;
        Some(OptionKey {
            expiry: self.shown_expiry(&d.chains)?,
            strike,
            right: self.side,
        })
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        self.frame.markers = [None, None];
        let t = self.theme;
        self.now = Trading::try_global(cx).and_then(|tr| tr.now(&self.store));
        window.paint_quad(fill(bounds, t.bg));
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        if w < 1.0 || h < 1.0 {
            return; // not laid out yet (a first frame can be empty and far off)
        }
        let hair = 1.0 / window.scale_factor();
        let fit = (((h - HEAD_H) / ROW_H).floor().max(1.0)) as usize;
        self.page = fit.saturating_sub(1).max(1);
        // On the chart's scale, when the chart beside it has one.
        let scale = if self.chart_shown() {
            self.chart.as_ref().and_then(|c| c.read(cx).locked_scale())
        } else {
            None
        };
        let armed = self.one_click;
        self.check_window_moved(window);
        if armed && !self.one_click {
            // (The header is already drawn.)
            window.on_next_frame(|window, _| window.refresh());
        }
        // The columns are laid out from the table's own bounds, chart or no chart.
        let table_at = (ox, top, w);
        if crate::chain_lock::table_moved(self.frame.table_at, table_at) && self.one_click {
            self.rows_moved();
            window.on_next_frame(|window, _| window.refresh());
        }
        self.frame.table_at = Some(table_at);
        let geometry = scale.as_ref().map(|s| s.geometry());
        if crate::chain_lock::rows_jumped(self.frame.locked_geometry, geometry) {
            self.frame.locked_geometry = geometry;
            self.frame.release();
            self.frame.locked_key = None;
            // Where a drag holds is no longer that price.
            self.line_drag = None;
            // A first fit, a reset that found no bars, or a new height. (The header is already
            // drawn.)
            if self.one_click {
                self.rows_moved();
                window.on_next_frame(|window, _| window.refresh());
            }
        }
        self.frame.locked = scale.is_some();
        // Copy what's in view and let go of the store; busy this instant: last frame's rows.
        match self.store.try_read() {
            Some(d) => {
                // One-click pins the expiry it shows: a sooner one arriving replaces no rows.
                if self.one_click && self.expiry.is_none() {
                    self.expiry = self.shown_expiry(&d.chains);
                    self.expiry_pinned = true;
                }
                let expiry = self.shown_expiry(&d.chains);
                // Another expiry is other rows (a sooner one can arrive while following it).
                if expiry != self.frame.expiry {
                    self.frame.release();
                }
                self.frame.expiry = expiry;
                let chain = self.frame.expiry.and_then(|e| d.chains.chain(e));
                // Before the rows: the strikes held or worked keep theirs.
                match (self.trade.account.as_ref(), self.trade.try_cache()) {
                    (Some(a), Some(st)) => self.frame.refresh_marks(&st, a, self.product.id),
                    // Busy this instant: last frame's marks, and another look next frame.
                    (Some(_), None) if Trading::try_global(cx).is_some() => {
                        self.seen_revision = u64::MAX;
                    }
                    _ => self.frame.marks.clear(),
                }
                match &scale {
                    Some(s) => {
                        let hold = hold_rows(
                            bounds,
                            window.mouse_position(),
                            window.is_window_hovered(),
                            self.one_click,
                        ) || self.line_drag.is_some();
                        self.frame.hold = hold;
                        if let Some(chart) = &self.chart {
                            chart.update(cx, |c, _| c.hold_scale(hold));
                        }
                        let band = top + HEAD_H + MARKER_H;
                        let table_bottom = f(bounds.origin.y + bounds.size.height);
                        let bottom = s.bottom().min(table_bottom) - MARKER_H;
                        self.frame.area = (band, bottom);
                        self.frame.refresh_locked(
                            chain,
                            d.last_trade,
                            self.selected,
                            s,
                            band,
                            bottom,
                        );
                    }
                    None => {
                        self.frame.hold = self.one_click;
                        self.frame.refresh(chain, d.last_trade, self.selected, fit);
                    }
                }
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
        // The Pos column: fixed, so the columns never shift under the pointer when an order
        // lands. It takes the room of the columns that only inform, but a pane too narrow for
        // it and the quotes leaves it out (the strike is marked, the selected row lists them).
        let action_w = self.glyphs.width("Cancel", t.text_main, false, window) + PAD;
        let pos_full =
            self.glyphs.width(POS_SAMPLE, t.text_main, false, window) + action_w + 2.0 * PAD;
        // P&L to close keeps only the traded side.
        let hidden = self
            .pnl_shown()
            .then(|| crate::chain_pnl::other_side(self.side));
        self.frame.hidden_side = hidden;
        let (show_calls, show_puts) = (hidden != Some(Right::Call), hidden != Some(Right::Put));
        let sides = if hidden.is_some() { 1.0 } else { 2.0 };
        let quotes_w = strike_w + sides * (cols[5] + cols[6]);
        let pos_w = if w >= quotes_w + sides * pos_full {
            pos_full
        } else if w >= quotes_w + sides * MARK_W {
            MARK_W
        } else {
            0.0
        };
        let compact = pos_w > 0.0 && pos_w < pos_full;
        // `fit_columns` counts two sides: one side has twice the room for its columns.
        let room = w - sides * pos_w;
        let shown = fit_columns(strike_w + (room - strike_w) * 2.0 / sides, strike_w, cols);
        let table_w = strike_w
            + sides
                * (pos_w
                    + cols
                        .iter()
                        .zip(&shown)
                        .filter(|(_, s)| **s)
                        .map(|(c, _)| c)
                        .sum::<f32>());
        let x0 = ox + ((w - table_w) / 2.0).max(0.0);
        // Calls: left to right in CALLS order; strike; puts: CALLS reversed.
        let mut call_x = [0.0f32; 7];
        let mut x = x0;
        if show_calls {
            x += pos_w;
            for i in 0..7 {
                if shown[i] {
                    call_x[i] = x;
                    x += cols[i];
                }
            }
        }
        let strike_x = x;
        x += strike_w;
        // Puts read outward from the strike: bid, ask, last, delta, IV, volume, OI.
        let mut put_x = [0.0f32; 7];
        if show_puts {
            for i in [5, 6, 4, 3, 2, 1, 0] {
                if shown[i] {
                    put_x[i] = x;
                    x += cols[i];
                }
            }
        }
        let put_pos_x = x;
        if show_puts {
            x += pos_w;
        }
        let calls_end = strike_x;
        let puts_start = strike_x + strike_w;
        // Header rows: CALLS / PUTS, then the titles.
        window.paint_quad(fill(
            Bounds::new(point(px(ox), px(top)), size(px(w), px(HEAD_H))),
            t.panel,
        ));
        // A narrow header has no room for the expiry: it stays in view here.
        if w < NARROW_HEADER
            && let Some(e) = self.frame.expiry
        {
            let mut date = FixedStr::<24>::new();
            let _ = write_chip_date(&mut date, e);
            self.glyphs.paint(
                date.as_str(),
                ox + PAD,
                top,
                ROW_H,
                t.text_main,
                false,
                window,
                cx,
            );
        }
        if show_calls {
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
        }
        if show_puts {
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
        }
        let title_y = top + ROW_H;
        for i in 0..7 {
            if !shown[i] {
                continue;
            }
            let c = Col::CALLS[i];
            for (on, cx0) in [(show_calls, call_x[i]), (show_puts, put_x[i])] {
                if !on {
                    continue;
                }
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
        if !compact && pos_w > 0.0 {
            if show_calls {
                self.glyphs.paint(
                    "Pos / Orders",
                    x0 + PAD,
                    title_y,
                    ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            if show_puts {
                self.glyphs.paint(
                    "Pos / Orders",
                    put_pos_x + PAD,
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
        // No quote in view is fresh: a word in the header, on the next render.
        let newest = self
            .frame
            .rows
            .iter()
            .flat_map(|r| [r.call, r.put])
            .flatten()
            .map(|q| q.time.0)
            .max();
        // The limit on what's in view, worked out once a frame.
        let stale_at = self.now.zip(self.frame.expiry).map(|(n, e)| {
            (
                n,
                lines_trading::risk::expiry_quote_stale_ms(self.product.id, e, n),
            )
        });
        let quotes_stale = stale_at
            .zip(newest)
            .is_some_and(|((n, limit), q)| n.0 - q > limit);
        if quotes_stale != self.quotes_stale {
            self.quotes_stale = quotes_stale;
            self.seen_revision = u64::MAX;
        }
        // Rows: around the selection, else the money (copied in `frame`).
        let line_at = self.frame.line_at;
        let quotes = |at: &[f32; 7]| Quotes {
            bid: (at[5], at[5] + cols[5]),
            ask: (at[6], at[6] + cols[6]),
        };
        let action_at = |cell_x: f32| {
            let end = cell_x + pos_w - PAD / 2.0;
            (end - action_w, end)
        };
        self.layout = Layout {
            strike_x: (calls_end, puts_start),
            calls: quotes(&call_x),
            puts: quotes(&put_x),
            // As large as the row allows: its height, and out to the cell's edge.
            actions: (pos_w > 0.0 && !compact)
                .then(|| [x0, put_pos_x].map(|cell_x| (action_at(cell_x).0 - 2.0, cell_x + pos_w))),
            first_row: self.frame.first_row,
            rows: self.frame.rows.len() + usize::from(line_at.is_some()),
            line_at,
        };
        let mut s = FixedStr::<24>::new();
        // A line runs across the chain and never covers a cell.
        if let Some(s) = &scale {
            self.paint_chain_lines(s, bounds, (x0, x), window);
        }
        for n in 0..self.frame.rows.len() {
            let row = self.frame.rows[n];
            let row = &row;
            let y = match self.frame.ys.get(n).filter(|_| self.frame.locked) {
                Some(row_top) => *row_top,
                None => top + HEAD_H + visual_row(n, line_at) as f32 * ROW_H,
            };
            // In the money: calls below the underlying's price, puts above it.
            let (call_itm, put_itm) = match last {
                Some(l) => (row.strike < l, row.strike > l),
                None => (false, false),
            };
            if call_itm && show_calls {
                window.paint_quad(fill(
                    Bounds::new(point(px(x0), px(y)), size(px(calls_end - x0), px(ROW_H))),
                    t.grid,
                ));
            }
            if put_itm && show_puts {
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
                for (on, side_x, q) in [
                    (show_calls, call_x[i], row.call.as_ref()),
                    (show_puts, put_x[i], row.put.as_ref()),
                ] {
                    if !on {
                        continue;
                    }
                    cell(&mut s, &self.product, q, c);
                    // A stale quote doesn't read as live (the header says so in a word).
                    let stale = q
                        .is_some_and(|q| stale_at.is_some_and(|(n, limit)| n.0 - q.time.0 > limit));
                    self.glyphs.paint_right(
                        s.as_str(),
                        side_x + cols[i] - PAD,
                        y,
                        ROW_H,
                        if stale { t.text_dim } else { t.text_main },
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
            // What the account has here: in the Pos column, a short mark in a narrow pane's, and
            // with no room for even that a dot beside the strike.
            for (right, cell_x, dot_x) in [
                (Right::Call, x0, strike_x + 1.0),
                (Right::Put, put_pos_x, strike_x + strike_w - PAD + 1.0),
            ] {
                if hidden == Some(right) {
                    continue;
                }
                let Some(m) = self.frame.mark(row.strike, right).copied() else {
                    continue;
                };
                if pos_w <= 0.0 {
                    self.glyphs
                        .paint("•", dot_x, y, ROW_H, t.text_main, true, window, cx);
                    continue;
                }
                let mut pos = FixedStr::<48>::new();
                let more = m.orders.saturating_sub(1);
                if compact {
                    if m.position != 0 {
                        let _ = ticket::write_compact(&mut pos, m.position, None);
                        let tw = self.glyphs.width(pos.as_str(), t.text_main, false, window);
                        self.glyphs.paint(
                            pos.as_str(),
                            cell_x + 2.0,
                            y,
                            ROW_H,
                            t.text_main,
                            false,
                            window,
                            cx,
                        );
                        // No room for the order's words: its filled mark (the strip has them).
                        if m.orders > 0 {
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(px(cell_x + 3.0 + tw), px(y + ROW_H / 2.0 - 2.0)),
                                    size(px(4.0), px(4.0)),
                                ),
                                t.text_dim,
                            ));
                        }
                    } else {
                        let order = m.order.map(|(_, side, open, _)| (side, open));
                        let _ = ticket::write_compact(&mut pos, 0, order);
                        let tw = self.glyphs.width(pos.as_str(), t.bg, false, window);
                        paint_order_chip(cell_x, y, tw + 4.0, &t, window);
                        self.glyphs.paint(
                            pos.as_str(),
                            cell_x + 2.0,
                            y,
                            ROW_H,
                            t.bg,
                            false,
                            window,
                            cx,
                        );
                    }
                    continue;
                }
                let mut order_x = cell_x + PAD;
                if m.position != 0 {
                    let _ = ticket::write_position(&mut pos, &self.product, m.position, m.avg);
                    order_x += self.glyphs.width(pos.as_str(), t.text_main, false, window) + 6.0;
                    self.glyphs.paint(
                        pos.as_str(),
                        cell_x + PAD,
                        y,
                        ROW_H,
                        t.text_main,
                        false,
                        window,
                        cx,
                    );
                }
                // A working order is filled (`docs/design.md` §3), never a position's plain text.
                if let Some((_, side, open, price)) = m.order {
                    pos.clear();
                    let _ = if m.position != 0 {
                        ticket::write_order_short(&mut pos, side, open, more)
                    } else {
                        ticket::write_order(&mut pos, &self.product, side, open, price)
                    };
                    if m.position == 0 && more > 0 {
                        let _ = std::fmt::Write::write_fmt(&mut pos, format_args!(" +{more}"));
                    }
                    let tw = self.glyphs.width(pos.as_str(), t.bg, false, window);
                    paint_order_chip(order_x - 3.0, y, tw + 6.0, &t, window);
                    self.glyphs
                        .paint(pos.as_str(), order_x, y, ROW_H, t.bg, false, window, cx);
                }
                let (ax, end) = action_at(cell_x);
                let word = if m.position != 0 { "Close" } else { "Cancel" };
                window.paint_quad(gpui::outline(
                    Bounds::new(
                        point(px(ax), px(y + 1.0)),
                        size(px(end - ax), px(ROW_H - 2.0)),
                    ),
                    t.control_edge,
                    gpui::BorderStyle::Solid,
                ));
                self.glyphs.paint_right(
                    word,
                    end - PAD / 2.0,
                    y,
                    ROW_H,
                    t.text_main,
                    false,
                    window,
                    cx,
                );
            }
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
                // The selected cell: what Enter takes.
                let side_x = match self.side {
                    Right::Call => &call_x,
                    Right::Put => &put_x,
                };
                let i = match self.cell {
                    Cell::Bid => 5,
                    Cell::Ask => 6,
                };
                window.paint_quad(gpui::outline(
                    Bounds::new(point(px(side_x[i]), px(y)), size(px(cols[i]), px(ROW_H))),
                    t.accent,
                    gpui::BorderStyle::Solid,
                ));
            }
        }
        // The underlying's price line between the strikes it falls between.
        let line_y = match (&scale, last) {
            // At its price on the chart, when that is in the chain's view.
            (Some(s), Some(l)) => {
                let (t, b) = self.frame.area;
                Some(s.y(l)).filter(|y| *y >= t + 8.0 && *y < b - 8.0)
            }
            _ => line_at.map(|v| top + HEAD_H + v as f32 * ROW_H + ROW_H / 2.0),
        };
        if let (Some(l), Some(y)) = (last, line_y) {
            let tag_h = 15.0;
            // On the chart's scale a row can be at this price: it keeps its cells.
            let (show_line, show_tag) = if scale.is_some() {
                crate::chain_lock::price_mark(y, &self.frame.ys, ROW_H, tag_h)
            } else {
                (true, true)
            };
            // Dashes by count, never by adding to a float (which a far-off origin can make
            // stand still).
            let dashes = if show_line {
                ((x - x0) / 10.0).ceil().clamp(0.0, 1_000.0) as usize
            } else {
                0
            };
            for k in 0..dashes {
                let dx = x0 + k as f32 * 10.0;
                window.paint_quad(fill(
                    Bounds::new(point(px(dx), px(y - hair)), size(px(6.0), px(2.0 * hair))),
                    t.last,
                ));
            }
            if show_tag {
                s.clear();
                let _ = std::fmt::Write::write_str(&mut s, self.product.symbol);
                let _ = std::fmt::Write::write_char(&mut s, ' ');
                let _ = self.product.price.write(l, &mut s);
                let tw = self.glyphs.width(s.as_str(), t.on_last, true, window) + 2.0 * PAD;
                // Inside the table: with one side showing the strike is its outer column, and a price
                // is never cut.
                let tx = (strike_x + (strike_w - tw) / 2.0).min(ox + w - tw).max(ox);
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
        }
        if let Some(s) = &scale {
            if self.frame.rows.is_empty() {
                let msg = "No strikes in view: zoom or pan the chart";
                self.glyphs.paint(
                    msg,
                    ox + PAD,
                    top + HEAD_H + MARKER_H,
                    ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            self.paint_markers(s, bounds, window, cx);
        }
        pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
    }

    /// A line's level, the drag's while it's the one being dragged.
    fn line_level(&self, line: &crate::chain_lines::Line) -> Price {
        match self.line_drag {
            Some(d) if d.line.same(line) => d.to,
            _ => line.level,
        }
    }

    fn line_color(&self, line: &crate::chain_lines::Line) -> gpui::Hsla {
        let t = &self.theme;
        if line.stale {
            return t.warn;
        }
        match self.line_drag {
            Some(d) if d.refused.is_some() && d.line.same(line) => t.warn,
            _ => match line.kind {
                TriggerKind::Stop => t.order_stop,
                TriggerKind::Target => t.order_target,
                TriggerKind::Entry { .. } => t.accent,
            },
        }
    }

    /// A line across `x0..x1`: solid once armed, dashed until then or while it can't fire.
    fn paint_line(
        &self,
        line: &crate::chain_lines::Line,
        x0: f32,
        x1: f32,
        y: f32,
        window: &mut Window,
    ) {
        let color = self.line_color(line);
        let armed = matches!(line.source, crate::chain_lines::LineSource::Armed(_));
        if armed && !line.stale {
            window.paint_quad(fill(
                Bounds::new(
                    point(px(x0), px(y - 0.5)),
                    size(px((x1 - x0).max(0.0)), px(1.0)),
                ),
                color,
            ));
            return;
        }
        // Dashes by count, never by adding to a float.
        let dashes = ((x1 - x0) / 10.0).ceil().clamp(0.0, 1_000.0) as usize;
        for k in 0..dashes {
            let dx = x0 + k as f32 * 10.0;
            window.paint_quad(fill(
                Bounds::new(
                    point(px(dx), px(y - 0.5)),
                    size(px(6.0f32.min(x1 - dx).max(0.0)), px(1.0)),
                ),
                color,
            ));
        }
    }

    /// The lines' stretch across the table, and where a press finds them. On a row's band only
    /// the margins beside the cells: the row's text stays readable.
    fn paint_chain_lines(
        &mut self,
        scale: &crate::chain_lock::LockedScale,
        bounds: Bounds<Pixels>,
        cells: (f32, f32),
        window: &mut Window,
    ) {
        let (x0, x1) = (
            f(bounds.origin.x),
            f(bounds.origin.x) + f(bounds.size.width),
        );
        let (top, bottom) = self.frame.area;
        for n in 0..self.lines.len() {
            let line = self.lines[n];
            let y = scale.y(self.line_level(&line));
            self.line_ys[n] = y;
            if y < top || y > bottom {
                continue;
            }
            let on_row = self.frame.ys.iter().any(|t| y >= *t && y < *t + ROW_H);
            if on_row {
                self.paint_line(&line, x0, cells.0, y, window);
                self.paint_line(&line, cells.1, x1, y, window);
            } else {
                self.paint_line(&line, x0, x1, y, window);
            }
        }
    }

    /// A line's tag: `Stop SPY 682.75 684C ≈ 1.25 -$40`. The value and P&L are estimates, always
    /// marked ≈, and text only (`option_estimate`); a line that can't fire says that first.
    fn write_tag(
        &self,
        out: &mut impl std::fmt::Write,
        line: &crate::chain_lines::Line,
        short: bool,
    ) {
        let level = self.line_level(line);
        let refused = match self.line_drag {
            Some(d) if d.line.same(line) => d.refused,
            _ => None,
        };
        if line.stale {
            let _ = out.write_str("STALE FEED  ");
        }
        let _ = write!(out, "{} ", line.kind.word());
        if !matches!(line.source, crate::chain_lines::LineSource::Armed(_)) {
            let _ = out.write_str("(not armed) ");
        }
        let _ = write!(out, "{} ", self.product.symbol);
        let _ = self.product.price.write(level, out);
        let _ = out.write_char(' ');
        let _ = crate::chain_lines::write_series(out, line.contract);
        let Some(key) = line.contract.option_key() else {
            return;
        };
        // Another expiry's strike is another contract.
        let shown = self.frame.expiry == Some(key.expiry);
        if !shown {
            let _ = out.write_char(' ');
            let _ = write_chip_date(out, key.expiry);
        }
        if let Some(why) = refused {
            // Short: its armed row or ticket has the reason.
            let _ = write!(out, "  {}", if short { "can't go there" } else { why });
            return;
        }
        if short {
            return;
        }
        let q = self.quote(key);
        let under = self.frame.last;
        let Some(e) =
            crate::option_estimate::estimate(line.contract, q.as_ref(), under, level, self.now)
        else {
            return;
        };
        let _ = out.write_str(" ≈ ");
        let _ = e.write(out, &self.product);
        if let (Some(per_unit), Some(m)) = (
            self.tag_money,
            self.frame
                .mark(key.strike, key.right)
                .filter(|m| shown && m.position != 0),
        ) && let Some(avg) = m.avg
        {
            let _ = out.write_char(' ');
            let _ = e.write_pnl(out, m.position, avg, per_unit);
        }
    }

    /// The lines over the chart, with their tags at the plot's right edge. Tags spread so none
    /// overlap; each stays as near its line as it can.
    fn paint_chart_lines(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let Some(chart) = self.chart.as_ref().filter(|_| self.chart_shown()) else {
            return;
        };
        // The chart's own menu or field is on top of what's drawn over it.
        if chart.read(cx).overlay_open() {
            self.tag_boxes = [(0.0, 0.0, 0.0); crate::chain_lines::MAX_LINES];
            return;
        }
        let Some(scale) = chart.read(cx).locked_scale() else {
            return;
        };
        let n = self.lines.len().min(crate::chain_lines::MAX_LINES);
        let x0 = f(bounds.origin.x);
        let plot_right = x0 + scale.vp.width as f32;
        let right = plot_right - 4.0;
        // Top first; those off the plot have no line and no tag (their armed row says where).
        let before = self.tag_boxes;
        let mut order = [0usize; crate::chain_lines::MAX_LINES];
        let mut centers = [0.0f32; crate::chain_lines::MAX_LINES];
        let mut shown = 0;
        for i in 0..n {
            self.tag_boxes[i] = (0.0, 0.0, 0.0);
            let line = self.lines[i];
            let y = scale.y(self.line_level(&line));
            if y < scale.plot_top || y > scale.bottom() {
                continue;
            }
            // To the plot's edge: the axis keeps its labels readable.
            self.paint_line(&line, x0, plot_right, y, window);
            let mut at = shown;
            while at > 0 && centers[at - 1] > y {
                centers[at] = centers[at - 1];
                order[at] = order[at - 1];
                at -= 1;
            }
            centers[at] = y;
            order[at] = i;
            shown += 1;
        }
        crate::chain_lines::spread_tags(
            &mut centers[..shown],
            TAG_H + 2.0,
            scale.plot_top,
            scale.bottom(),
        );
        let t = self.theme;
        for k in 0..shown {
            let line = self.lines[order[k]];
            let mut s = FixedStr::<128>::new();
            self.write_tag(&mut s, &line, false);
            let mut w = self.glyphs.width(s.as_str(), t.text_main, true, window) + 2.0 * PAD;
            // Whole, in a narrow plot: the words it can spare go. Its armed row has them.
            if w > right - x0 {
                s = FixedStr::<128>::new();
                self.write_tag(&mut s, &line, true);
                w = self.glyphs.width(s.as_str(), t.text_main, true, window) + 2.0 * PAD;
            }
            let x = (right - w).max(x0);
            let y = centers[k] - TAG_H / 2.0;
            let chip = Bounds::new(point(px(x), px(y)), size(px(w), px(TAG_H)));
            window.paint_quad(fill(chip, t.panel));
            window.paint_quad(gpui::outline(
                chip,
                self.line_color(&line),
                gpui::BorderStyle::Solid,
            ));
            self.glyphs
                .paint(s.as_str(), x + PAD, y, TAG_H, t.text_main, true, window, cx);
            self.tag_boxes[order[k]] = (x - x0, y, w);
        }
        // The handles are built before this paints: a tag or plot that moved needs them built again.
        let plot_w = scale.vp.width as f32;
        if self.tag_boxes != before || self.plot_w != plot_w {
            self.plot_w = plot_w;
            window.on_next_frame(|window, _| window.refresh());
        }
    }

    /// P&L to close: past now to the close, on the chart's price scale, shaded by estimated P&L.
    /// Signed values and a breakeven line say it without color. Display only; nothing here
    /// allocates: the cells are computed and painted as they go.
    fn paint_pnl_panel(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        const COLS: usize = 14;
        const CELL_H: f32 = 12.0;
        const MAX_ROWS: usize = 160;
        /// Where the signed values go: near now, the middle, and near the close.
        const LABEL_COLS: [usize; 3] = [2, 7, 12];
        const LABEL_EVERY: f32 = 54.0;
        let t = self.theme;
        window.paint_quad(fill(bounds, t.bg));
        let Some(scale) = self.chart_scale(cx) else {
            return;
        };
        let Some(traded) = self.traded else {
            self.glyphs.paint(
                "No estimate",
                f(bounds.origin.x) + PAD,
                scale.plot_top + 6.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
            return;
        };
        let (x0, w) = (f(bounds.origin.x), f(bounds.size.width));
        let (top, bottom) = (scale.plot_top, scale.bottom());
        let band_top = f(bounds.origin.y);
        window.paint_quad(fill(
            Bounds::new(point(px(x0), px(band_top)), size(px(w), px(top - band_top))),
            t.panel,
        ));
        // What it's of. No estimate without the quote's volatility: then it says so and stops.
        let mut title = FixedStr::<64>::new();
        let product = &self.product;
        let basis = traded.basis;
        let room = w - 2.0 * PAD;
        let _ = crate::chain_pnl::write_title(
            &mut title,
            traded.contract,
            traded.position,
            |o| match basis {
                Some(b) => crate::trade_bar::write_avg(o, product, b),
                None => std::fmt::Write::write_str(o, "—"),
            },
            traded.qty,
        );
        // Whole in a narrow panel: its price goes, never what says whose it is, or the ≈.
        if self.glyphs.width(title.as_str(), t.text_main, true, window) > room {
            title = FixedStr::<64>::new();
            let _ = crate::chain_pnl::write_title_short(
                &mut title,
                traded.contract,
                traded.position,
                traded.qty,
            );
        }
        self.glyphs.paint(
            title.as_str(),
            x0 + PAD,
            band_top,
            top - band_top,
            t.text_main,
            true,
            window,
            cx,
        );
        let (Some(model), Some(_)) = (traded.model, basis) else {
            self.glyphs.paint(
                "No estimate",
                x0 + PAD,
                top + 6.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
            return;
        };
        let (now, close) = (model.now(), model.close());
        let cw = w / COLS as f32;
        let time =
            |col: f32| now + ((close - now).max(0) as f64 * f64::from(col / COLS as f32)) as i64;
        let price_at = |y: f32| scale.vp.y_to_price(f64::from(y - top));
        // The shade is against the largest in view, which is at a corner.
        let mut largest = 0.0f64;
        for y in [top, bottom] {
            for at in [now, close] {
                if let Some(p) = traded.pnl_at(&model, price_at(y), at) {
                    largest = largest.max(p.size());
                }
            }
        }
        let rows = (((bottom - top) / CELL_H).ceil() as usize).min(MAX_ROWS);
        let cell_h = (bottom - top) / rows.max(1) as f32;
        for col in 0..COLS {
            let at = time(col as f32 + 0.5);
            // Edges on whole pixels, each cell's start the last one's end: no gap, and no overlap
            // to show as a line through a translucent shade.
            let cx0 = (x0 + col as f32 * cw).round();
            let cx1 = (x0 + (col + 1) as f32 * cw).round();
            for row in 0..rows {
                let y = (top + row as f32 * cell_h).round();
                let y1 = (top + (row + 1) as f32 * cell_h).round();
                let Some(p) = traded.pnl_at(&model, price_at((y + y1) / 2.0), at) else {
                    continue;
                };
                let color = if p.is_loss() { t.down } else { t.up };
                let a = 0.03 + 0.25 * p.shade(largest);
                window.paint_quad(fill(
                    Bounds::new(point(px(cx0), px(y)), size(px(cx1 - cx0), px(y1 - y))),
                    gpui::Hsla { a, ..color },
                ));
            }
        }
        // The breakeven: where it's worth what it cost, at each time.
        let (lo, hi) = (scale.vp.price_lo, scale.vp.price_hi);
        let mut even = [f32::NAN; COLS];
        for (col, y) in even.iter_mut().enumerate() {
            if let Some(level) = traded
                .basis
                .and_then(|b| model.level_for(b, time(col as f32 + 0.5), lo, hi))
            {
                *y = top + scale.vp.price_to_y(level) as f32;
            }
        }
        for col in 0..COLS {
            let y = even[col];
            if y.is_nan() {
                continue;
            }
            let cx0 = x0 + col as f32 * cw;
            window.paint_quad(fill(
                Bounds::new(point(px(cx0), px(y - 0.5)), size(px(cw), px(1.0))),
                t.text_main,
            ));
            if let Some(next) = even.get(col + 1).filter(|n| !n.is_nan()) {
                let (a, b) = (y.min(*next), y.max(*next));
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(cx0 + cw - 0.5), px(a)),
                        size(px(1.0), px((b - a).max(1.0))),
                    ),
                    t.text_main,
                ));
            }
        }
        // Signed values, so color isn't the only signal; none on the breakeven's line.
        let mut y = top + LABEL_EVERY / 2.0;
        while y < bottom - ROW_H {
            for col in LABEL_COLS {
                if (even[col] - y).abs() < ROW_H {
                    continue;
                }
                let Some(p) = traded.pnl_at(&model, price_at(y), time(col as f32 + 0.5)) else {
                    continue;
                };
                let mut s = FixedStr::<16>::new();
                let _ = p.write(&mut s);
                let tw = self.glyphs.width(s.as_str(), t.text_main, false, window);
                // Inside the panel, however large the figure: the chain paints over the rest.
                let lx = (x0 + (col as f32 + 0.5) * cw - tw / 2.0)
                    .min(x0 + w - tw - 2.0)
                    .max(x0 + 2.0);
                self.glyphs.paint(
                    s.as_str(),
                    lx,
                    y - ROW_H / 2.0,
                    ROW_H,
                    t.text_main,
                    false,
                    window,
                    cx,
                );
            }
            y += LABEL_EVERY;
        }
        // Its name, where the line is, on a chip so it's read over the values.
        // (A plot shorter than the chip has no room to name it.)
        let named = (3..COLS)
            .find(|c| !even[*c].is_nan())
            .filter(|_| bottom - top >= TAG_H);
        if let Some(col) = named {
            let word = "breakeven";
            let tw = self.glyphs.width(word, t.text_main, false, window) + 2.0 * 4.0;
            let (bx, by) = (
                (x0 + col as f32 * cw).min(x0 + w - tw),
                (even[col] - TAG_H / 2.0).clamp(top, bottom - TAG_H),
            );
            window.paint_quad(fill(
                Bounds::new(point(px(bx), px(by)), size(px(tw), px(TAG_H))),
                t.panel,
            ));
            self.glyphs
                .paint(word, bx + 4.0, by, TAG_H, t.text_main, false, window, cx);
        }
        // Now is the left edge; the lines and the price carry on across.
        window.paint_quad(fill(
            Bounds::new(point(px(x0), px(top)), size(px(1.0), px(bottom - top))),
            t.axis_line,
        ));
        if let Some(l) = self.frame.last {
            let y = scale.y(l);
            if y >= top && y <= bottom {
                window.paint_quad(fill(
                    Bounds::new(point(px(x0), px(y - 0.5)), size(px(w), px(1.0))),
                    t.last,
                ));
            }
        }
        for n in 0..self.lines.len() {
            let line = self.lines[n];
            let y = scale.y(self.line_level(&line));
            if y >= top && y <= bottom {
                self.paint_line(&line, x0, x0 + w, y, window);
            }
        }
        // The time axis, in its band under the plot.
        let (band, band_h) = (bottom, f(bounds.origin.y + bounds.size.height) - bottom);
        window.paint_quad(fill(
            Bounds::new(point(px(x0), px(band)), size(px(w), px(band_h))),
            t.bg,
        ));
        self.glyphs.paint(
            "now",
            x0 + 4.0,
            band,
            band_h.min(ROW_H),
            t.text_dim,
            false,
            window,
            cx,
        );
        let mut closes = FixedStr::<24>::new();
        let _ = std::fmt::Write::write_str(&mut closes, "close ");
        if let Some(k) = traded.contract.option_key() {
            let _ = write_chip_date(&mut closes, k.expiry);
        }
        self.glyphs.paint_right(
            closes.as_str(),
            x0 + w - 4.0,
            band,
            band_h.min(ROW_H),
            t.text_dim,
            false,
            window,
            cx,
        );
        pane::paint_stale_veil(&self.store, bounds, &self.theme, window);
    }

    /// What the account has past the rows, at the edge it's past: nothing it holds or works goes
    /// unseen on the view it trades from. The foot's marker joins the top's while the ticket or
    /// a long message covers the foot.
    fn paint_markers(
        &mut self,
        scale: &crate::chain_lock::LockedScale,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        use crate::chain_lock::Off;
        let off = self.frame.off_rows(scale, self.foot_top);
        let none = Off::default();
        let top_y = f(bounds.origin.y) + HEAD_H;
        let foot_y = self.frame.area.1;
        if self.foot_top < foot_y + MARKER_H {
            let above = off.above.nearest.is_some();
            let target = off.above.nearest.or(off.below.nearest);
            self.paint_marker(
                0,
                off.above,
                off.below,
                off.between,
                top_y,
                target,
                above,
                bounds,
                window,
                cx,
            );
        } else {
            let above = off.above.nearest;
            let below = off.below.nearest;
            self.paint_marker(
                0, off.above, none, none, top_y, above, true, bounds, window, cx,
            );
            self.paint_marker(
                1,
                none,
                off.below,
                off.between,
                foot_y,
                below,
                false,
                bounds,
                window,
                cx,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_marker(
        &mut self,
        slot: usize,
        above: crate::chain_lock::Off,
        below: crate::chain_lock::Off,
        between: crate::chain_lock::Off,
        y: f32,
        target: Option<Price>,
        target_above: bool,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !above.any() && !below.any() && !between.any() {
            return;
        }
        let t = self.theme;
        let (ox, w) = (f(bounds.origin.x), f(bounds.size.width));
        use crate::chain_lock::Detail;
        let mut s = FixedStr::<128>::new();
        let mut tw = 0.0;
        for detail in [Detail::Full, Detail::Totals, Detail::Bare] {
            s = FixedStr::<128>::new();
            crate::chain_lock::write_marker(&mut s, above, below, between, detail);
            tw = self.glyphs.width(s.as_str(), t.text_main, true, window);
            if tw + 2.0 * PAD <= w {
                break;
            }
        }
        let chip_w = (tw + 2.0 * PAD).min(w);
        let x = ox + (w - chip_w) / 2.0;
        let chip = Bounds::new(point(px(x), px(y)), size(px(chip_w), px(MARKER_H)));
        window.paint_quad(fill(chip, t.panel));
        let edge = if target.is_some() {
            t.control_edge
        } else {
            t.axis_line
        };
        window.paint_quad(gpui::outline(chip, edge, gpui::BorderStyle::Solid));
        self.glyphs.paint(
            s.as_str(),
            x + PAD,
            y,
            MARKER_H,
            t.text_main,
            true,
            window,
            cx,
        );
        self.frame.markers[slot] = target.map(|target| crate::chain_lock::MarkerHit {
            x: (x, x + chip_w),
            y: (y, y + MARKER_H),
            target,
            above: target_above,
        });
    }

    /// A press on a marker: the chart's scale widens to take in the nearest strike past that
    /// edge. It's the trader's own scale change, as a zoom is, and the rows follow.
    fn bring_into_view(&mut self, hit: crate::chain_lock::MarkerHit, cx: &mut Context<Self>) {
        let area = self.frame.area;
        if let Some(chart) = &self.chart {
            chart.update(cx, |c, cx| {
                let range = c.locked_scale().and_then(|s| {
                    crate::chain_lock::range_showing(&s, hit.target, area, ROW_H, hit.above)
                });
                if let Some((lo, hi)) = range {
                    c.set_price_range(lo, hi);
                    cx.notify();
                }
            });
        }
    }

    fn header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(48);
        // Narrow: the price stays on its row, so the expiry and feed word fit whole.
        let narrow = self.header_width() < NARROW_HEADER;
        let today = pane::today(self.frozen, &self.store);
        self.expiries.clear();
        let mut shown = None;
        let mut word = None;
        if let Some(title) = pane::unavailable_title(&self.feed) {
            left.push_str(&title);
        } else if let Some(d) = self.store.try_read() {
            left.push_str(self.product.symbol);
            if let Some(l) = d.last_trade.filter(|_| !narrow) {
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
        if !narrow {
            left.push_str("  Option chain");
        }
        // The shown expiry, named even where its chip doesn't fit.
        if let Some(e) = shown.filter(|_| !narrow) {
            left.push_str(&format!("  {}", chip_date(e)));
        }
        let held = self.held_expiries();
        let account = self.trade.account.as_ref().and_then(|a| {
            Trading::try_global(cx)
                .and_then(|t| t.account(a))
                .map(|i| (i.name, i.badge))
        });
        let one_click = self.one_click;
        let size = self.size(cx);
        let exits = if self.exits_in_header() {
            self.exit_buttons(cx)
        } else {
            None
        };
        let chips = self.expiries.clone().into_iter().map(|e| {
            let on = Some(e) == shown;
            let mut label = chip_date(e);
            label.push_str(" · ");
            let _ = write_dte(&mut label, today, e);
            // Something held or working on another expiry never goes unseen.
            if held.contains(&e) {
                label.push_str(" •");
            }
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
                            this.expiry_pinned = false;
                            this.selected = None;
                            this.frame.release();
                            this.leave_ticket();
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
            // Fixed: the exits' buttons appearing never push the rows down.
            .h(px(pane::header_height(crate::layout::PaneKind::Chain)))
            .items_center()
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
                        .when(self.quotes_stale, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .px_1()
                                    .bg(t.warn)
                                    .text_color(t.on_warn)
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .child("QUOTES STALE"),
                            )
                        })
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
                                // Narrow: not even a whole one fits beside the switch.
                                .when(!narrow, |d| d.children(chips)),
                        ),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    // A slot of their own, there before anything is held: they never appear
                    // under a pointer aimed at an expiry chip.
                    .when(
                        self.exits_in_header() && self.trade.account.is_some(),
                        |d| {
                            d.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_shrink_0()
                                    .justify_end()
                                    .w(px(EXITS_SLOT))
                                    .children(exits),
                            )
                        },
                    )
                    .children(account.map(|(name, badge)| {
                        // A click goes to the next account, on the badge alone when narrow.
                        div()
                            .id("chain-account")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .text_color(t.text_main)
                            .when(!narrow, |d| d.child(format!("{name} ▸")))
                            .child(trade_bar::account_badge(badge, &t))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.next_account(cx);
                                }),
                            )
                    }))
                    // One side in view: the way to the other. Left of the chips here, so turning P&L to close
                    // on moves none of them, and the same width either side.
                    .children(self.pnl_shown().then(|| {
                        let word = match self.side {
                            Right::Call => "Calls",
                            Right::Put => "Puts",
                        };
                        div()
                            .id("chain-side")
                            .flex_shrink_0()
                            .min_w(px(42.0))
                            .cursor_pointer()
                            .px_1()
                            .border_1()
                            .rounded_sm()
                            .border_color(t.control_edge)
                            .text_color(t.text_main)
                            .child(word)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.flip_side(cx);
                                }),
                            )
                    }))
                    // Off, it costs a narrow header room for nothing: the chart wouldn't fit there.
                    .children(
                        (self.chart.is_some() || self.pane_w >= CHART_MIN_W).then(|| {
                            let on = self.chart.is_some();
                            div()
                                .id("chain-chart")
                                .flex_shrink_0()
                                .cursor_pointer()
                                .px_1()
                                .border_1()
                                .rounded_sm()
                                .border_color(if on { t.accent } else { t.control_edge })
                                .text_color(t.text_main)
                                .child(if on { "Chart: on" } else { "Chart: off" })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_chart(cx);
                                    }),
                                )
                        }),
                    )
                    .children(self.chart_shown().then(|| {
                        let on = self.pnl_shown();
                        div()
                            .id("chain-pnl")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .px_1()
                            .border_1()
                            .rounded_sm()
                            .border_color(if on { t.accent } else { t.control_edge })
                            .text_color(t.text_main)
                            .child(if on { "To close: on" } else { "To close: off" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_pnl(cx);
                                }),
                            )
                    }))
                    .child(
                        div()
                            .id("chain-one-click")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .px_1()
                            .border_1()
                            .rounded_sm()
                            .border_color(if one_click { t.accent } else { t.control_edge })
                            .text_color(if one_click { t.bg } else { t.text_main })
                            .when(one_click, |d| {
                                d.bg(t.accent).font_weight(gpui::FontWeight::BOLD)
                            })
                            .child(if one_click && narrow {
                                format!("1-click ON · {size}")
                            } else if one_click {
                                format!("One-click: ON · Qty {size}")
                            } else {
                                "One-click: off".to_owned()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_one_click(cx);
                                }),
                            ),
                    ),
            )
    }

    /// The header has room for the exits (below this they're in the footer).
    fn exits_in_header(&self) -> bool {
        self.header_width() >= EXITS_IN_HEADER
    }

    /// The header spans the pane; beside a chart the table is only part of it.
    fn header_width(&self) -> f32 {
        if self.chart_shown() {
            self.pane_w
        } else {
            f(self.bounds.size.width)
        }
    }

    /// Expiries where the account holds or works something.
    fn held_expiries(&self) -> Vec<CivilDate> {
        let mut out = Vec::new();
        let (Some(a), Some(st)) = (self.trade.account.as_ref(), self.trade.try_cache()) else {
            return out;
        };
        for k in ticket::held_contracts(&st, a, self.product.id)
            .iter()
            .filter_map(|c| c.option_key())
        {
            if !out.contains(&k.expiry) {
                out.push(k.expiry);
            }
        }
        out
    }

    /// The selected series' position and orders with Close and Cancel, in a slot of its own
    /// that is always there while the pane trades: nothing moves when a first order lands.
    fn row_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let t = self.theme;
        let account = self.trade.account.as_ref()?;
        if !self.frame.has_chain {
            return None;
        }
        let key = self.selected_key();
        let (position, orders) = match (key, self.trade.try_cache()) {
            (Some(key), Some(st)) => {
                let contract = self.contract(key);
                let position = st.position(account, contract).map(|p| (p.qty, p.avg_price));
                let orders: Vec<(OrderId, Side, i64, Option<Price>)> = st
                    .open_orders(account, contract)
                    .map(|o| (o.id, o.side, o.qty.0 - o.filled.0, o.kind.prices().next()))
                    .collect();
                (position, orders)
            }
            _ => (None, Vec::new()),
        };
        let button = |id: gpui::SharedString, word: &'static str| {
            div()
                .id(id)
                .flex_shrink_0()
                .cursor_pointer()
                .px_1()
                .h(px(MIN_HIT))
                .flex()
                .items_center()
                .border_1()
                .rounded_sm()
                .border_color(t.control_edge)
                .text_color(t.text_main)
                .child(word)
        };
        let width = f(self.bounds.size.width);
        let narrow = width < NARROW_HEADER;
        let mut row = div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .h(px(STRIP_H))
            .overflow_hidden()
            .whitespace_nowrap()
            .items_center()
            .gap_2()
            .px_2()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .text_color(t.text_main);
        // Narrow: the row's own place in the table names it; the room goes to its actions.
        if let Some(key) = key.filter(|_| !narrow) {
            row = row.child(ticket::contract_name(
                &self.product,
                key,
                &chip_date(key.expiry),
            ));
        }
        if let Some(text) = strip_state(key.is_some(), position.is_some() || !orders.is_empty()) {
            row = row.child(div().text_color(t.text_dim).child(text));
        }
        let key = key.unwrap_or(OptionKey {
            expiry: self.frame.expiry?,
            strike: Price(0),
            right: self.side,
        });
        let shown = strip_orders_fit(width, narrow, position.is_some(), orders.len());
        let hidden = orders.len() - shown;
        let orders = orders.into_iter().take(shown);
        if let Some((qty, avg)) = position {
            let mut s = String::new();
            let _ = ticket::write_position(&mut s, &self.product, qty, avg);
            row = row
                .child(s)
                .child(button("chain-close".into(), "Close").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        if ev.click_count > 1 {
                            return;
                        }
                        this.focus.focus(window, cx);
                        this.press_close(key, cx);
                        cx.notify();
                    }),
                ));
        }
        for (id, side, open, price) in orders {
            let mut s = String::new();
            let _ = ticket::write_order(&mut s, &self.product, side, open, price);
            // An order and its Cancel wrap as one.
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(t.text_dim)
                            .text_color(t.bg)
                            .child(s),
                    )
                    .child(
                        button(format!("chain-cancel-{}", id.0).into(), "Cancel").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                // The next order's Cancel slides under a double-click's second press.
                                if ev.click_count > 1 {
                                    return;
                                }
                                this.focus.focus(window, cx);
                                this.press_cancel(id, cx);
                                cx.notify();
                            }),
                        ),
                    ),
            );
        }
        // The rest stay reachable: the row's mark, and Cancel for all of them.
        if hidden > 0 {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_color(t.text_dim)
                    .child(format!("+{hidden} more")),
            );
        }
        Some(row.into_any_element())
    }

    /// The ticket, docked at the bottom of the pane.
    fn ticket_dock(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let t = self.theme;
        let tk = self.ticket.clone()?;
        let product = self.product.clone();
        let contract = self.contract(tk.key);
        let rules = self.rules()?;
        let q = self.live_quote(tk.key);
        let (bid, ask) = ticket::sides(q.as_ref());
        let mid = match (bid, ask) {
            (Some(b), Some(a)) => Some(ticket::mid_limit(tk.side, b, a, rules)),
            _ => None,
        };
        let position = self.position(tk.key);
        let closable = ticket::closes(position, tk.side);
        let qty = if tk.close {
            closable.unwrap_or(0)
        } else {
            self.size(cx)
        };
        // What's true now decides the button; the core's last refusal stays in view until the
        // ticket changes or Enter tries again.
        let closing_now = tk.close
            && self
                .trade
                .account
                .as_ref()
                .is_some_and(|a| self.closing(a, tk.key));
        let when = tk.when;
        let live = if when.is_some() {
            self.when_refusal()
        } else if tk.close && closable.is_none() {
            Some("no position to close".to_owned())
        } else if closing_now {
            Some("a close is already working; Cancel it first".to_owned())
        } else {
            self.refusal_of(tk.key, tk.side, tk.price, tk.close, cx)
        };
        let can_send_now = live.is_none();
        let reason = live.or_else(|| tk.refused.clone());
        let stale = self.store.stale_for().is_some();
        let can_send = can_send_now && tk.sent.is_none() && qty > 0;
        let chip = |id: gpui::SharedString, on: bool, label: String| {
            div()
                .id(id)
                .flex_shrink_0()
                .cursor_pointer()
                .px_1()
                .h(px(MIN_HIT))
                .min_w(px(MIN_HIT))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .rounded_sm()
                .border_color(if on { t.accent } else { t.control_edge })
                .text_color(t.text_main)
                .when(on, |d| d.font_weight(gpui::FontWeight::BOLD))
                .child(label)
        };
        let price_chips = [("Bid", bid), ("Mid", mid), ("Ask", ask)].map(|(word, p)| {
            let label = match p {
                Some(p) => format!("{word} {}", product.price.format::<32>(p).as_str()),
                None => format!("{word} —"),
            };
            chip(
                format!("ticket-{word}").into(),
                p.is_some() && p == tk.price && tk.typed.is_none(),
                label,
            )
            .min_w(px(PRICE_CHIP))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.focus.focus(window, cx);
                    if let (Some(p), Some(t)) = (p, this.ticket.as_mut()) {
                        t.set_price(p);
                    }
                    cx.notify();
                }),
            )
        });
        let size = self.size(cx);
        let narrow = f(self.bounds.size.width) < NARROW_HEADER;
        let qty_chips = (!tk.close).then(|| {
            crate::dom_trade::QTY_PRESETS.map(|n| {
                chip(format!("ticket-qty-{n}").into(), n == size, n.to_string()).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.set_size(n, cx);
                    }),
                )
            })
        });
        // A size set elsewhere that isn't a preset still reads.
        let custom = (!tk.close && !crate::dom_trade::QTY_PRESETS.contains(&size))
            .then(|| chip("ticket-qty-own".into(), true, size.to_string()));
        let price_text = match (&tk.typed, tk.price) {
            (Some(s), _) => s.clone(),
            (None, Some(p)) => product.price.format::<32>(p).as_str().to_owned(),
            (None, None) => "—".to_owned(),
        };
        let premium = tk
            .price
            .filter(|_| qty > 0)
            .and_then(|p| ticket::premium(&product, contract, qty, p))
            .map(|m| {
                let word = match tk.side {
                    Side::Buy if when.is_some() => "Up to",
                    Side::Buy => "Cost",
                    Side::Sell => "Credit",
                };
                format!("{word} {}", trade_bar::money_grouped(m))
            });
        let send = match when {
            Some(level) => format!(
                "Arm: buy {qty} when {} hits {}",
                product.symbol,
                product.price.format::<32>(level).as_str()
            ),
            None => ticket::send_label(&product, tk.side, tk.close, qty, tk.price),
        };
        let side_color = match tk.side {
            Side::Buy => t.up,
            Side::Sell => t.down,
        };
        let account_name = self
            .trade
            .account
            .as_ref()
            .and_then(|a| Trading::try_global(cx).and_then(|t| t.account(a)))
            .map(|i| i.name);
        let top = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(side_color)
                    .child(ticket::side_word(tk.side)),
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(ticket::contract_name(
                        &product,
                        tk.key,
                        &chip_date(tk.key.expiry),
                    )),
            )
            .children((closable.is_some() || tk.close).then(|| {
                let label = match closable {
                    Some(n) => format!("Close {n}"),
                    None => "Close".to_owned(),
                };
                chip("ticket-close".into(), tk.close, label).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.toggle_close(cx);
                    }),
                )
            }))
            // Now, or when the underlying gets there (decision 0029): a line needs the chart.
            .children((self.chart_shown() && tk.can_be_when()).then(|| {
                let symbol = product.symbol;
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        chip("ticket-now".into(), when.is_none(), "Now".to_owned()).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.focus.focus(window, cx);
                                this.set_when(false, cx);
                            }),
                        ),
                    )
                    .child(
                        chip(
                            "ticket-when".into(),
                            when.is_some(),
                            format!("When {symbol}"),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.focus.focus(window, cx);
                                this.set_when(true, cx);
                            }),
                        ),
                    )
            }))
            .children(account_name.map(|n| div().text_color(t.text_dim).child(n)));
        let mut controls = div().flex().flex_row().flex_wrap().items_center().gap_2();
        if let Some(chips) = qty_chips {
            controls = controls.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .min_w(px(QTY_SLOT))
                    .items_center()
                    .gap_1()
                    .child(div().text_color(t.text_dim).child("Qty"))
                    .children(chips)
                    .children(custom),
            );
        } else if !narrow {
            // Close mode: the size is the position, and the slot stays so nothing moves.
            controls = controls.child(
                div()
                    .flex_shrink_0()
                    .min_w(px(QTY_SLOT))
                    .text_color(t.text_dim)
                    .child(format!("Qty {qty}")),
            );
        }
        let controls = controls
            .child(
                // Wraps rather than run past a narrow chain's edge: a price is never cut.
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .max_w_full()
                    .items_center()
                    .gap_1()
                    .child(div().text_color(t.text_dim).child(if when.is_some() {
                        "Max"
                    } else {
                        "Limit"
                    }))
                    .children(price_chips),
            )
            // The level, when there is one: Tab or a press chooses which field keys go to.
            .children(when.map(|level| {
                let text = tk
                    .level_typed
                    .clone()
                    .unwrap_or_else(|| product.price.format::<32>(level).as_str().to_owned());
                div()
                    .id("ticket-level")
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .child(div().text_color(t.text_dim).child(product.symbol))
                    .child(
                        div()
                            .min_w(px(56.0))
                            .h(px(MIN_HIT))
                            .px_1()
                            .flex()
                            .items_center()
                            .justify_end()
                            .border_1()
                            .rounded_sm()
                            .border_color(if tk.on_level { t.accent } else { t.text_dim })
                            .text_color(t.text_main)
                            .child(text),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.focus.focus(window, cx);
                            if let Some(t) = this.ticket.as_mut() {
                                t.on_level = true;
                            }
                            cx.notify();
                        }),
                    )
            }))
            .child(
                div()
                    .id("ticket-price")
                    .when(when.is_some(), |d| {
                        d.cursor_pointer().on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.focus.focus(window, cx);
                                if let Some(t) = this.ticket.as_mut() {
                                    t.on_level = false;
                                }
                                cx.notify();
                            }),
                        )
                    })
                    .flex_shrink_0()
                    .min_w(px(56.0))
                    .h(px(MIN_HIT))
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .border_1()
                    .rounded_sm()
                    .border_color(if when.is_some() && !tk.on_level {
                        t.accent
                    } else {
                        t.text_dim
                    })
                    .text_color(t.text_main)
                    .child(price_text),
            )
            // A slot of its own width, so the buttons after it hold still as the price moves.
            .child(
                div()
                    .flex_shrink_0()
                    .min_w(px(92.0))
                    .text_color(t.text_main)
                    .children(premium),
            )
            .child(
                chip("ticket-esc".into(), false, "Esc".to_owned()).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.leave_ticket();
                        cx.notify();
                    }),
                ),
            )
            .child(
                div()
                    .id("ticket-send")
                    .flex_shrink_0()
                    .ml_2()
                    .min_w(px(SEND_W))
                    .justify_center()
                    .px_2()
                    .h(px(MIN_HIT))
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .font_weight(gpui::FontWeight::BOLD)
                    .when(can_send, |d| {
                        d.cursor_pointer().bg(t.accent).text_color(t.bg)
                    })
                    .when(!can_send, |d| {
                        d.border_1()
                            .border_color(t.control_edge)
                            .text_color(t.text_dim)
                    })
                    // Enter's glyph only where Enter sends.
                    .child(if can_send {
                        format!("{send} ⏎")
                    } else {
                        send
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.focus.focus(window, cx);
                            this.send_ticket(cx);
                            cx.notify();
                        }),
                    ),
            );
        let flagged = |word: &'static str, text: String| {
            // The reason wraps beside its word, whole at any width.
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .flex_shrink_0()
                        .px_1()
                        .bg(t.warn)
                        .text_color(t.on_warn)
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(word),
                )
                .child(div().flex_1().min_w_0().text_color(t.text_main).child(text))
        };
        let others = if tk.close {
            self.other_working(tk.key, tk.side)
        } else {
            0
        };
        let line = match (reason, stale) {
            (Some(why), _) => flagged("Can't send", why),
            (None, _) if others > 0 => flagged("Still working", leaves_working(others)),
            // The line's height stays, or a message arriving would move Send under the pointer.
            (None, false) if when.is_some() => div().min_h(px(ROW_H)),
            (None, true) => flagged(
                "Stale feed",
                "no new data: this quote may be old".to_owned(),
            ),
            (None, false) => div()
                .text_color(t.text_dim)
                .child(if tk.close {
                    "↑↓ price by tick  ·  Tab: back to the order  ·  Enter send  ·  Esc dismiss"
                } else if closable.is_some() {
                    "↑↓ price by tick  ·  Shift ↑↓ quantity  ·  Tab: close the position  ·  Enter send  ·  Esc dismiss"
                } else {
                    "↑↓ price by tick  ·  Shift ↑↓ quantity  ·  Enter send  ·  Esc dismiss"
                }),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .gap_1()
                .px_2()
                .py_1()
                .bg(t.panel)
                .border_t_1()
                .border_color(t.accent)
                .text_size(px(12.0))
                .text_color(t.text_main)
                .child(top)
                .child(controls)
                .child(line)
                // The limit, and what it does when it fires (decision 0029).
                .children(when.map(|_| {
                    div().text_color(t.text_dim).child(format!(
                        "{}. It sends a limit at the ask and doesn't chase: an ask over your max ends it.",
                        crate::chain_lines::LIMIT
                    ))
                }))
                .into_any_element(),
        )
    }

    /// `684C  Risk ≈ -$72  ·  Reward ≈ +$308  ·  1 : 4.3`: for a position beside the chart with a
    /// Stop or Target on it, armed or not, with the P&L display showing money.
    fn summary_text(&self) -> Option<String> {
        let traded = self
            .summary
            .filter(|_| self.chart_shown() && self.show_money)?;
        let model = traded.model?;
        let at = |kind: TriggerKind| {
            let line = self
                .lines
                .iter()
                .find(|l| l.contract == traded.contract && l.kind == kind)?;
            traded.pnl_at(&model, self.line_level(line).0 as f64, model.now())
        };
        let (risk, reward) = (at(TriggerKind::Stop), at(TriggerKind::Target));
        if risk.is_none() && reward.is_none() {
            return None;
        }
        let mut s = String::new();
        let _ = crate::chain_lines::write_series(&mut s, traded.contract);
        s.push_str("  ");
        let _ = crate::chain_pnl::write_summary(&mut s, risk, reward);
        Some(s)
    }

    /// A row for each trigger, with its Cancel, and each states the limit (decision 0029): the
    /// loud part first if it can't fire. Beside a chart: Stop and Target to add for the selection.
    fn armed_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use crate::chain_lines::{Line, row_warning};
        let t = self.theme;
        self.trade.account.as_ref()?;
        let views = self.armed_views.clone();
        let scale = self.chart_scale(cx);
        let off_chart = |level: Price| {
            scale
                .as_ref()
                .is_some_and(|s| s.y(level) < s.plot_top || s.y(level) > s.bottom())
        };
        let button = |id: String, word: &'static str| {
            div()
                .id(gpui::SharedString::from(id))
                .flex_shrink_0()
                .cursor_pointer()
                .px_1()
                .h(px(MIN_HIT))
                .flex()
                .items_center()
                .border_1()
                .rounded_sm()
                .border_color(t.control_edge)
                .text_color(t.text_main)
                .child(word)
        };
        let row = |swatch: gpui::Hsla, text: String| {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_x_2()
                .px_2()
                .py(px(2.0))
                .bg(t.panel)
                .border_t_1()
                .border_color(t.axis_line)
                .text_size(px(12.0))
                .text_color(t.text_main)
                .child(div().flex_shrink_0().size(px(8.0)).bg(swatch))
                .child(
                    div()
                        .max_w_full()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(text),
                )
        };
        let limit = || {
            div()
                .max_w_full()
                .text_color(t.text_dim)
                .child(crate::chain_lines::LIMIT)
        };
        let warned = |word: &'static str, text: String| {
            div()
                .flex()
                .flex_row()
                .max_w_full()
                .gap_1()
                .items_start()
                .child(
                    div()
                        .flex_shrink_0()
                        .px_1()
                        .bg(t.warn)
                        .text_color(t.on_warn)
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(word),
                )
                .child(div().min_w_0().child(text))
        };
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        // Risk and reward of the position, at its exits. Estimates, marked ≈; wraps, whole.
        if let Some(text) = self.summary_text() {
            rows.push(
                div()
                    .px_2()
                    .py(px(2.0))
                    .bg(t.panel)
                    .border_t_1()
                    .border_color(t.axis_line)
                    .text_size(px(12.0))
                    .text_color(t.text_main)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(text)
                    .into_any_element(),
            );
        }
        for v in views {
            let mut v = v;
            v.restored |= self.shot_restored && v.kind.is_exit();
            let mut line = Line::of(&v);
            line.stale = false;
            let mut text = FixedStr::<128>::new();
            self.write_tag(&mut text, &line, false);
            let mut text = text.as_str().to_owned();
            if let TriggerKind::Entry { qty, max_price } = v.kind {
                text.push_str(&format!(
                    ": buy {} at the ask, max {}",
                    qty.0,
                    self.product.price.format::<32>(max_price).as_str()
                ));
            }
            if v.state == lines_trading::trigger::TriggerState::Working {
                text.push_str(": closing the position");
            }
            if off_chart(v.level) {
                text.push_str(" (off the chart)");
            }
            line.stale = v.stale;
            let view = v.clone();
            let color = self.line_color(&line);
            rows.push(
                row(color, text)
                    .children(row_warning(&v).map(|(word, why)| warned(word, why)))
                    .child(limit())
                    .child(
                        button(format!("trigger-cancel-{}", v.id.0), "Cancel").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.focus.focus(window, cx);
                                if ev.click_count <= 1 && this.rows_guard_ok(cx) {
                                    this.cancel_trigger(&view, cx);
                                }
                            }),
                        ),
                    )
                    .into_any_element(),
            );
        }
        for n in 0..self.drafts.len() {
            let line = self.drafts[n];
            let mut text = FixedStr::<128>::new();
            self.write_tag(&mut text, &line, false);
            let color = self.line_color(&line);
            rows.push(
                row(color, text.as_str().to_owned())
                    .child(limit())
                    .child(button(format!("draft-arm-{n}"), "Arm").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.focus.focus(window, cx);
                            if ev.click_count <= 1 && this.rows_guard_ok(cx) {
                                this.arm_draft(n, cx);
                            }
                        }),
                    ))
                    .child(button(format!("draft-drop-{n}"), "Discard").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.focus.focus(window, cx);
                            if ev.click_count <= 1
                                && this.rows_guard_ok(cx)
                                && n < this.drafts.len()
                            {
                                this.drafts.remove(n);
                            }
                            cx.notify();
                        }),
                    ))
                    .into_any_element(),
            );
        }
        // The selection's position can have a Stop and a Target, each a line to drag first.
        if self.chart_shown()
            && let Some(key) = self.selected_key().filter(|k| self.position(*k) != 0)
        {
            let contract = self.contract(key);
            let missing = |kind: TriggerKind| {
                !self
                    .lines
                    .iter()
                    .any(|l| l.contract == contract && l.kind == kind)
            };
            let add = [
                (TriggerKind::Stop, "Set Stop"),
                (TriggerKind::Target, "Set Target"),
            ]
            .into_iter()
            .filter(|(kind, _)| missing(*kind))
            .map(|(kind, word)| {
                button(format!("draft-add-{word}"), word).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.note = None;
                        if ev.click_count <= 1 && this.rows_guard_ok(cx) {
                            this.add_draft(kind, key, cx);
                        }
                    }),
                )
            })
            .collect::<Vec<_>>();
            if !add.is_empty() {
                let mut series = String::new();
                let _ = crate::chain_lines::write_series(&mut series, contract);
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py(px(2.0))
                        .bg(t.panel)
                        .border_t_1()
                        .border_color(t.axis_line)
                        .text_size(px(12.0))
                        .text_color(t.text_main)
                        .child(
                            div().child(format!("Exits for {series}, on {}:", self.product.symbol)),
                        )
                        .children(add)
                        .into_any_element(),
                );
            }
        }
        // Many triggers scroll: the foot never takes the whole column.
        (!rows.is_empty()).then(|| {
            div()
                // Rows can move without the foot doing so (one wraps inside the cap).
                .on_children_prepainted({
                    let entity = cx.entity();
                    move |bounds, _, cx| {
                        entity.update(cx, |v, _| {
                            let same = v.armed_tops.len() == bounds.len()
                                && v.armed_tops
                                    .iter()
                                    .zip(&bounds)
                                    .all(|(t, b)| *t == f(b.origin.y));
                            if !same {
                                v.armed_tops.clear();
                                v.armed_tops.extend(bounds.iter().map(|b| f(b.origin.y)));
                                v.rows_guard.moved_under_pointer();
                            }
                        });
                    }
                })
                .id("armed-rows")
                .flex()
                .flex_col()
                .max_h(px(ARMED_MAX_H))
                .overflow_y_scroll()
                .on_scroll(cx.listener(|this, _: &gpui::ScrollEvent, _, _| {
                    this.rows_guard.moved_under_pointer();
                }))
                .children(rows)
                .into_any_element()
        })
    }

    fn footer(&self, size: i64, exits: Option<gpui::Div>) -> impl IntoElement {
        let t = self.theme;
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            // The exits' height whether they show or not.
            .min_h(px(STRIP_H))
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    // A reason wraps, whole; a lasting state gives way at the edge.
                    .when(self.note.is_none(), |d| {
                        d.whitespace_nowrap().overflow_hidden()
                    })
                    .text_color(if self.note.is_some() {
                        t.text_main
                    } else {
                        t.text_dim
                    })
                    // Narrow beside the exits there's no room for a lasting state: only a message
                    // shows. With nothing to report the row stays, empty, at its height.
                    .when(
                        self.note.is_some()
                            || (self.ticket.is_none()
                                && !(exits.is_some() && f(self.bounds.size.width) < NARROW_HEADER)),
                        |d| d.children(self.footer_text(size)),
                    ),
            )
            .children(exits)
    }

    /// What the footer reports: a message, or a state that changes what a click does or what shows.
    /// No hints in the pane (the owner's no-clutter rule).
    fn footer_text(&self, size: i64) -> Option<String> {
        footer_state(
            self.note.as_deref(),
            self.one_click,
            self.chart.is_some() && !self.chart_shown(),
            size,
        )
    }
}

/// The row strip's words beside a position or orders: state, not a hint, so with no strike selected
/// it says nothing and stays, empty, at its height.
fn strip_state(selected: bool, has_any: bool) -> Option<&'static str> {
    (selected && !has_any).then_some("No position or orders here")
}

/// The footer's words, if it has any to report.
fn footer_state(
    note: Option<&str>,
    one_click: bool,
    chart_hidden: bool,
    size: i64,
) -> Option<String> {
    if let Some(note) = note {
        return Some(note.to_owned());
    }
    if one_click {
        return Some(format!(
            "One-click ON: a click on a bid or ask sends {size} at once  ·  Shift+Up/Down: quantity"
        ));
    }
    chart_hidden.then(|| "Chart hidden: this pane is too narrow for both".to_owned())
}

/// The footer's message once One-click is switched: turned on, nothing may stand in front of its
/// warning that a click sends at once (the message is shown first, and could say it's off).
fn note_after_one_click(on: bool, note: Option<String>) -> Option<String> {
    if on { None } else { note }
}

/// Whether new data must leave the option chart's rows where they are: a press could land on one.
/// Worked out each paint from what is true now, so nothing can leave it stuck on. GPUI on Windows
/// never reports a pointer leaving an element or the window by event, only this flag.
fn hold_rows(
    table: Bounds<gpui::Pixels>,
    pointer: gpui::Point<gpui::Pixels>,
    window_hovered: bool,
    one_click: bool,
) -> bool {
    one_click || (window_hovered && table.contains(&pointer))
}

/// How many of a series' orders the strip's one line holds; at least one (at the narrowest its
/// Cancel can clip: Cancel for all of them stays).
pub fn strip_orders_fit(width: f32, narrow: bool, has_position: bool, orders: usize) -> usize {
    const ORDER_W: f32 = 150.0;
    let mut room = width - 2.0 * PAD;
    if !narrow {
        room -= 150.0; // the contract's name
    }
    if has_position {
        room -= 130.0;
    }
    (((room / ORDER_W).floor().max(1.0)) as usize).min(orders)
}

/// The rows to show: those on screen while the selected row is still among them and they still
/// fit, else [`window`] around `center`.
pub fn window_held(
    n: usize,
    fit: usize,
    center: usize,
    selected: Option<usize>,
    was: std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    if let Some(s) = selected
        && !was.is_empty()
    {
        let start = was.start.min(n.saturating_sub(fit));
        if s >= start && s < start + fit && s < n {
            return start..(start + fit).min(n);
        }
    }
    window(n, fit, center)
}

/// What a close leaves behind: the close itself can't flip, but an order left working can.
fn leaves_working(n: i64) -> String {
    format!("{n} more on this side stay working and could open a new position; Cancel them")
}

/// A working order's mark behind its text (`t.bg` on it): filled, as `docs/design.md` §3 has it.
fn paint_order_chip(x: f32, y: f32, w: f32, t: &Theme, window: &mut Window) {
    let b = Bounds::new(point(px(x), px(y + 1.0)), size(px(w), px(ROW_H - 2.0)));
    window.paint_quad(fill(b, t.text_dim));
}

/// A limit command's price.
fn order_price(cmd: &lines_trading::Command) -> Option<Price> {
    match cmd {
        lines_trading::Command::PlaceExit(o) | lines_trading::Command::PlaceOrder(o) => {
            o.kind.prices().next()
        }
        _ => None,
    }
}

impl Render for ChainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let t = self.theme;
        self.aimed = self.focus.is_focused(window) && window.is_window_active();
        self.sync_chart_visible(cx);
        self.sync_chart_span(cx);
        self.apply_shot(cx);
        self.refresh_lines(cx);
        self.refresh_traded(cx);
        self.apply_shot_drag();
        self.take_ended();
        let size = self.size(cx);
        let strip = self.row_strip(cx);
        let dock = self.ticket_dock(cx);
        let armed = self.armed_rows(cx);
        let footer_exits = if self.exits_in_header() {
            None
        } else {
            self.exit_buttons(cx)
        };
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
        let shown_chart = self.chart.clone().filter(|_| self.chart_shown());
        let table = div()
            .id("chain-table")
            .relative()
            .flex_1()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            // A redraw when the pointer crosses the edge: the hold is worked out in paint.
            .on_hover(cx.listener(|_, _: &bool, _, cx| cx.notify()))
            .child(body)
            .when_some(overlay, |d, o| d.child(o));
        // The pane's width, for whether the chart fits beside the chain.
        let measure = {
            let entity = cx.entity();
            canvas(
                move |bounds, window, cx| {
                    let w = f(bounds.size.width);
                    entity.update(cx, |v, _| {
                        v.main_top = f(bounds.origin.y);
                        // (A notify during this draw wouldn't bring another.)
                        if (v.pane_w >= CHART_MIN_W) != (w >= CHART_MIN_W) {
                            window.on_next_frame(|window, _| window.refresh());
                            // The chart comes or goes and every row moves, as with the toggle.
                            if v.pane_w > 0.0 && v.chart.is_some() {
                                v.rows_moved();
                                if w < CHART_MIN_W {
                                    v.end_line_state();
                                }
                            }
                        }
                        v.pane_w = w;
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        let main = div()
            .relative()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(measure);
        // The ticket has its own hints; a message still shows.
        let footer = (self.ticket.is_none() || self.note.is_some() || footer_exits.is_some())
            .then(|| self.footer(size, footer_exits));
        let below = div()
            .flex()
            .flex_col()
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, _| {
                this.rows_guard.pointer_moved();
            }))
            .children(armed)
            .children(strip)
            .children(dock)
            .children(footer)
            // Where it starts this frame (before the table paints): what it covers is counted.
            .child({
                let entity = cx.entity();
                canvas(
                    move |bounds, window, cx| {
                        entity.update(cx, |v, _| {
                            let top = f(bounds.origin.y);
                            let pointer = window.mouse_position();
                            // One-click: a press meant for the foot must not send on the table.
                            if v.one_click
                                && f(pointer.x) >= f(bounds.origin.x)
                                && crate::chain_lock::foot_left_pointer(
                                    v.foot_top,
                                    top,
                                    f(pointer.y),
                                )
                            {
                                v.rows_moved();
                                window.on_next_frame(|window, _| window.refresh());
                            }
                            if v.foot_top != top {
                                v.rows_guard.moved_under_pointer();
                            }
                            v.foot_top = top;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            });
        let header = self.header(cx);
        let root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(header);
        // The chart's lines, over it and under its handles.
        let chart_lines = {
            let entity = cx.entity();
            canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    entity.update(cx, |v, cx| v.paint_chart_lines(bounds, window, cx));
                },
            )
            .absolute()
            .size_full()
        };
        // Taking hold of a line on the chart: the line itself and its tag. Each takes the press
        // from the chart, which would otherwise pan. Where they were last frame, and unmoved
        // unless the plot changed, which redraws.
        // Tags on top of lines: a press on a tag means what the tag says.
        let mut line_handles = Vec::new();
        let mut tag_handles = Vec::new();
        let chart_free = !self
            .chart
            .as_ref()
            .is_some_and(|c| c.read(cx).overlay_open());
        if let Some(s) = self.chart_scale(cx).filter(|_| chart_free) {
            let handle = |id: String, n: usize, cx: &mut Context<Self>| {
                div()
                    .id(gpui::SharedString::from(id))
                    .absolute()
                    .occlude()
                    .cursor_row_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.grab_line(n, f(ev.position.y), window, cx);
                        }),
                    )
            };
            for n in 0..self.lines.len() {
                let line = self.lines[n];
                if !line.draggable() {
                    continue;
                }
                let y = s.y(self.line_level(&line));
                if y < s.plot_top || y > s.bottom() {
                    continue;
                }
                line_handles.push(
                    handle(format!("line-grab-{n}"), n, cx)
                        .left_0()
                        .top(px(y - self.main_top - LINE_GRAB))
                        .w(px(s.vp.width as f32))
                        .h(px(2.0 * LINE_GRAB)),
                );
                let (x, top, w) = self.tag_boxes[n];
                if w > 0.0 {
                    tag_handles.push(
                        handle(format!("tag-grab-{n}"), n, cx)
                            .left(px(x))
                            .top(px(top - self.main_top))
                            .w(px(w))
                            .h(px(TAG_H)),
                    );
                }
            }
        }
        // While dragging, everything in the pane is the drag's: a release off it aborts.
        let capture = self.line_drag.map(|_| {
            div()
                .id("line-drag")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .cursor_row_resize()
                .on_mouse_move(cx.listener(Self::on_line_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_line_up))
                .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_line_up_out))
        });
        // P&L to close, between the chart's axis and the chain.
        let pnl_split = self
            .pnl_shown()
            .then(|| crate::chain_pnl::split(self.pane_w));
        let pnl_panel = pnl_split.map(|(panel_w, _)| {
            let entity = cx.entity();
            div().flex_shrink_0().w(px(panel_w)).h_full().child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        entity.update(cx, |v, cx| v.paint_pnl_panel(bounds, window, cx));
                    },
                )
                .size_full(),
            )
        });
        let chain_w = pnl_split.map_or(gpui::relative(CHART_CHAIN_SHARE).into(), |(_, c)| {
            gpui::Length::from(px(c))
        });
        let pane = match shown_chart {
            // The strip, the ticket and a long message cover the column's foot rather than push
            // the plot up: a taller foot would move every row under the pointer. Its usual two lines are
            // always kept clear of rows.
            Some(chart) => root.child(
                // Cached, as a workspace pane's view is: chain quotes don't repaint the chart.
                main.child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(
                            gpui::AnyView::from(chart)
                                .cached(gpui::StyleRefinement::default().size_full()),
                        )
                        .child(chart_lines)
                        .children(line_handles)
                        .children(tag_handles),
                )
                .children(pnl_panel)
                .child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .h_full()
                        .w(chain_w)
                        .child(table)
                        .child(div().flex_shrink_0().h(px(2.0 * STRIP_H)))
                        .child(
                            below
                                .id("chain-foot")
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .occlude()
                                .on_hover(cx.listener(|_, _: &bool, _, cx| cx.notify())),
                        ),
                ),
            ),
            None => root.child(main.child(table)).child(below.flex_shrink_0()),
        };
        pane.children(capture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strip reports a selected strike's state; with none selected it has nothing to say.
    #[test]
    fn the_strip_reports_state_and_no_hints() {
        assert_eq!(
            strip_state(false, false),
            None,
            "no hint to select a strike"
        );
        assert_eq!(strip_state(true, false), Some("No position or orders here"));
        assert_eq!(
            strip_state(true, true),
            None,
            "the position and orders say it"
        );
    }

    /// The footer reports states and refusals, never a hint; with none, nothing.
    #[test]
    fn the_footer_reports_state_and_no_hints() {
        assert_eq!(footer_state(None, false, false, 3), None, "no hint");
        assert_eq!(
            footer_state(Some("refused"), true, true, 3).unwrap(),
            "refused"
        );
        let one_click = footer_state(None, true, true, 3).unwrap();
        assert!(one_click.starts_with("One-click ON") && one_click.contains("sends 3"));
        assert!(
            footer_state(None, false, true, 3)
                .unwrap()
                .starts_with("Chart hidden")
        );
    }

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

    /// Nothing can say One-click is off while it's on.
    #[test]
    fn turning_one_click_on_clears_the_message_in_front_of_its_warning() {
        let note = || Some("One-click is off: the chain moved".to_owned());
        assert_eq!(note_after_one_click(true, note()), None);
        assert_eq!(note_after_one_click(false, note()), note());
    }

    /// The hold is what's true now: the pointer on the table in a hovered window, or One-click.
    /// Nothing is left on once the pointer has gone, whichever way it went.
    #[test]
    fn the_rows_hold_only_while_a_press_could_land() {
        let table = Bounds::new(point(px(800.0), px(40.0)), size(px(700.0), px(600.0)));
        let on = point(px(900.0), px(300.0));
        let chart = point(px(300.0), px(300.0));
        assert!(hold_rows(table, on, true, false), "on the table");
        assert!(!hold_rows(table, chart, true, false), "moved to the chart");
        assert!(!hold_rows(table, on, false, false), "left the window");
        assert!(hold_rows(table, chart, true, true), "One-click, anywhere");
        assert!(hold_rows(table, chart, false, true), "One-click, anywhere");
    }

    /// With one side in view, what's held on the other is counted: it has a row, but no cell.
    #[test]
    fn a_held_series_on_the_hidden_side_is_not_shown() {
        let day = CivilDate::new(2026, 11, 9).unwrap();
        let chains = key_chain(day, 670, 690);
        let s = locked_scale();
        let mut f = locked_frame();
        f.locked = true;
        f.area = (116.0, 480.0);
        f.refresh_locked(
            chains.chain(day),
            Some(Price(68_050)),
            None,
            &s,
            116.0,
            480.0,
        );
        let strike = f.rows[0].strike;
        f.marks.push(Mark {
            strike,
            right: Right::Put,
            position: 2,
            avg: None,
            order: None,
            orders: 1,
        });
        assert_eq!(f.off_rows(&s, f32::INFINITY).between.held, 0, "both sides");
        f.hidden_side = Some(Right::Call);
        assert_eq!(f.off_rows(&s, f32::INFINITY).between.held, 0, "its side");
        f.hidden_side = Some(Right::Put);
        let off = f.off_rows(&s, f32::INFINITY);
        assert_eq!((off.between.held, off.between.working), (1, 1), "hidden");
    }

    /// A marker counts what no row shows, and its strips never hold a row, so it can't take a
    /// row's press: a row at the same y would win anyway.
    #[test]
    fn a_marker_counts_what_no_row_shows_and_never_takes_a_rows_press() {
        let day = CivilDate::new(2026, 11, 9).unwrap();
        let chains = key_chain(day, 670, 690);
        let s = locked_scale();
        let mut f = locked_frame();
        f.locked = true;
        let area = (116.0, 480.0);
        f.area = area;
        f.refresh_locked(
            chains.chain(day),
            Some(Price(68_050)),
            None,
            &s,
            area.0,
            area.1,
        );
        assert!(f.ys.iter().all(|y| *y >= area.0 && y + ROW_H <= area.1));
        let mark = |k: i64, position: i64, orders: u32| Mark {
            strike: Price(k),
            right: Right::Call,
            position,
            avg: None,
            order: None,
            orders,
        };
        let shown = f.rows[0].strike.0;
        f.marks.push(mark(shown, 2, 0));
        f.marks.push(mark(68_950, 1, 0)); // past the top on this scale
        f.marks.push(mark(67_010, 0, 3)); // past the foot
        let off = f.off_rows(&s, f32::INFINITY);
        assert_eq!((off.above.held, off.above.working), (1, 0));
        assert_eq!((off.below.held, off.below.working), (0, 3));
        assert_eq!(off.between.held + off.between.working, 0, "a row shows it");
        // A row the ticket covers is counted too, though a row still has it.
        let covered = f.off_rows(&s, f.ys[0]);
        assert_eq!(covered.between.held, 1, "behind the foot");
        assert_eq!(
            f.off_rows(&s, f.ys[0] + ROW_H).between.held,
            0,
            "just clear"
        );
        // A marker over a row can't happen; if it did, the row keeps its press.
        let y = f.ys[0] + 1.0;
        f.markers[0] = Some(crate::chain_lock::MarkerHit {
            x: (0.0, 1_000.0),
            y: (y - 5.0, y + 5.0),
            target: Price(68_950),
            above: true,
        });
        assert_eq!(f.marker_at(500.0, y), None);
        assert!(
            f.marker_at(500.0, y - 3.0 - ROW_H).is_none(),
            "off the marker"
        );
    }

    /// The chain's foot is below the table, so a strike behind it has no row to press.
    #[test]
    fn a_strike_below_the_table_has_no_row() {
        let day = CivilDate::new(2026, 11, 9).unwrap();
        let chains = key_chain(day, 670, 690);
        let s = locked_scale();
        let mut f = locked_frame();
        f.locked = true;
        f.refresh_locked(chains.chain(day), None, None, &s, 76.0, 10_000.0);
        let all = f.rows.len();
        let cut = 340.0;
        f.refresh_locked(chains.chain(day), None, None, &s, 76.0, cut);
        assert!(f.rows.len() < all && !f.rows.is_empty(), "{all}");
        assert!(f.ys.iter().all(|y| y + ROW_H <= cut));
    }

    fn locked_scale() -> crate::chain_lock::LockedScale {
        let mut vp = lines_core::Viewport::default();
        vp.set_size(800.0, 600.0);
        vp.price_lo = 67_000.0;
        vp.price_hi = 69_000.0;
        crate::chain_lock::LockedScale { vp, plot_top: 40.0 }
    }

    fn locked_frame() -> Frame {
        Frame {
            ys: Vec::with_capacity(MAX_ROWS),
            candidates: Vec::with_capacity(4 * MAX_ROWS),
            picked: Vec::with_capacity(MAX_ROWS),
            ..Frame::default()
        }
    }

    /// On the chart's scale every row is at its strike's price, a press finds that strike, and
    /// the price row takes no slot of its own.
    #[test]
    fn locked_rows_sit_at_their_prices_and_clicks_find_them() {
        let chains = key_chain(CivilDate::new(2026, 11, 9).unwrap(), 670, 690);
        let chain = chains.chain(CivilDate::new(2026, 11, 9).unwrap());
        let s = locked_scale();
        let mut f = locked_frame();
        f.locked = true;
        f.refresh_locked(chain, Some(Price(68_050)), None, &s, 76.0, 10_000.0);
        assert!(f.rows.len() > 5);
        assert_eq!(f.rows.len(), f.ys.len());
        assert_eq!(f.line_at, None);
        for (r, top) in f.rows.iter().zip(&f.ys) {
            assert_eq!(top + ROW_H / 2.0, s.y(r.strike), "{r:?}");
            assert_eq!(f.locked_at(top + 1.0), Some(r.strike));
            assert_eq!(f.locked_at(top + ROW_H - 1.0), Some(r.strike));
        }
        assert!(f.ys.windows(2).all(|t| t[1] - t[0] >= ROW_H));
        assert_eq!(f.locked_at(f.ys[0] - 1.0), None, "above the rows: nothing");
    }

    /// Held (the pointer on the chain, or One-click): the same strikes in the same places while
    /// the scale is the same, whatever the price does. The trader's own zoom moves them.
    #[test]
    fn locked_rows_hold_under_the_pointer_until_the_scale_changes() {
        let day = CivilDate::new(2026, 11, 9).unwrap();
        let chains = key_chain(day, 670, 690);
        let s = locked_scale();
        let mut f = locked_frame();
        f.locked = true;
        f.hold = true;
        f.refresh_locked(
            chains.chain(day),
            Some(Price(68_050)),
            None,
            &s,
            76.0,
            10_000.0,
        );
        let (rows, ys) = (f.rows.clone(), f.ys.clone());
        // The money moves, and a selection appears: nothing moves.
        f.refresh_locked(
            chains.chain(day),
            Some(Price(68_950)),
            Some(Price(68_500)),
            &s,
            76.0,
            10_000.0,
        );
        assert_eq!(f.ys, ys);
        assert_eq!(
            f.rows.iter().map(|r| r.strike).collect::<Vec<_>>(),
            rows.iter().map(|r| r.strike).collect::<Vec<_>>()
        );
        // The trader zooms the chart: they follow it.
        let mut zoomed = s;
        zoomed.vp.price_hi = 68_500.0;
        f.refresh_locked(
            chains.chain(day),
            Some(Price(68_050)),
            None,
            &zoomed,
            76.0,
            10_000.0,
        );
        assert_ne!(f.ys, ys);
        for (r, top) in f.rows.iter().zip(&f.ys) {
            assert_eq!(top + ROW_H / 2.0, zoomed.y(r.strike));
        }
    }

    /// A press never lands on a strike that moved: the rows hold while the selection is among
    /// them, and follow it once it leaves.
    #[test]
    fn rows_hold_still_under_the_pointer() {
        // Selected row 14 in view: the rows stay, whatever the center says.
        assert_eq!(window_held(41, 10, 14, Some(14), 10..20), 10..20);
        // Nothing selected: they follow the center.
        assert_eq!(window_held(41, 10, 30, None, 10..20), 25..35);
        // The selection left the rows: around it.
        assert_eq!(window_held(41, 10, 30, Some(30), 10..20), 25..35);
        // First frame: around it.
        assert_eq!(window_held(41, 10, 14, Some(14), 0..0), 9..19);
        // The ticket opened and fewer rows fit, with the selection still among them.
        assert_eq!(window_held(41, 6, 14, Some(14), 10..20), 10..16);
        // Fewer rows, the selection now below them: around it (with One-click on, `Frame` holds).
        assert_eq!(window_held(41, 3, 14, Some(14), 10..20), 13..16);
        // A shorter chain.
        assert_eq!(window_held(5, 10, 2, Some(2), 3..8), 0..5);
    }

    /// A frame holding through less room doesn't jump on the next frame, when the selected row
    /// is cut off and no longer among the rows drawn.
    #[test]
    fn holding_lasts_past_the_first_frame() {
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
        let mut f = Frame::default();
        let sel = Some(Price(68_400));
        f.refresh(Some(chain), None, sel, 21);
        let start = f.first_row;
        let at = chain.find(Price(68_400)).unwrap();
        assert!(at > start && at < start + f.rows.len());
        // One-click goes on: the next frame takes hold of what's on screen.
        f.hold = true;
        f.refresh(Some(chain), None, sel, 21);
        assert_eq!(f.first_row, start);
        // The strip takes its height: fewer rows, the selection now below them.
        let fit = at - start;
        for _ in 0..3 {
            f.refresh(Some(chain), None, sel, fit);
            assert_eq!(f.first_row, start, "no row moved");
        }
        // Not holding, the rows follow the selection.
        f.hold = false;
        f.refresh(Some(chain), None, sel, fit);
        assert_ne!(f.first_row, start);
    }

    fn key_chain(exp: CivilDate, low: i64, high: i64) -> lines_core::OptionChains {
        let mut c = lines_core::OptionChains::default();
        for k in low..=high {
            c.apply(
                lines_core::OptionKey {
                    expiry: exp,
                    strike: Price(k * 100),
                    right: Right::Call,
                },
                OptionQuote::default(),
            );
        }
        c
    }

    /// With One-click on the same strikes stay in the same places: when a new strike is listed
    /// below them, and when the underlying's price crosses a strike.
    #[test]
    fn holding_keeps_strikes_in_place() {
        let exp = CivilDate::new(2026, 11, 9).unwrap();
        let build = |low: i64| {
            let mut c = lines_core::OptionChains::default();
            for k in low..=704 {
                c.apply(
                    lines_core::OptionKey {
                        expiry: exp,
                        strike: Price(k * 100),
                        right: Right::Call,
                    },
                    OptionQuote::default(),
                );
            }
            c
        };
        let drawn = |f: &Frame| -> Vec<(usize, Price)> {
            f.rows
                .iter()
                .enumerate()
                .map(|(n, r)| (visual_row(n, f.line_at), r.strike))
                .collect()
        };
        let before = build(664);
        let after = build(650);
        let last = Some(Price(68_350));
        let sel = Some(Price(68_400));
        let mut f = Frame::default();
        f.refresh(before.chain(exp), last, sel, 21);
        f.hold = true;
        let want = drawn(&f);
        let line = f.line_at;
        // Fourteen new strikes below the rows: their indices move, they don't.
        f.refresh(after.chain(exp), last, sel, 21);
        assert_eq!(drawn(&f), want, "a new strike below");
        // A strike listed among the rows stays out of them until the hold ends.
        let mut among = build(650);
        among.apply(
            lines_core::OptionKey {
                expiry: exp,
                strike: Price(68_750),
                right: Right::Call,
            },
            OptionQuote::default(),
        );
        f.refresh(among.chain(exp), last, sel, 21);
        assert_eq!(drawn(&f), want, "a new strike among them");
        // Less room, then the room back: the price row's slot and every strike return.
        f.refresh(among.chain(exp), last, sel, 6);
        f.refresh(among.chain(exp), last, sel, 21);
        assert_eq!(drawn(&f), want, "less room and back");
        // More room than ever: the next strikes go below, nothing above moves.
        f.refresh(among.chain(exp), last, sel, 24);
        assert_eq!(drawn(&f)[..want.len()], want[..], "more room");
        f.refresh(among.chain(exp), last, sel, 21);
        // The price crosses two strikes: the price row keeps its slot, and so does every strike.
        f.refresh(after.chain(exp), Some(Price(68_650)), sel, 21);
        assert_eq!(drawn(&f), want, "the price moved");
        assert_eq!(f.line_at, line);
        assert_eq!(f.last, Some(Price(68_650)), "its tag reads the live price");
        // Keys move only among the strikes on screen: never onto the one listed among them.
        let on = |p: i64| Some(Price(p));
        assert_eq!(f.step_held(sel, 3), on(68_700));
        assert_eq!(
            f.step_held(on(68_700), 1),
            on(68_800),
            "past the hidden 687.50"
        );
        let (top, bottom) = (f.rows[0].strike, f.rows[f.rows.len() - 1].strike);
        assert_eq!(f.step_held(on(68_700), -1_000), Some(top));
        assert_eq!(f.step_held(on(68_700), 1_000), Some(bottom));
        assert_eq!(
            f.step_held(None, 1),
            Some(f.rows[(f.rows.len() - 1) / 2].strike)
        );
        // Home or another expiry: released, the next frame centers again and holds from there.
        f.release();
        f.refresh(among.chain(exp), Some(Price(68_650)), None, 21);
        assert!(
            f.rows.iter().any(|r| r.strike == Price(68_750)),
            "shown now"
        );
        assert!(f.holding);
        // At the chain's end, more room adds nothing and moves nothing.
        let end = key_chain(exp, 700, 704);
        let mut g = Frame {
            hold: true,
            ..Frame::default()
        };
        g.refresh(end.chain(exp), None, None, 4);
        let was = drawn(&g);
        g.refresh(end.chain(exp), None, None, 30);
        assert_eq!(drawn(&g)[..was.len()], was[..]);
        assert_eq!(
            g.rows.len(),
            4,
            "held from 701: to the chain's own end, blank below"
        );
        // Let go, it goes back between the strikes it falls between.
        f.hold = false;
        f.refresh(after.chain(exp), Some(Price(68_650)), sel, 21);
        assert_ne!(f.line_at, line);
    }

    #[test]
    fn the_strip_holds_at_least_one_order() {
        assert_eq!(strip_orders_fit(340.0, true, true, 3), 1);
        assert_eq!(strip_orders_fit(340.0, true, false, 3), 2);
        assert_eq!(strip_orders_fit(1600.0, false, true, 3), 3);
        assert_eq!(strip_orders_fit(1000.0, false, true, 9), 4);
        assert_eq!(strip_orders_fit(100.0, true, true, 2), 1);
        assert_eq!(strip_orders_fit(1000.0, false, false, 0), 0);
    }

    #[test]
    fn the_quote_cells_are_where_clicks_land() {
        let q = Quotes {
            bid: (100.0, 160.0),
            ask: (160.0, 220.0),
        };
        assert_eq!(q.at(100.0), Some(Cell::Bid));
        assert_eq!(q.at(159.9), Some(Cell::Bid));
        assert_eq!(q.at(160.0), Some(Cell::Ask));
        assert_eq!(q.at(219.9), Some(Cell::Ask));
        assert_eq!(q.at(99.9), None);
        assert_eq!(q.at(220.0), None);
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
