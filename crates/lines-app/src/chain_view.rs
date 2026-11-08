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
//!   the selected cell's bid and ask, Enter opens the ticket on it, Delete cancels the selected
//!   series' first working order. Enter always opens the ticket, and a held Enter sends nothing.
//!   Clicking a cell selects its
//!   strike and side; clicking an expiry chip selects it. Typing a symbol (or Ctrl+K) opens the
//!   symbol picker, as in the other panes.
//! - **The ticket** (decision 0028, `chain_ticket.rs`): a click on an ask buys and on a bid
//!   sells, docked at the bottom. Up/Down move the price by tick, Shift+Up/Down the quantity,
//!   digits type a price, Tab closes the position instead, Enter sends, Esc leaves. What the
//!   guard would refuse says so there, in the core's words, and Enter sends nothing.
//! - **Positions and working orders** sit on their strike rows, in a Pos column at each outer
//!   edge, with Close and Cancel; a narrow pane leaves the column out and marks the strike. The
//!   selected series lists all of its own above the ticket, at any width. An expiry holding any
//!   carries a dot on its chip.
//! - **One-click** (off in every new pane, never saved, off again on another symbol or account):
//!   a single click in the focused pane sends a limit at the clicked side, still under the guard,
//!   for the size the switch shows (Shift+Up/Down, or the ticket's chips). The rows stay put under
//!   the pointer while the selected strike is in view.
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
use lines_core::{
    CivilDate, ContractKey, FixedStr, OptionChain, OptionKey, OptionQuote, Price, Product, Right,
};
use lines_feed::catalog::Target;
use lines_feed::store::ChartStore;
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
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 8.0;
/// The CALLS / PUTS row and the column titles.
const HEAD_H: f32 = 2.0 * ROW_H;
/// Expiry chips in the header.
const MAX_EXPIRIES: usize = 8;
/// The widest thing the Pos column's text shows.
const POS_SAMPLE: &str = "S 10 @ 13.10 + 9 ord";
/// Below this the header leaves out its title, so the account and One-click stay.
const NARROW_HEADER: f32 = 560.0;
/// How long to wait for the core's answer before saying there was none.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
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
    /// Commands waiting for the core's answer, with when they went.
    asked: Vec<(CommandId, std::time::Instant)>,
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
        let was = self.first_row..self.first_row + self.rows.len();
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
        let fit_rows = fit.saturating_sub(1).clamp(1, MAX_ROWS);
        let selected_at = selected.and_then(|s| chain.find(s).ok());
        let range = window_held(chain.rows.len(), fit_rows, center, selected_at, was);
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

    /// The account. No view state: an expiry passes, and the view starts on the money.
    fn save_state(&self, doc: &mut PaneDoc) {
        doc.trade = self.trade.doc();
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
        self.selected = None;
        // A ticket or a One-click press belongs to the symbol it was opened on.
        self.leave_ticket();
        self.one_click = false;
        self.note = None;
        self.trade.feed_changed(&self.store, follow, cx);
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
            sent_at: None,
            watching: Vec::new(),
            shot,
            glyphs: Glyphs::new(TEXT_PX),
            bounds: Bounds::default(),
            layout: Layout::default(),
            frame: Frame {
                rows: Vec::with_capacity(MAX_ROWS),
                marks: Vec::with_capacity(64),
                ..Frame::default()
            },
        }
    }

    /// Taking typed text now (the symbol picker, the ticket's keys).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open() || self.ticket.is_some()
    }

    /// The pane's saved account, as the other trading panes restore it.
    pub fn restore_trade(&mut self, doc: Option<&TradeDoc>, cx: &mut Context<Self>) {
        self.trade = PaneTrade::new(doc, cx);
    }

    /// The One-click switch.
    pub fn toggle_one_click(&mut self, cx: &mut Context<Self>) {
        self.one_click = !self.one_click;
        cx.notify();
    }

    /// The next account: a ticket on the last one doesn't carry over.
    fn next_account(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = Trading::try_global(cx) {
            let next = t.next_account(self.trade.account.as_ref());
            self.trade.account = next;
            self.leave_ticket();
            self.one_click = false;
            self.note = None;
            self.seen_revision = u64::MAX;
            cx.notify();
        }
    }

    /// Close the ticket; an order on its way still gets its answer shown (in the footer).
    fn leave_ticket(&mut self) {
        if let Some(id) = self.ticket.take().and_then(|t| t.sent) {
            self.asked.push((id, std::time::Instant::now()));
        }
        self.sent_at = None;
    }

    /// The market or trading state is mid-update this instant: say so rather than act on a guess.
    fn busy(&mut self) -> bool {
        let busy = self.store.try_read().is_none() || self.trade.try_cache().is_none();
        if busy {
            self.note = Some("Market data busy; try again".to_owned());
        }
        busy
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
        let read_only = t.read_only_reason(id);
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
                now: t.now(&self.store),
                prop: info.as_ref().is_some_and(|i| i.rules.is_some()),
                closed: t.now(&self.store).is_some_and(|n| {
                    !lines_trading::paper::option_session_open(self.product.id, n)
                }),
                read_only: read_only.as_deref(),
            },
            price,
            close,
        )
    }

    fn submit(&self, cmd: lines_trading::Command, cx: &App) -> Option<CommandId> {
        Trading::try_global(cx)
            .map(|t| t.submit(lines_trading::Source::Dom, std::time::Instant::now(), cmd))
    }

    /// Open the ticket on a cell of a series.
    fn open_ticket(&mut self, key: OptionKey, cell: Cell) {
        let Some(rules) = self.rules() else {
            return;
        };
        self.note = None;
        if self.busy() {
            return;
        }
        self.leave_ticket();
        let q = self.quote(key);
        self.ticket = Some(Ticket::new(key, cell.side(), q.as_ref(), rules));
    }

    /// A click on a quote cell with One-click on: a limit at that side, at once.
    fn send_click(&mut self, key: OptionKey, cell: Cell, cx: &mut Context<Self>) {
        self.note = None;
        if self.busy() {
            return;
        }
        {
            self.leave_ticket();
            let q = self.quote(key);
            let price = ticket::click_limit(cell, q.as_ref());
            if let Some(why) = self.refusal_of(key, price, false, cx) {
                self.note = Some(format!("Not sent: {why}"));
                return;
            }
            let (Some(price), Some(account)) = (price, self.trade.account.clone()) else {
                return;
            };
            let qty = self.size(cx);
            let cmd = ticket::command(account, self.contract(key), cell.side(), qty, price, false);
            self.ask(cmd, cx);
        }
    }

    fn ask(&mut self, cmd: lines_trading::Command, cx: &App) {
        if let Some(id) = self.submit(cmd, cx) {
            self.asked.push((id, std::time::Instant::now()));
        }
    }

    /// Enter on the ticket: send what the button says, once.
    fn send_ticket(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.ticket.as_ref() else {
            return;
        };
        if t.sent.is_some() {
            return;
        }
        let (key, side, close, price) = (t.key, t.side, t.close, t.price);
        if self.refusal_of(key, price, close, cx).is_some() {
            return; // already on the ticket, in the core's words
        }
        let (Some(price), Some(account)) = (price, self.trade.account.clone()) else {
            return;
        };
        if close && self.closing(&account, key) {
            if let Some(t) = self.ticket.as_mut() {
                t.refused = Some("a close is already working; Cancel it first".to_owned());
            }
            return;
        }
        let qty = if close {
            match ticket::closes(self.position(key), side) {
                Some(q) => q,
                None => {
                    if let Some(t) = self.ticket.as_mut() {
                        t.refused = Some("no position to close".to_owned());
                    }
                    return;
                }
            }
        } else {
            self.size(cx)
        };
        let cmd = ticket::command(account, self.contract(key), side, qty, price, close);
        let id = self.submit(cmd, cx);
        self.sent_at = Some(std::time::Instant::now());
        if let Some(t) = self.ticket.as_mut() {
            t.sent = id;
            t.refused = None;
        }
    }

    /// The row's Close: reduce-only, the whole position, at the mid.
    fn press_close(&mut self, key: OptionKey, cx: &mut Context<Self>) {
        let (Some(rules), Some(account)) = (self.rules(), self.trade.account.clone()) else {
            return;
        };
        self.note = None;
        if self.busy() {
            return;
        }
        let position = self.position(key);
        // A close already working: another would only sit behind it, so it says so.
        if self.closing(&account, key) {
            self.note = Some("Not sent: a close is already working; Cancel it first".to_owned());
            return;
        }
        let q = self.quote(key);
        match ticket::close_command(account, self.contract(key), position, q.as_ref(), rules) {
            Some(cmd) => match self.refusal_of(key, order_price(&cmd), true, cx) {
                Some(why) => self.note = Some(format!("Not sent: {why}")),
                None => self.ask(cmd, cx),
            },
            None if position != 0 => {
                let why = self
                    .refusal_of(key, None, true, cx)
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
        asked.retain(|(id, at)| match st.result(*id) {
            Some(Err(e)) => {
                note = Some(format!("Refused: {}", e.message));
                false
            }
            Some(Ok(ids)) => {
                accepted.extend_from_slice(ids);
                false
            }
            // The answers kept are finite: an old one that never came stops being waited for.
            None => at.elapsed() < ANSWER_WAIT,
        });
        let mut changed = asked.len() != before;
        if let Some(Ok(ids)) = &answer {
            accepted.extend_from_slice(ids);
        }
        self.watching.extend(accepted);
        // Accepted isn't filled: the broker can still reject an order on arrival.
        self.watching
            .retain(|id| match st.order(*id).map(|o| &o.state) {
                Some(lines_trading::OrderState::Rejected { reason }) => {
                    note = Some(format!("Rejected: {reason}"));
                    false
                }
                Some(lines_trading::OrderState::Submitted) | None => true,
                Some(_) => false,
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
            None => {
                if self.sent_at.is_some_and(|at| at.elapsed() >= ANSWER_WAIT) {
                    if let Some(t) = self.ticket.as_mut() {
                        t.sent = None;
                        t.refused = Some("no answer from the trading core".to_owned());
                    }
                    self.sent_at = None;
                    changed = true;
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
        match k.key.as_str() {
            "escape" => self.leave_ticket(),
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
                if let Some(t) = self.ticket.as_mut() {
                    t.step(k.key == "up", rules);
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
        let q = self.quote(key);
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
        let expiry = match self.store.try_read() {
            Some(d) => d.chains.expiries().next(),
            None => return,
        };
        let Some(expiry) = expiry else {
            return;
        };
        self.shot = None;
        self.one_click = shot.one_click;
        let Some((cell, strike, right)) = shot.open else {
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
        self.selected = Some(key.strike);
        self.side = right;
        self.cell = cell;
        self.open_ticket(key, cell);
        if shot.close {
            self.toggle_close(cx);
        }
        if let Some(at) = shot.at
            && let Ok(units) = self.product.price.scale.parse_decimal(&at)
            && let Some(t) = self.ticket.as_mut()
        {
            t.set_price(Price(units));
        }
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
            drop(d);
            self.expiry = Some(all[j]);
            self.selected = None; // that strike may not exist there: back to the money
            self.leave_ticket();
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
            "up" | "down" if m.shift => {
                let q = crate::dom_trade::step_qty(self.size(cx), k.key == "up");
                self.set_size(q, cx);
            }
            "up" => self.move_strike(-1),
            "down" => self.move_strike(1),
            "pageup" => self.move_strike(-(self.page.max(1) as i64)),
            "pagedown" => self.move_strike(self.page.max(1) as i64),
            "home" => self.selected = None,
            "left" => self.step_expiry(-1),
            "right" => self.step_expiry(1),
            "tab" if m.shift => self.cell = self.cell.other(),
            "delete" => {
                // The selected series' first working order: pressed again, the next.
                let first = self.selected_key().and_then(|key| {
                    let a = self.trade.account.as_ref()?;
                    let st = self.trade.try_cache()?;
                    st.open_orders(a, self.contract(key)).next().map(|o| o.id)
                });
                if let Some(id) = first {
                    self.press_cancel(id, cx);
                }
            }
            "tab" => {
                self.side = match self.side {
                    Right::Call => Right::Put,
                    Right::Put => Right::Call,
                }
            }
            // A held key never opens or sends: one press, one ticket.
            "enter" if ev.is_held => {}
            "enter" => match self.selected_key() {
                Some(key) => self.open_ticket(key, self.cell),
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
        // The click that only brings the pane forward aims at nothing: One-click waits for the next.
        let focused = self.focus.is_focused(window);
        self.focus.focus(window, cx);
        // A click that closes the picker does only that: never a press on what's under it.
        if self.picker.close() {
            cx.notify();
            return;
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
        let Some(ri) = row_at(i, self.layout.first_row, self.layout.line_at) else {
            return;
        };
        // The row as painted: a series arriving since can't shift the press onto another strike.
        let Some(strike) = ri
            .checked_sub(self.frame.first_row)
            .and_then(|n| self.frame.rows.get(n))
            .map(|r| r.strike)
        else {
            return;
        };
        let Some(expiry) = self.frame.expiry else {
            return;
        };
        self.selected = Some(strike);
        let right = if x < self.layout.strike_x.0 {
            self.side = Right::Call;
            Some(Right::Call)
        } else if x > self.layout.strike_x.1 {
            self.side = Right::Put;
            Some(Right::Put)
        } else {
            None
        };
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
                } else if !self.one_click {
                    self.open_ticket(key, cell);
                } else if !focused {
                    self.note = Some("Pane focused: click again to send".to_owned());
                }
            } else if let Some(a) = self.layout.actions.map(|a| a[side])
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
                self.frame.expiry = self.shown_expiry(&d.chains);
                let chain = self.frame.expiry.and_then(|e| d.chains.chain(e));
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
        // The Pos column: fixed, so the columns never shift under the pointer when an order
        // lands. It takes the room of the columns that only inform, but a pane too narrow for
        // it and the quotes leaves it out (the strike is marked, the selected row lists them).
        let action_w = self.glyphs.width("Cancel", t.text_main, false, window) + PAD;
        let pos_full =
            self.glyphs.width(POS_SAMPLE, t.text_main, false, window) + action_w + 2.0 * PAD;
        let pos_w = if w >= strike_w + 2.0 * (cols[5] + cols[6] + pos_full) {
            pos_full
        } else {
            0.0
        };
        let shown = fit_columns(w - 2.0 * pos_w, strike_w, cols);
        let table_w = strike_w
            + 2.0
                * (pos_w
                    + cols
                        .iter()
                        .zip(&shown)
                        .filter(|(_, s)| **s)
                        .map(|(c, _)| c)
                        .sum::<f32>());
        let x0 = ox + ((w - table_w) / 2.0).max(0.0);
        if let (Some(a), Some(st)) = (self.trade.account.as_ref(), self.trade.try_cache()) {
            self.frame.refresh_marks(&st, a, self.product.id);
        }
        // Calls: left to right in CALLS order; strike; puts: CALLS reversed.
        let mut call_x = [0.0f32; 7];
        let mut x = x0 + pos_w;
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
        let put_pos_x = x;
        x += pos_w;
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
        if pos_w > 0.0 {
            self.glyphs.paint(
                "Pos",
                x0 + PAD,
                title_y,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            self.glyphs.paint(
                "Pos",
                put_pos_x + PAD,
                title_y,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
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
            actions: (pos_w > 0.0).then(|| [action_at(x0), action_at(put_pos_x)]),
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
            // What the account has here: in the Pos column, else a mark beside the strike.
            for (right, cell_x, dot_x) in [
                (Right::Call, x0, strike_x + 1.0),
                (Right::Put, put_pos_x, strike_x + strike_w - PAD + 1.0),
            ] {
                let Some(m) = self.frame.mark(row.strike, right).copied() else {
                    continue;
                };
                if pos_w <= 0.0 {
                    self.glyphs
                        .paint("•", dot_x, y, ROW_H, t.text_main, true, window, cx);
                    continue;
                }
                let mut pos = FixedStr::<48>::new();
                if m.position != 0 {
                    let _ = ticket::write_position(&mut pos, &self.product, m.position, m.avg);
                } else if let Some((_, side, open, price)) = m.order {
                    let _ = ticket::write_order(&mut pos, &self.product, side, open, price);
                }
                let extra = if m.position != 0 {
                    m.orders
                } else {
                    m.orders.saturating_sub(1)
                };
                if extra > 0 {
                    let _ = std::fmt::Write::write_fmt(&mut pos, format_args!(" + {extra} ord"));
                }
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
        // Narrow: the price stays on its row, so the expiry and feed word fit whole.
        let narrow = f(self.bounds.size.width) < NARROW_HEADER;
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
        if let Some(e) = shown {
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
                            this.selected = None;
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
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
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
                    .child(
                        div()
                            .id("chain-one-click")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .px_1()
                            .border_1()
                            .rounded_sm()
                            .border_color(if one_click { t.accent } else { t.control_edge })
                            .text_color(t.text_main)
                            .when(one_click, |d| d.font_weight(gpui::FontWeight::BOLD))
                            .child(if one_click {
                                format!("One-click: ON · {size}")
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

    /// Expiries where the account holds or works something.
    fn held_expiries(&self) -> Vec<CivilDate> {
        let mut out = Vec::new();
        let (Some(a), Some(st)) = (self.trade.account.as_ref(), self.trade.try_cache()) else {
            return out;
        };
        let contracts = st
            .positions_of(a)
            .map(|p| p.contract)
            .chain(st.open_orders_of(a).map(|o| o.contract));
        for c in contracts {
            if c.product == self.product.id
                && let Some(k) = c.option_key()
                && !out.contains(&k.expiry)
            {
                out.push(k.expiry);
            }
        }
        out
    }

    /// The selected series' position and orders with Close and Cancel: at any width, and with
    /// every order when the row's cell has room for one.
    fn row_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let t = self.theme;
        let key = self.selected_key()?;
        let account = self.trade.account.as_ref()?;
        let contract = self.contract(key);
        let (position, orders) = {
            let st = self.trade.try_cache()?;
            let position = st.position(account, contract).map(|p| (p.qty, p.avg_price));
            let orders: Vec<(OrderId, Side, i64, Option<Price>)> = st
                .open_orders(account, contract)
                .map(|o| (o.id, o.side, o.qty.0 - o.filled.0, o.kind.prices().next()))
                .collect();
            (position, orders)
        };
        if position.is_none() && orders.is_empty() {
            return None;
        }
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
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .text_color(t.text_main)
            .child(ticket::contract_name(
                &self.product,
                key,
                &chip_date(key.expiry),
            ));
        if let Some((qty, avg)) = position {
            let mut s = String::new();
            let _ = ticket::write_position(&mut s, &self.product, qty, avg);
            row = row
                .child(s)
                .child(button("chain-close".into(), "Close").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.press_close(key, cx);
                        cx.notify();
                    }),
                ));
        }
        for (id, side, open, price) in orders {
            let mut s = String::new();
            let _ = ticket::write_order(&mut s, &self.product, side, open, price);
            row = row.child(s).child(
                button(format!("chain-cancel-{}", id.0).into(), "Cancel").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.press_cancel(id, cx);
                        cx.notify();
                    }),
                ),
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
        let q = self.quote(tk.key);
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
        let live = if tk.close && closable.is_none() {
            Some("no position to close".to_owned())
        } else {
            self.refusal_of(tk.key, tk.price, tk.close, cx)
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
                .flex()
                .items_center()
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
                    Side::Buy => "Cost",
                    Side::Sell => "Credit",
                };
                format!("{word} {}", trade_bar::money_grouped(m))
            });
        let send = ticket::send_label(&product, tk.side, tk.close, qty, tk.price);
        let side_color = match tk.side {
            Side::Buy => t.up,
            Side::Sell => t.down,
        };
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
            }));
        let mut controls = div().flex().flex_row().flex_wrap().items_center().gap_2();
        if let Some(chips) = qty_chips {
            controls = controls.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .child(div().text_color(t.text_dim).child("Qty"))
                    .children(chips)
                    .children(custom),
            );
        }
        let controls = controls
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .child(div().text_color(t.text_dim).child("Limit"))
                    .children(price_chips),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .min_w(px(56.0))
                    .h(px(MIN_HIT))
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .border_1()
                    .rounded_sm()
                    .border_color(t.text_dim)
                    .text_color(t.text_main)
                    .child(price_text),
            )
            .children(premium.map(|c| div().flex_shrink_0().text_color(t.text_main).child(c)))
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
                    .child(format!("{send} ⏎"))
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
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_shrink_0()
                        .px_1()
                        .bg(t.warn)
                        .text_color(t.on_warn)
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(word),
                )
                .child(div().text_color(t.text_main).child(text))
        };
        let line = match (reason, stale) {
            (Some(why), _) => flagged("Can't send", why),
            (None, true) => flagged(
                "Stale feed",
                "no new data: this quote may be old".to_owned(),
            ),
            (None, false) => div()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_color(t.text_dim)
                .child("↑↓ price by tick  ·  Shift ↑↓ quantity  ·  Tab close  ·  Enter send  ·  Esc cancel"),
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
                .into_any_element(),
        )
    }

    fn footer(&self, size: i64) -> impl IntoElement {
        let t = self.theme;
        div()
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            // A reason wraps, whole; the hints give way at the edge.
            .when(self.note.is_none(), |d| d.whitespace_nowrap().overflow_hidden())
            .text_color(if self.note.is_some() {
                t.text_main
            } else {
                t.text_dim
            })
            .child(self.note.clone().unwrap_or_else(|| {
                if self.one_click {
                    return format!(
                        "One-click ON: a click on an ask or bid sends a limit there for {size}, at once  ·  Shift+Up/Down: quantity  ·  Enter: ticket"
                    );
                }
                "Click an ask to buy, a bid to sell, or Enter  ·  Up/Down: strike  ·  Left/Right: expiry  ·  Tab: calls / puts  ·  Shift+Tab: bid / ask  ·  Delete: cancel  ·  Home: the money  ·  Shaded: in the money".to_owned()
            }))
    }
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let t = self.theme;
        self.apply_shot(cx);
        let size = self.size(cx);
        let strip = self.row_strip(cx);
        let dock = self.ticket_dock(cx);
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
            .children(strip)
            .children(dock)
            .child(self.footer(size))
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
        // Fewer rows, the selection now below them: around it.
        assert_eq!(window_held(41, 3, 14, Some(14), 10..20), 13..16);
        // A shorter chain.
        assert_eq!(window_held(5, 10, 2, Some(2), 3..8), 0..5);
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
