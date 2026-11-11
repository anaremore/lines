//! Chart order lines (roadmap 8, phase 1), without GPUI: what a chart's order tag says, what
//! dragging a position handle places, what an edge marker says, and the chart's per-frame copy
//! of its account's orders. The chart view measures and draws;
//! `lines_core::geometry::order_tags` lays out and hit-tests.
//!
//! - **Tags**: role and what is left (`S1`, `T1`, `BUY LMT 2`), the price, and for an exit the
//!   P&L if it fills there, as the P&L display shows it (`S1 · 5806.50 · -$175.00`).
//! - **Moving** an order (drag its tag) and **cancelling** it (×) need no arming, as on the DOM.
//! - **Adding** a stop or target (drag a position handle) is a new order, so it needs trading
//!   enabled; it is a reduce-only exit for the whole position (`Command::PlaceExit`), refused
//!   when it would fill or trigger at once.

use std::fmt::{self, Write};

use lines_core::Qty;
use lines_core::{ContractKey, FixedStr, Price, PriceFormat, Product};
use lines_trading::{AccountId, Command as TradeCommand, Money, OrderKind, OrderSpec, Side};

use crate::dom_trade::{MarkRole, MarkStyle, OrderMark, Refusal};
use crate::order_resize::ResizeHit;
use crate::pnl::PnlDisplay;

/// Shown when a handle is dragged with no linked DOM enabled (a chart shows no switch).
pub const ENABLE_TO_ADD: &str = "Enable trading on the linked DOM to add a stop or target";

/// A chart's resize upward with no linked DOM enabled.
pub const ENABLE_TO_GROW: &str = "Enable trading on the linked DOM to add to an order";

/// Whether a chart exit may go out: a DOM in the chart's link group (not unlinked), on the
/// same account and contract, has trading enabled. `doms`: (link, account, contract, enabled).
pub fn linked_dom_enabled<'a>(
    link: crate::layout::LinkGroup,
    account: &AccountId,
    contract: ContractKey,
    doms: impl IntoIterator<
        Item = (
            crate::layout::LinkGroup,
            Option<&'a AccountId>,
            Option<ContractKey>,
            bool,
        ),
    >,
) -> bool {
    link.is_some()
        && doms
            .into_iter()
            .any(|(l, a, k, on)| l == link && a == Some(account) && k == Some(contract) && on)
}

/// Money per (price unit × quantity unit), exact (1e-10 currency units), as the trading core
/// computes it; `None` if it isn't whole (no catalog product). Allocation-free.
pub fn money_per_unit(p: &Product) -> Option<i128> {
    let num = p.price.scale.num() as i128 * p.point_value.num as i128 * Money::UNITS_PER_WHOLE;
    let den =
        p.price.scale.den() as i128 * p.point_value.den as i128 * 10i128.pow(p.qty.decimals as u32);
    (den != 0 && num % den == 0).then(|| num / den)
}

/// The P&L of closing `qty` of a position (signed `position`, average `avg`) at `price`: ticks
/// per contract (profit positive) and money (`mpu`: money per price unit × quantity unit).
pub fn exit_pnl(
    position: i64,
    avg: f64,
    price: Price,
    qty: i64,
    tick: i64,
    mpu: i128,
) -> (f64, Money) {
    let dir = position.signum() as f64;
    let units = (price.0 as f64 - avg) * dir;
    let ticks = units / tick.max(1) as f64;
    let money = (units * qty as f64 * mpu as f64).round();
    (ticks, Money(money as i128))
}

/// An order's tag, quantity always separate from the role (a bare "S2" read as a short of
/// 2 on the DOM): `Stop 2 @ 5806.50`, `Target 1 @ 5820.00`, `Buy lmt 2 @ 5999.50`,
/// `Sell stp 1/2 · …` (partially filled), then ` · held` for a bracket exit waiting on its
/// entry. The P&L at its price is drawn after it, in its own tone. No allocation.
pub fn write_order_tag(out: &mut impl Write, m: &OrderMark, fmt: &PriceFormat) -> fmt::Result {
    let left = m.qty.0 - m.filled.0;
    match MarkRole::of(m) {
        MarkRole::Target => write!(out, "Target {left}")?,
        MarkRole::Stop => write!(out, "Stop {left}")?,
        MarkRole::Entry => {
            let side = if m.side == Side::Buy { "Buy" } else { "Sell" };
            let kind = if m.stop { "stp" } else { "lmt" };
            if m.style == MarkStyle::Partial {
                write!(out, "{side} {kind} {}/{}", m.filled.0, m.qty.0)?;
            } else {
                write!(out, "{side} {kind} {left}")?;
            }
        }
    }
    out.write_str(" @ ")?;
    fmt.write(m.price, out)?;
    if m.style == MarkStyle::Held {
        out.write_str(" · held")?;
    }
    Ok(())
}

/// A tag for several orders of one kind (`g`; `m`: any of them): `Target 200 ×20 @ ~5822.86
/// 5822.75–5823.00`, what is left and how many, the average (exact until here, rounded to a price
/// unit for this label only) and the range; one price needs neither. Orders of several kinds
/// read `Orders 200 ×20 @ 5820.00–5823.00`. No allocation.
pub fn write_group_tag(
    out: &mut impl Write,
    m: &OrderMark,
    g: &TagGroup,
    fmt: &PriceFormat,
) -> fmt::Result {
    let mixed = g.key == MIXED_KEY;
    if mixed {
        out.write_str("Orders")?;
    } else {
        match MarkRole::of(m) {
            MarkRole::Target => out.write_str("Target")?,
            MarkRole::Stop => out.write_str("Stop")?,
            MarkRole::Entry => {
                out.write_str(if m.side == Side::Buy { "Buy" } else { "Sell" })?;
                out.write_str(if m.stop { " stp" } else { " lmt" })?;
            }
        }
    }
    write!(out, " {} ×{} @ ", g.qty, g.n)?;
    if g.lo != g.hi && !mixed {
        out.write_char('~')?;
        fmt.write(g.average(), out)?;
        out.write_char(' ')?;
    }
    fmt.write(g.lo, out)?;
    if g.lo != g.hi {
        out.write_char('–')?;
        fmt.write(g.hi, out)?;
    }
    if !mixed && m.style == MarkStyle::Held {
        out.write_str(" · held")?;
    }
    Ok(())
}

/// One row of a group's list: the order as of the press, and whether it is still open. Rows never
/// move or leave while the list is open: what the trader aims at stays under the pointer.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupRow {
    pub mark: OrderMark,
    pub open: bool,
}

/// The list a group tag opened, its orders as of the press.
/// Cancel all takes two presses, the second for exactly the orders the first one counted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupMenu {
    pub rows: Vec<GroupRow>,
    /// The account they are on: cancels go there, and the list closes if the chart's changes.
    pub account: Option<AccountId>,
    /// `Target: 200 in 20 orders, 5822.75–5823.00`.
    pub title: String,
    /// Plot-local bottom of the tag pressed (the list opens under it).
    pub tag_bottom: f32,
    /// Cancel all pressed once, with this many orders open.
    pub confirm: Option<usize>,
}

impl GroupMenu {
    pub fn open_count(&self) -> usize {
        self.rows.iter().filter(|r| r.open).count()
    }

    /// Each row's state from the trading cache (`is_open`, a successful read): an order that
    /// finished keeps its row, inert. Any change disarms Cancel all, so the second press always
    /// answers the count on screen. Returns whether anything changed.
    pub fn refresh(&mut self, is_open: impl Fn(lines_trading::OrderId) -> bool) -> bool {
        let mut changed = false;
        for r in &mut self.rows {
            let open = is_open(r.mark.id);
            changed |= open != r.open;
            r.open = open;
        }
        if changed {
            self.confirm = None;
        }
        changed
    }

    /// Cancel all pressed: the first press arms and returns nothing, the second returns the
    /// orders to cancel.
    pub fn cancel_all(&mut self) -> Option<Vec<lines_trading::OrderId>> {
        let n = self.open_count();
        if n == 0 {
            return None;
        }
        if self.confirm == Some(n) {
            self.confirm = None;
            Some(
                self.rows
                    .iter()
                    .filter(|r| r.open)
                    .map(|r| r.mark.id)
                    .collect(),
            )
        } else {
            self.confirm = Some(n);
            None
        }
    }
}

/// The list's title: `Target: 200 in 20 orders, 5822.75–5823.00`. Says what `200 ×20` means.
pub fn write_group_title(
    out: &mut impl Write,
    m: &OrderMark,
    g: &TagGroup,
    fmt: &PriceFormat,
) -> fmt::Result {
    if g.key == MIXED_KEY {
        out.write_str("Orders")?;
    } else {
        match MarkRole::of(m) {
            MarkRole::Target => out.write_str("Target")?,
            MarkRole::Stop => out.write_str("Stop")?,
            MarkRole::Entry => {
                out.write_str(if m.side == Side::Buy { "Buy" } else { "Sell" })?;
                out.write_str(if m.stop { " stp" } else { " lmt" })?;
            }
        }
    }
    write!(out, ": {} in {} orders, ", g.qty, g.n)?;
    fmt.write(g.lo, out)?;
    if g.lo != g.hi {
        out.write_char('–')?;
        fmt.write(g.hi, out)?;
    }
    Ok(())
}

/// An exit's P&L at its price as the P&L display shows it (ticks, currency, both; nothing when
/// off or for an order that doesn't close the position), and its money (for the tone).
#[allow(clippy::too_many_arguments)]
pub fn write_exit_pnl(
    out: &mut impl Write,
    m: &OrderMark,
    position: Option<(i64, f64)>,
    tick: i64,
    mpu: Option<i128>,
    mode: PnlDisplay,
) -> Result<Option<Money>, fmt::Error> {
    let (Some((pos, avg)), Some(mpu)) = (position, mpu) else {
        return Ok(None);
    };
    // Only orders that close (part of) the position: the opposite side.
    if pos == 0 || (pos > 0) == (m.side == Side::Buy) {
        return Ok(None);
    }
    let qty = (m.qty.0 - m.filled.0).min(pos.abs());
    let (ticks, money) = exit_pnl(pos, avg, m.price, qty, tick, mpu);
    Ok(crate::pnl::write_pnl(out, mode, Some(ticks), money)?.then_some(money))
}

/// The edge marker for an order off the plot, as its tag reads: `↑ Target 1 @ 5820.00`, and
/// `+N` for more beyond that edge. No allocation.
pub fn write_edge(
    out: &mut impl Write,
    m: &OrderMark,
    above: bool,
    n: usize,
    fmt: &PriceFormat,
) -> fmt::Result {
    out.write_str(if above { "↑ " } else { "↓ " })?;
    write_order_tag(out, m, fmt)?;
    if n > 1 {
        write!(out, " +{} more", n - 1)?;
    }
    Ok(())
}

/// An edge marker in short, for the DOM's title cell: the role in a short word and the price
/// (`↑Tgt 5819.50`, `↓Stop 5795.00`, `↑Buy 5830.00`), no bare quantity digit beside a role.
pub fn write_edge_short(out: &mut impl Write, m: &OrderMark, above: bool) -> fmt::Result {
    out.write_str(if above { "↑" } else { "↓" })?;
    out.write_str(match MarkRole::of(m) {
        MarkRole::Target => "Tgt",
        MarkRole::Stop => "Stop",
        MarkRole::Entry if m.side == Side::Buy => "Buy",
        MarkRole::Entry => "Sell",
    })
}

/// [`write_edge_short`] with the order's price: `↑Tgt 5819.50`.
pub fn write_edge_priced(
    out: &mut impl Write,
    m: &OrderMark,
    above: bool,
    fmt: &PriceFormat,
) -> fmt::Result {
    write_edge_short(out, m, above)?;
    out.write_char(' ')?;
    fmt.write(m.price, out)
}

/// The exit a position handle places at `price`: a target (limit) or a stop for the whole
/// position, reduce-only (`PlaceExit`). Refused when it would fill or trigger at once, by the
/// DOM's rules against the inside market (`bid`, `ask`; else the last trade): a long's target
/// must be above the best bid and its stop below it; a short's target below the best ask and
/// its stop above it (equal counts as at once).
pub fn exit_command(
    account: &AccountId,
    contract: ContractKey,
    position: i64,
    target: bool,
    price: Price,
    (bid, ask, last): (Option<Price>, Option<Price>, Option<Price>),
) -> Result<TradeCommand, Refusal> {
    if position == 0 {
        return Err(Refusal("no position to protect"));
    }
    let long = position > 0;
    // A long exits by selling into the bid; a short by buying from the ask.
    let reference = if long { bid.or(last) } else { ask.or(last) };
    let market = reference.ok_or(Refusal("no market price yet"))?;
    let profit_side = if long { price > market } else { price < market };
    let loss_side = if long { price < market } else { price > market };
    if target && !profit_side {
        return Err(Refusal(if long {
            "a target must be above the bid"
        } else {
            "a target must be below the ask"
        }));
    }
    if !target && !loss_side {
        return Err(Refusal(if long {
            "a stop must be below the bid"
        } else {
            "a stop must be above the ask"
        }));
    }
    let kind = if target {
        OrderKind::Limit { price }
    } else {
        OrderKind::Stop { trigger: price }
    };
    Ok(TradeCommand::PlaceExit(OrderSpec {
        account: account.clone(),
        contract,
        side: if long { Side::Sell } else { Side::Buy },
        qty: Qty(position.abs()),
        kind,
    }))
}

// ---- the chart's per-frame trading state ------------------------------------------------

use lines_core::geometry::order_tags::{
    EdgeMarker, GroupIn, Handle, LineIn, MAX_TAGS, MIXED_KEY, TagBox, TagGroup,
};

use crate::dom_orders::DragOrder;
use crate::dom_trade::{Ghost, GhostOutcome, ghost_outcome, order_mark};
use crate::trading::TradeCache;

/// One tag's text and look, by tag id (order id).
#[derive(Clone, Copy)]
pub struct TagText {
    pub id: u64,
    /// `Stop 1 @ 5806.50`.
    pub text: FixedStr<96>,
    /// The P&L at its price (`-20.0 t  -$250.00`), drawn in its tone; empty: none.
    pub pnl: FixedStr<48>,
    pub money: Option<Money>,
    pub mark: OrderMark,
}

/// A position handle being dragged out: a target or a stop, at the pointer's snapped price.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AddExit {
    pub target: bool,
    pub to: Price,
}

/// What the chart draws of its account's orders this frame, copied out of the trading cache
/// (`try_lock`: a busy pump keeps the last frame's copy) into buffers allocated once: paint
/// never allocates here.
pub struct ChartTradeState {
    pub marks: Vec<OrderMark>,
    pub texts: Vec<TagText>,
    /// Every order, for the edge markers.
    pub lines: Vec<LineIn>,
    /// The orders on the plot, sorted by kind and price, and the tags they make.
    pub on_plot: Vec<GroupIn>,
    pub groups: [TagGroup; MAX_TAGS],
    pub n_groups: usize,
    /// One line per tag, at its order's price or a group's average.
    pub tag_lines: Vec<LineIn>,
    /// The list a group tag opened.
    pub group_menu: Option<GroupMenu>,
    pub tags: [TagBox; MAX_TAGS],
    pub n_tags: usize,
    pub markers: [Option<EdgeMarker>; 2],
    pub handles: [Option<Handle>; 2],
    /// The position: signed qty, average price.
    pub position: Option<(i64, f64)>,
    /// Drags waiting for the broker, drawn at their new price.
    pub ghosts: Vec<Ghost>,
    pub drag: Option<DragOrder>,
    pub adding: Option<AddExit>,
    /// A press on a tag's ×: cancels on release if still there.
    pub closing: Option<u64>,
    /// The single-order tag under the pointer, or whose − / size / + is, and whether its + can
    /// do anything.
    pub resize_hover: Option<u64>,
    pub resize_grows: bool,
    /// A tag drag's grab offset (order price − the price under the press), so the order moves
    /// by how far the pointer moved, never to the pointer's price (tags may sit off their line).
    pub grab: i64,
    /// Where a handle drag started: releasing there (a click, no drag) places nothing.
    pub add_from: Option<Price>,
    /// The position tag as last drawn (plot-local): pressing it starts a position-line drag.
    pub pos_tag: Option<TagBox>,
    /// The position line is being dragged (phase 2): `adding` follows the pointer's side.
    pub pos_drag: bool,
    /// Where an add drag (handle or position line) was pressed, plot-local y, and the farthest
    /// the pointer has moved from it (px): under [`DRAG_PX`] it was a click.
    pub press_y: f32,
    pub moved: f32,
    /// A strip limit button pressed: the next click on the plot is its price (the crosshair's
    /// there). Esc, the same button again, or another symbol drops it.
    pub strip_pick: Option<StripOrder>,
    /// The order the menu's "…with bracket" staged on the chart, unsent (one per chart).
    pub staged: Option<Staged>,
    /// The staged part being dragged (a body, or a handle pulling out a leg), its grab offset
    /// (the part's price − the price under the press), and the order as it was (Esc, or a
    /// release off the plot, puts it back).
    pub staged_drag: Option<(StagedPart, i64, Staged)>,
    /// A press on the staged entry's Place or a tag's ×: acts on release if still there, as a
    /// working tag's × does.
    pub staged_press: Option<StagedHit>,
    /// Place was pressed and the order went to the window: kept until the window answers, so a
    /// refusal there (no linked DOM enabled) puts it back on the chart.
    pub placing: Option<Staged>,
    /// The staged entry's handles as last drawn (plot-local).
    pub staged_handles: [Option<Handle>; 2],
    /// The staged entry's Place and × Esc widths as last drawn.
    pub staged_buttons: (f32, f32),
    /// A discard notice for a chart that was hidden: shown once it is visible again.
    pub notice: Option<&'static str>,
}

/// The resize control left of a tag: −, the size, +, sized by the tag's height (so by the UI
/// zoom): the buttons square, the size twice as wide (20, 40 and 20 px at 100 %).
/// `(button, value)` widths.
pub fn resize_parts(b: &TagBox) -> (f32, f32) {
    (b.h, 2.0 * b.h)
}

/// The whole resize control's width for tag `b`.
pub fn resize_w(b: &TagBox) -> f32 {
    let (btn, value) = resize_parts(b);
    2.0 * btn + value
}

/// The piece of the resize control of tag `b` at plot-local (`x`, `y`); `None` off it, or when
/// the tag sits too near the plot's left edge to have one.
pub fn resize_hit(b: &TagBox, x: f32, y: f32) -> Option<ResizeHit> {
    let (btn, value) = resize_parts(b);
    let left = b.left - resize_w(b);
    if left < 0.0 || y < b.top || y >= b.top + b.h || x < left || x >= b.left {
        return None;
    }
    Some(if x < left + btn {
        ResizeHit::Down
    } else if x < left + btn + value {
        ResizeHit::Value
    } else {
        ResizeHit::Up
    })
}

/// An add drag shorter than this (px) is a click and places nothing.
pub const DRAG_PX: f32 = 4.0;

/// What releasing an add drag (a "+T"/"+S" handle or the position line) does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Release {
    /// A click, or released where no exit side applies (at the average).
    Nothing,
    /// No exit of that kind yet: place one (`PlaceExit`).
    Place(AddExit),
    /// The position already has that exit: move it there (the DOM's modify path), never a
    /// second, unpaired one.
    Move {
        order: lines_trading::OrderId,
        to: Price,
    },
}

/// The release decision, pure: `adding` (the previewed exit, if any), how far the pointer moved
/// (px), and the position's existing exit of that kind.
pub fn release_action(
    adding: Option<AddExit>,
    moved: f32,
    existing: impl Fn(bool) -> Option<lines_trading::OrderId>,
) -> Release {
    let Some(a) = adding else {
        return Release::Nothing;
    };
    if moved < DRAG_PX {
        return Release::Nothing;
    }
    match existing(a.target) {
        Some(order) => Release::Move { order, to: a.to },
        None => Release::Place(a),
    }
}

// ---- order strip (phase 2b) ----------------------------------------------------------------

/// A button on the chart's order strip.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StripOrder {
    BuyMarket,
    SellMarket,
    /// A buy limit at the price clicked next (the crosshair's).
    BuyLimit,
    /// A sell limit at the price clicked next.
    SellLimit,
}

/// A strip limit picked where the chart has no price (no bars).
pub const PICK_A_PRICE: &str = "No price there: click a price on the chart's plot";
/// A strip order from a chart in no link group (no DOM's switch can send it).
pub const LINK_TO_TRADE: &str = "Link this chart to a DOM to place orders from it";
/// A strip order with no linked DOM enabled.
pub const ENABLE_TO_ORDER: &str = "Enable trading on the linked DOM to place orders from the chart";

/// Whether a press on a strip button acts. A market button sends at once, so never on a
/// double-click's second: one double-click on BUY MKT is one market order, not two (and two
/// brackets with a bracket on). `popover::second_press` isn't enough here: it lets a second press
/// on the same spot through. So two quick presses within the system's double-click time are one
/// order; after a pause, each is its own. A limit button only arms a pick and a press on it again
/// disarms it, so it always acts: a quick change of mind must not leave a pick armed.
pub fn strip_press_acts(o: StripOrder, click_count: usize) -> bool {
    match o {
        StripOrder::BuyMarket | StripOrder::SellMarket => click_count <= 1,
        StripOrder::BuyLimit | StripOrder::SellLimit => true,
    }
}

/// The command for a strip button: the DOM's own intents (market as its BUY/SELL MKT, a limit
/// as a click in its bid/ask column at that price), with the chart's quantity and bracket. A
/// chart shows no switch: the window sends this only when a linked DOM has trading enabled,
/// so it is built here as if enabled and the switch is checked there (`linked_dom_enabled`).
pub fn strip_command(
    o: StripOrder,
    at: Option<Price>,
    ctx: &crate::dom_trade::EntryCtx,
) -> Result<TradeCommand, Refusal> {
    use crate::dom_trade::{Arming, DomColumn, QuickOrder, click, quick};
    let on = Arming::new(true);
    match o {
        StripOrder::BuyMarket => quick(QuickOrder::BuyMarket, on, ctx),
        StripOrder::SellMarket => quick(QuickOrder::SellMarket, on, ctx),
        StripOrder::BuyLimit | StripOrder::SellLimit => {
            let p = at.ok_or(Refusal(PICK_A_PRICE))?;
            let col = if o == StripOrder::BuyLimit {
                DomColumn::Bid
            } else {
                DomColumn::Ask
            };
            click(col, p, false, on, ctx)
        }
    }
}

/// An order from the chart's right-click menu: a limit or a stop at a price.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MenuOrder {
    pub side: Side,
    pub stop: bool,
    pub price: Price,
}

/// What a menu row was offered with: its contract, size and bracket. A row chosen after any of
/// them changed (a linked pane switched the chart, the size or the bracket was changed while
/// the menu was open) is refused, never sent with what it didn't show (nor an entry without the
/// protection it named).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub contract: ContractKey,
    pub qty: Qty,
    pub bracket: Option<crate::dom_trade::BracketPreset>,
    /// The account's fingerprint ([`account_key`]): commands are `Copy`, an account id isn't.
    pub account: u64,
}

impl Offer {
    /// What a row built from `ctx` shows: its contract, size, bracket and account.
    pub fn of(ctx: &crate::dom_trade::EntryCtx) -> Offer {
        Offer {
            contract: ctx.contract,
            qty: ctx.qty,
            bracket: ctx.bracket,
            account: account_key(&ctx.account),
        }
    }
}

/// An account's fingerprint for an [`Offer`]: the same id always gives the same key in a run.
pub fn account_key(a: &AccountId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.hash(&mut h);
    h.finish()
}

/// Whether a menu row's account is still the chart's (the chart remembers the one it was
/// offered on).
pub fn same_account(offered: Option<&AccountId>, now: Option<&AccountId>) -> bool {
    offered.is_some() && offered == now
}

/// The menu's dim row when the chart can't trade: no linked DOM has trading enabled.
pub const TRADING_OFF_ROW: &str = "Trading is off · turn on in the trade panel";
/// The menu's dim row with no market to place against (no feed, a stale one, no quote).
pub const NO_PRICE_ROW: &str = "No live price";
/// A menu order chosen after the market moved through its price (it would fill at once).
pub const MARKET_MOVED: &str = "The market moved: right-click again";
/// A menu order chosen after its chart moved to another contract, account or size.
pub const CHART_CHANGED: &str = "The chart changed: right-click again";
/// Place pressed on a staged entry that would now fill or trigger at once, whether the market
/// moved or the trader dragged it there: it stays staged, to drag back.
pub const STAGED_CROSSES: &str = "The entry would fill or trigger now: drag it back";

/// The two orders a right click at `at` offers, never one that fills at once. The market is
/// the inside (bid and ask, either falling back to the last trade, as the trade panel does).
/// - Below the bid: a buy limit and a sell stop at `at`.
/// - Above the ask: a sell limit and a buy stop at `at`.
/// - At the inside (from the bid to the ask, both included): a buy limit at the bid and a sell
///   limit at the ask. Both rest; a stop there would trigger at once, and a limit at `at` would
///   cross whenever `at` is the other side.
///
/// A row that would fill at once in a one-sided or crossed book is left out (the same test as
/// when it is chosen, [`crosses`]). `None`: no market price.
pub fn menu_orders(
    at: Price,
    bid: Option<Price>,
    ask: Option<Price>,
    last: Option<Price>,
) -> Option<Vec<MenuOrder>> {
    // The inside, low to high (a one-sided book's last trade can fall either side of it).
    let (b, a) = (bid.or(last)?, ask.or(last)?);
    let (lo, hi) = (b.min(a), b.max(a));
    let o = |side, stop, price| MenuOrder { side, stop, price };
    let two = if at.0 < lo.0 {
        [o(Side::Buy, false, at), o(Side::Sell, true, at)]
    } else if at.0 > hi.0 {
        [o(Side::Sell, false, at), o(Side::Buy, true, at)]
    } else {
        [o(Side::Buy, false, lo), o(Side::Sell, false, hi)]
    };
    Some(
        two.into_iter()
            .filter(|m| !crosses(m, bid, ask, last))
            .collect(),
    )
}

/// Whether a menu order would fill (a limit) or trigger (a stop) at once against this market:
/// a buy limit at or above the ask, a sell limit at or below the bid, a buy stop at or below
/// the ask, a sell stop at or above the bid; with a side missing, against the last trade
/// (a limit may rest at it, a stop may not).
pub fn crosses(o: &MenuOrder, bid: Option<Price>, ask: Option<Price>, last: Option<Price>) -> bool {
    let p = o.price.0;
    match (o.side, o.stop) {
        (Side::Buy, false) => match (ask, last) {
            (Some(a), _) => p >= a.0,
            (None, Some(l)) => p > l.0,
            (None, None) => false,
        },
        (Side::Sell, false) => match (bid, last) {
            (Some(b), _) => p <= b.0,
            (None, Some(l)) => p < l.0,
            (None, None) => false,
        },
        (Side::Buy, true) => ask.or(last).is_some_and(|m| p <= m.0),
        (Side::Sell, true) => bid.or(last).is_some_and(|m| p >= m.0),
    }
}

/// A menu order's row: "▲ Buy limit 2 @ 6012.25" (the glyph and the word both carry the side).
pub fn menu_order_label(o: &MenuOrder, qty: &str, fmt: &PriceFormat) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{} {} {} {qty} @ ",
        if o.side == Side::Buy { "▲" } else { "▼" },
        if o.side == Side::Buy { "Buy" } else { "Sell" },
        if o.stop { "stop" } else { "limit" },
    );
    let _ = fmt.write(o.price, &mut s);
    s
}

/// The order section's header: "At 6012.25 · 2 contracts · bracket 8/16" (no bracket part
/// without one; a quantity in its product's units says no "contracts").
pub fn menu_header(
    at: Price,
    qty: &str,
    whole: bool,
    one: bool,
    bracket: Option<crate::dom_trade::BracketPreset>,
    fmt: &PriceFormat,
) -> String {
    let mut s = String::from("At ");
    let _ = fmt.write(at, &mut s);
    let _ = write!(s, " · {qty}");
    if whole {
        s.push_str(if one { " contract" } else { " contracts" });
    }
    if let Some(b) = bracket {
        let _ = write!(s, " · bracket {}/{}", b.stop_ticks, b.target_ticks);
    }
    s
}

/// The chart menu's order section for a right click at `at` (`None`: the press wasn't on the
/// plot's price, or it is gone: the section is taken with its menu). `option`: an option
/// chart, which keeps its trigger flow (no section). `no_market`: the pane has no feed.
/// `trading_on`: a linked DOM on this account and contract has trading enabled. `ctx`: the
/// chart's entry context (`trade_bar::entry_ctx`).
pub fn order_section(
    at: Option<Price>,
    option: bool,
    no_market: bool,
    trading_on: bool,
    ctx: Result<crate::dom_trade::EntryCtx, &'static str>,
    fmt: &PriceFormat,
    qfmt: lines_core::QtyFormat,
) -> Option<crate::pane_menu::ChartOrders> {
    use crate::pane_menu::ChartOrders;
    let note = |s: &str| Some(ChartOrders::Note(s.to_owned()));
    let at = at?;
    if option {
        return None;
    }
    if no_market {
        return note(NO_PRICE_ROW);
    }
    if !trading_on {
        return note(TRADING_OFF_ROW);
    }
    let ctx = match ctx {
        Ok(c) => c,
        Err(why) if why == crate::trade_bar::FEED_STALE => return note(NO_PRICE_ROW),
        Err(why) => return note(&crate::pane_menu::capitalized(why)),
    };
    let orders = match menu_orders(at, ctx.best_bid, ctx.best_ask, ctx.last) {
        Some(v) if !v.is_empty() => v,
        _ => return note(NO_PRICE_ROW),
    };
    let mut qty = String::new();
    let _ = qfmt.write(ctx.qty, &mut qty);
    let whole = qfmt.decimals == 0;
    // The bracket goes on an order that opens or adds (as from the DOM): named in the header
    // when flat (both rows get it), else on the row that would get it.
    let flat = ctx.position == 0;
    let rows = orders
        .iter()
        .map(|o| {
            let mut label = menu_order_label(o, &qty, fmt);
            let adds = crate::dom_trade::opens_or_adds(o.side, ctx.position);
            // Staged on the chart with its legs (only an order that may carry a bracket).
            let staged = adds.then(|| format!("{label}{WITH_BRACKET}"));
            if let Some(b) = ctx.bracket
                && !flat
                && adds
            {
                let _ = write!(label, " · bracket {}/{}", b.stop_ticks, b.target_ticks);
            }
            (label, *o, staged)
        })
        .collect();
    Some(ChartOrders::Rows {
        offer: Offer::of(&ctx),
        header: menu_header(
            at,
            &qty,
            whole,
            ctx.qty.0 == 1,
            ctx.bracket.filter(|_| flat),
            fmt,
        ),
        rows,
    })
}

/// The command for a menu order: the DOM's own click intent (a limit as a click in its bid or
/// ask column, a stop as a Shift+click there, with its far-side check), with the chart's size
/// and bracket. Sent like a strip order: only when a linked DOM has trading enabled.
pub fn menu_command(
    o: MenuOrder,
    offered: Offer,
    ctx: &crate::dom_trade::EntryCtx,
) -> Result<TradeCommand, Refusal> {
    use crate::dom_trade::{Arming, DomColumn, click};
    // Its price belongs to the contract it was offered on (a linked pane can switch the chart
    // while the menu is open): never sent on another.
    if ctx.contract != offered.contract
        || ctx.qty != offered.qty
        || ctx.bracket != offered.bracket
        || account_key(&ctx.account) != offered.account
    {
        return Err(Refusal(CHART_CHANGED));
    }
    // The market may have moved while the menu was open: an order that would now fill or
    // trigger at once is refused, never sent.
    if crosses(&o, ctx.best_bid, ctx.best_ask, ctx.last) {
        return Err(Refusal(MARKET_MOVED));
    }
    let col = if o.side == Side::Buy {
        DomColumn::Bid
    } else {
        DomColumn::Ask
    };
    click(col, o.price, o.stop, Arming::new(true), ctx)
}

// ---- a staged order: a menu order held on the chart until Place --------------------------

/// A staged order discarded because the chart switched symbol or contract.
pub const DISCARDED_SYMBOL: &str = "Staged order discarded: symbol changed";
/// A staged order discarded because the chart switched account.
pub const DISCARDED_ACCOUNT: &str = "Staged order discarded: account changed";
/// A staged order discarded because its tab or pane was hidden (said when next shown).
pub const DISCARDED_HIDDEN: &str = "Staged order discarded: chart hidden";
/// A staged order discarded because its pane moved to or from its own window.
pub const DISCARDED_MOVED: &str = "Staged order discarded: chart moved";
/// Place refused: a buy's stop at or above its entry.
pub const STOP_ABOVE: &str = "The stop is above the entry: drag it below";
/// Place refused: a sell's stop at or below its entry.
pub const STOP_BELOW: &str = "The stop is below the entry: drag it above";
/// Place refused: a buy's target at or below its entry.
pub const TARGET_BELOW: &str = "The target is below the entry: drag it above";
/// Place refused: a sell's target at or above its entry.
pub const TARGET_ABOVE: &str = "The target is above the entry: drag it below";
/// Place refused: a fill since staging made the entry reduce the position, and a bracket only
/// goes on an order that opens or adds.
pub const POSITION_CHANGED: &str =
    "The position changed: a bracket needs an order that opens or adds";

/// The menu row that stages its order: the plain row's words and " with bracket".
pub const WITH_BRACKET: &str = " with bracket";

/// Tag ids of a staged order's three tags (the position tag is `u64::MAX`; order ids are far
/// below these).
pub const STAGED_ENTRY_ID: u64 = u64::MAX - 1;
pub const STAGED_STOP_ID: u64 = u64::MAX - 2;
pub const STAGED_TARGET_ID: u64 = u64::MAX - 3;

/// One of a staged order's lines.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StagedPart {
    Entry,
    Stop,
    Target,
}

impl StagedPart {
    /// Its tag's id.
    pub fn id(self) -> u64 {
        match self {
            StagedPart::Entry => STAGED_ENTRY_ID,
            StagedPart::Stop => STAGED_STOP_ID,
            StagedPart::Target => STAGED_TARGET_ID,
        }
    }

    /// The part a tag id stands for.
    pub fn of_id(id: u64) -> Option<StagedPart> {
        match id {
            STAGED_ENTRY_ID => Some(StagedPart::Entry),
            STAGED_STOP_ID => Some(StagedPart::Stop),
            STAGED_TARGET_ID => Some(StagedPart::Target),
            _ => None,
        }
    }
}

/// An order from the right-click menu's "…with bracket" row, on the chart and not sent: its
/// entry, the offer it was staged with (Place refuses after the contract, size or account
/// changed) and its legs. Nothing reaches the broker before Place.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub order: MenuOrder,
    pub offer: Offer,
    pub stop: Option<Price>,
    pub target: Option<Price>,
    /// The order opened or added to the position when staged: it may carry legs. A reducing
    /// order stages without legs or handles (as the menu's rows attach no bracket to it).
    pub bracketable: bool,
}

/// Stage menu order `o`, offered with `offer`, in context `ctx`: refused like the menu row
/// itself when the chart changed or the market moved since the menu opened. Legs start at the
/// bracket preset's distances (the DOM's math, `dom_trade::click`); with no preset, none (the
/// entry's "+T" / "+S" handles add them).
pub fn stage(
    o: MenuOrder,
    offer: Offer,
    ctx: &crate::dom_trade::EntryCtx,
) -> Result<Staged, Refusal> {
    if ctx.contract != offer.contract
        || ctx.qty != offer.qty
        || ctx.bracket != offer.bracket
        || account_key(&ctx.account) != offer.account
    {
        return Err(Refusal(CHART_CHANGED));
    }
    if crosses(&o, ctx.best_bid, ctx.best_ask, ctx.last) {
        return Err(Refusal(MARKET_MOVED));
    }
    let bracketable = crate::dom_trade::opens_or_adds(o.side, ctx.position);
    let (stop, target) = match ctx.bracket {
        Some(b) if bracketable => {
            let s = o.side.sign();
            (
                Some(Price(o.price.0 - s * b.stop_ticks as i64 * ctx.tick)),
                Some(Price(o.price.0 + s * b.target_ticks as i64 * ctx.tick)),
            )
        }
        _ => (None, None),
    };
    Ok(Staged {
        order: o,
        offer,
        stop,
        target,
        bracketable,
    })
}

impl Staged {
    /// A part's price, if it is there.
    pub fn price(&self, part: StagedPart) -> Option<Price> {
        match part {
            StagedPart::Entry => Some(self.order.price),
            StagedPart::Stop => self.stop,
            StagedPart::Target => self.target,
        }
    }

    /// Move a part to `to` (a snapped price). The entry takes its legs with it, so the risk
    /// and reward it was staged with stay as they were.
    pub fn move_part(&mut self, part: StagedPart, to: Price) {
        match part {
            StagedPart::Entry => {
                let d = to.0 - self.order.price.0;
                self.order.price = to;
                self.stop = self.stop.map(|p| Price(p.0 + d));
                self.target = self.target.map(|p| Price(p.0 + d));
            }
            StagedPart::Stop if self.bracketable => self.stop = Some(to),
            StagedPart::Target if self.bracketable => self.target = Some(to),
            _ => {}
        }
    }

    /// × on a part: a leg goes (its handle comes back); `true` for the entry, which discards
    /// the whole order.
    pub fn remove(&mut self, part: StagedPart) -> bool {
        match part {
            StagedPart::Entry => return true,
            StagedPart::Stop => self.stop = None,
            StagedPart::Target => self.target = None,
        }
        false
    }

    /// Which "+T" / "+S" handles the entry shows, `[above, below]`: one per missing leg, on its
    /// side of the entry (a buy's target above, its stop below; a sell's the other way round).
    pub fn wanted_handles(&self) -> [bool; 2] {
        if !self.bracketable {
            return [false, false];
        }
        let (t, s) = (self.target.is_none(), self.stop.is_none());
        if self.order.side == Side::Buy {
            [t, s]
        } else {
            [s, t]
        }
    }

    /// The leg a handle adds: the target above a buy and below a sell.
    pub fn handle_part(&self, above: bool) -> StagedPart {
        if (self.order.side == Side::Buy) == above {
            StagedPart::Target
        } else {
            StagedPart::Stop
        }
    }

    /// The parts it has, entry first.
    pub fn parts(&self) -> impl Iterator<Item = (StagedPart, Price)> + '_ {
        [StagedPart::Entry, StagedPart::Stop, StagedPart::Target]
            .into_iter()
            .filter_map(|p| self.price(p).map(|x| (p, x)))
    }

    /// Why the legs can't go out as they are: a stop or target on the wrong side of the entry.
    pub fn leg_refusal(&self) -> Option<&'static str> {
        let e = self.order.price.0;
        let buy = self.order.side == Side::Buy;
        if let Some(s) = self.stop {
            if buy && s.0 >= e {
                return Some(STOP_ABOVE);
            }
            if !buy && s.0 <= e {
                return Some(STOP_BELOW);
            }
        }
        if let Some(t) = self.target {
            if buy && t.0 <= e {
                return Some(TARGET_BELOW);
            }
            if !buy && t.0 >= e {
                return Some(TARGET_ABOVE);
            }
        }
        None
    }
}

/// Whether a key goes to the staged order (Enter places it, Esc discards it): plain keys only,
/// and never while something else on the chart holds the key (`typing`: a text field, a drawing
/// gesture, an open list, an order drag or price pick), whose Enter and Esc are its own.
pub fn staged_takes_key(staged: bool, key: &str, plain: bool, typing: bool) -> bool {
    staged && plain && !typing && matches!(key, "enter" | "escape")
}

/// The one command Place sends, or why not (shown as a flash, nothing sent):
/// - the chart's contract, size or account changed since staging: [`CHART_CHANGED`];
/// - the entry would now fill or trigger at once: [`STAGED_CROSSES`];
/// - a leg on the wrong side of the entry; legs on an order that now reduces the position.
///
/// With a leg: `PlaceBracket` with exactly the legs on the chart; with none: `PlaceOrder`. The
/// entry is the DOM's own click intent (a stop's far-side check included), so the core's and
/// the window's checks apply as to any chart order.
pub fn staged_command(
    s: &Staged,
    ctx: &crate::dom_trade::EntryCtx,
) -> Result<TradeCommand, Refusal> {
    use crate::dom_trade::{Arming, DomColumn, click};
    if ctx.contract != s.offer.contract
        || ctx.qty != s.offer.qty
        || account_key(&ctx.account) != s.offer.account
    {
        return Err(Refusal(CHART_CHANGED));
    }
    if crosses(&s.order, ctx.best_bid, ctx.best_ask, ctx.last) {
        return Err(Refusal(STAGED_CROSSES));
    }
    if let Some(why) = s.leg_refusal() {
        return Err(Refusal(why));
    }
    let legs = s.stop.is_some() || s.target.is_some();
    if legs && !crate::dom_trade::opens_or_adds(s.order.side, ctx.position) {
        return Err(Refusal(POSITION_CHANGED));
    }
    // The entry alone: the legs are the chart's, never the preset's.
    let plain = crate::dom_trade::EntryCtx {
        bracket: None,
        ..ctx.clone()
    };
    let col = if s.order.side == Side::Buy {
        DomColumn::Bid
    } else {
        DomColumn::Ask
    };
    let entry = match click(col, s.order.price, s.order.stop, Arming::new(true), &plain)? {
        TradeCommand::PlaceOrder(spec) => spec,
        _ => return Err(Refusal(CHART_CHANGED)),
    };
    Ok(if legs {
        TradeCommand::PlaceBracket(lines_trading::BracketSpec {
            entry,
            take_profit: s.target,
            stop_loss: s.stop,
        })
    } else {
        TradeCommand::PlaceOrder(entry)
    })
}

/// A leg's ticks per contract and money from the entry (profit positive): what fills there
/// would make or lose, as the P&L display shows it.
pub fn leg_pnl(s: &Staged, at: Price, tick: i64, mpu: i128) -> (f64, Money) {
    let units = (at.0 - s.order.price.0) * s.order.side.sign();
    let ticks = units as f64 / tick.max(1) as f64;
    (ticks, Money(units as i128 * s.offer.qty.0 as i128 * mpu))
}

/// The staged entry's tag: `▲ Buy limit 2 @ 7510.00`, then ` · risk -16.0 t  -$400.00` (the
/// stop's loss as the P&L display shows it; nothing when off) and ` · 1 : 2.0` (reward over
/// risk, both legs there). No allocation.
pub fn write_staged_entry(
    out: &mut impl Write,
    s: &Staged,
    fmt: &PriceFormat,
    qfmt: lines_core::QtyFormat,
    mpu: Option<i128>,
    mode: PnlDisplay,
) -> fmt::Result {
    let o = &s.order;
    write!(
        out,
        "{} {} {} ",
        if o.side == Side::Buy { "▲" } else { "▼" },
        if o.side == Side::Buy { "Buy" } else { "Sell" },
        if o.stop { "stop" } else { "limit" },
    )?;
    qfmt.write(s.offer.qty, out)?;
    out.write_str(" @ ")?;
    fmt.write(o.price, out)?;
    if let (Some(stop), Some(mpu)) = (s.stop, mpu)
        && mode.shown()
    {
        let (ticks, money) = leg_pnl(s, stop, fmt.tick, mpu);
        out.write_str(" · risk ")?;
        crate::pnl::write_pnl(out, mode, Some(ticks), money)?;
    }
    if let (Some(stop), Some(target)) = (s.stop, s.target) {
        let risk = (o.price.0 - stop.0).abs();
        let reward = (target.0 - o.price.0).abs();
        if risk > 0 {
            let tenths = (reward * 10 + risk / 2) / risk;
            write!(out, " · 1 : {}.{}", tenths / 10, tenths % 10)?;
        }
    }
    Ok(())
}

/// A staged leg's tag: `Target 2 @ 7518.00` in `text`, and its P&L there (`+32.0 t  +$800.00`)
/// in `pnl` with its money for the tone (`None`: P&L off or no money per unit). No allocation.
#[allow(clippy::too_many_arguments)]
pub fn write_staged_leg(
    text: &mut impl Write,
    pnl: &mut impl Write,
    s: &Staged,
    part: StagedPart,
    fmt: &PriceFormat,
    qfmt: lines_core::QtyFormat,
    mpu: Option<i128>,
    mode: PnlDisplay,
) -> Result<Option<Money>, fmt::Error> {
    let Some(at) = s.price(part) else {
        return Ok(None);
    };
    text.write_str(match part {
        StagedPart::Target => "Target ",
        StagedPart::Stop => "Stop ",
        StagedPart::Entry => "Entry ",
    })?;
    qfmt.write(s.offer.qty, text)?;
    text.write_str(" @ ")?;
    fmt.write(at, text)?;
    let Some(mpu) = mpu else {
        return Ok(None);
    };
    let (ticks, money) = leg_pnl(s, at, fmt.tick, mpu);
    Ok(crate::pnl::write_pnl(pnl, mode, Some(ticks), money)?.then_some(money))
}

/// The staged entry's two buttons: "Place ⏎" and "× Esc".
pub const PLACE_LABEL: &str = "Place ⏎";
pub const DISCARD_LABEL: &str = "× Esc";

/// What is under the pointer on a staged order's tags. The entry's tag ends in its Place and
/// × Esc buttons (`buttons`: their widths); a leg's in the usual × box (`close_left`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StagedHit {
    Place,
    Close(StagedPart),
    Body(StagedPart),
}

/// The staged tag under plot-local (`x`, `y`) among this frame's tags, if any.
pub fn staged_hit(tags: &[TagBox], buttons: (f32, f32), x: f32, y: f32) -> Option<StagedHit> {
    let b = tags
        .iter()
        .find(|b| StagedPart::of_id(b.id).is_some() && b.contains(x, y))?;
    let part = StagedPart::of_id(b.id)?;
    let right = b.left + b.w;
    Some(match part {
        StagedPart::Entry => {
            let (place, esc) = buttons;
            if x >= right - esc {
                StagedHit::Close(part)
            } else if x >= right - esc - place {
                StagedHit::Place
            } else {
                StagedHit::Body(part)
            }
        }
        _ if x >= b.close_left => StagedHit::Close(part),
        _ => StagedHit::Body(part),
    })
}

/// A staged order's handle under plot-local (`x`, `y`): which leg it adds.
pub fn staged_handle_hit(
    handles: &[Option<Handle>; 2],
    s: &Staged,
    x: f32,
    y: f32,
) -> Option<StagedPart> {
    handles
        .iter()
        .flatten()
        .find(|h| h.contains(x, y))
        .map(|h| s.handle_part(h.above))
}

/// A screenshot's staged order (`--stage-order`): `buy limit 6010.00 stop 6006.00 target
/// 6018.00` (the side, `limit` or `stop`, the price, then optional legs). Prices in the
/// product's format.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StageSpec {
    pub order: MenuOrder,
    pub stop: Option<Price>,
    pub target: Option<Price>,
}

/// Parse a [`StageSpec`] with `parse_price` (the product's own parser).
pub fn parse_stage_spec(
    s: &str,
    parse_price: impl Fn(&str) -> Option<Price>,
) -> Result<StageSpec, String> {
    let w: Vec<&str> = s.split_whitespace().collect();
    let bad = || {
        format!(
            "--stage-order {s:?}: expected e.g. \"buy limit 6010.00 stop 6006.00 target 6018.00\""
        )
    };
    if w.len() < 3 || w.len().is_multiple_of(2) {
        return Err(bad());
    }
    let side = match w[0] {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        _ => return Err(bad()),
    };
    let stop = match w[1] {
        "limit" => false,
        "stop" => true,
        _ => return Err(bad()),
    };
    let price = parse_price(w[2]).ok_or_else(bad)?;
    let mut spec = StageSpec {
        order: MenuOrder { side, stop, price },
        stop: None,
        target: None,
    };
    for pair in w[3..].chunks(2) {
        let p = parse_price(pair[1]).ok_or_else(bad)?;
        match pair[0] {
            "stop" => spec.stop = Some(p),
            "target" => spec.target = Some(p),
            _ => return Err(bad()),
        }
    }
    Ok(spec)
}

/// Whether a press released `dx`, `dy` px from where it started was a click (a pick sends) or a
/// drag (a pan: the pick stays armed), by the chart's click rule ([`DRAG_PX`]).
pub fn is_click(dx: f32, dy: f32) -> bool {
    dx.hypot(dy) < DRAG_PX
}

/// Strip height, margin from the plot's bottom-left corner, and its pieces' widths at 100 %
/// (times [`StripBox::s`], the UI zoom never under 100 %, so the pointer test needs no layout):
/// quantity, two market buttons, two limit buttons.
pub const STRIP_H: f32 = 26.0;
pub const STRIP_MARGIN: f32 = 8.0;
pub const STRIP_PAD: f32 = 3.0;
pub const STRIP_GAP: f32 = 4.0;
/// The size control: − (hit target at least 20 px), the value, +.
pub const STRIP_STEP_W: f32 = 20.0;
pub const STRIP_VALUE_W: f32 = 44.0;
pub const STRIP_QTY_W: f32 = 2.0 * STRIP_STEP_W + STRIP_VALUE_W + 4.0;
pub const STRIP_MKT_W: f32 = 96.0;
pub const STRIP_LMT_W: f32 = 96.0;

/// Where the strip sits in the plot (plot-local).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct StripBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Narrow plots drop the limit buttons (market only).
    pub limits: bool,
    /// The factor its sizes are at: the UI zoom, never under 100 % (they are hit targets).
    pub s: f32,
}

impl StripBox {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h()
    }

    /// Its height.
    pub fn h(&self) -> f32 {
        (STRIP_H * self.s).round()
    }

    /// A 100 % size at its scale, in whole px.
    pub fn px(&self, v: f32) -> f32 {
        (v * self.s).round()
    }
}

/// The strip's box in a `plot_w` x `plot_h` plot at UI zoom `zoom`: bottom-left, inside the
/// margin; `None` when the plot is too small for it.
pub fn strip_box(plot_w: f32, plot_h: f32, zoom: f32) -> Option<StripBox> {
    let s = if zoom.is_finite() { zoom.max(1.0) } else { 1.0 };
    let px = |v: f32| (v * s).round();
    let market = px(STRIP_PAD) * 2.0 + px(STRIP_QTY_W) + 2.0 * (px(STRIP_GAP) + px(STRIP_MKT_W));
    let full = market + 2.0 * (px(STRIP_GAP) + px(STRIP_LMT_W));
    let room = plot_w - 2.0 * px(STRIP_MARGIN);
    let w = if room >= full {
        full
    } else if room >= market {
        market
    } else {
        return None;
    };
    let y = plot_h - px(STRIP_MARGIN) - px(STRIP_H);
    (y >= px(STRIP_MARGIN)).then_some(StripBox {
        x: px(STRIP_MARGIN),
        y,
        w,
        limits: w == full,
        s,
    })
}

/// Orders a chart holds without growing its buffers (more just grow them once): a chart never
/// leaves an open order out.
const RESERVED_ORDERS: usize = 256;

impl Default for ChartTradeState {
    fn default() -> Self {
        ChartTradeState {
            marks: Vec::with_capacity(RESERVED_ORDERS),
            texts: Vec::with_capacity(MAX_TAGS),
            lines: Vec::with_capacity(RESERVED_ORDERS),
            on_plot: Vec::with_capacity(RESERVED_ORDERS),
            groups: [TagGroup::default(); MAX_TAGS],
            n_groups: 0,
            // The groups' tags and a staged order's three (`chart_view::draw_orders`).
            tag_lines: Vec::with_capacity(MAX_TAGS + 3),
            group_menu: None,
            tags: [TagBox::default(); MAX_TAGS],
            n_tags: 0,
            markers: [None, None],
            handles: [None, None],
            position: None,
            ghosts: Vec::with_capacity(8),
            drag: None,
            adding: None,
            closing: None,
            grab: 0,
            add_from: None,
            pos_tag: None,
            pos_drag: false,
            press_y: 0.0,
            moved: 0.0,
            strip_pick: None,
            resize_hover: None,
            resize_grows: false,
            staged: None,
            staged_drag: None,
            staged_press: None,
            placing: None,
            staged_handles: [None, None],
            staged_buttons: (0.0, 0.0),
            notice: None,
        }
    }
}

impl ChartTradeState {
    /// Copy this frame's orders and position for `account` / `contract`. Returns refused drags
    /// (rare; allocates only then).
    pub fn prepare(
        &mut self,
        cache: &TradeCache,
        account: Option<&AccountId>,
        contract: Option<ContractKey>,
    ) -> Vec<(Price, String)> {
        let mut refused = Vec::new();
        let (Some(account), Some(contract)) = (account, contract) else {
            self.marks.clear();
            self.position = None;
            self.ghosts.clear();
            return refused;
        };
        let Some(s) = cache.try_read() else {
            return refused;
        };
        self.marks.clear();
        self.marks
            .extend(s.open_orders(account, contract).filter_map(order_mark));
        self.position = s
            .position(account, contract)
            .and_then(|p| p.avg_price.map(|a| (p.qty, a)))
            .filter(|p| p.0 != 0);
        self.resize_grows = self
            .resize_hover
            .and_then(|id| s.order(lines_trading::OrderId(id)))
            .is_some_and(|o| {
                let working = o
                    .parent
                    .and_then(|p| s.order(p))
                    .is_some_and(lines_trading::Order::is_open);
                let position = s.position(account, contract).map_or(0, |p| p.qty);
                crate::order_resize::can_grow(o, working, position)
            });
        self.ghosts.retain(
            |g| match ghost_outcome(g, s.order(g.order), s.result(g.command)) {
                GhostOutcome::Pending => true,
                GhostOutcome::Acked => false,
                GhostOutcome::Rejected(why) => {
                    refused.push((g.price, why));
                    false
                }
            },
        );
        refused
    }

    /// A strip limit button pressed: arm its price pick, or disarm it if it was armed.
    pub fn arm_pick(&mut self, o: StripOrder) {
        self.strip_pick = (self.strip_pick != Some(o)).then_some(o);
    }

    /// The chart shows another symbol or contract: a pick armed on the old one is dropped, so
    /// its click never sends at a price read on another product's scale, and a group's list
    /// closes (its orders and money are the old contract's).
    /// A staged order goes too (its prices are the old product's): the notice to show, if one
    /// was staged.
    pub fn feed_changed(&mut self) -> Option<&'static str> {
        self.strip_pick = None;
        self.group_menu = None;
        self.discard(DISCARDED_SYMBOL)
    }

    /// Drop the staged order (and a drag of it), nothing sent: `why` if one was staged.
    pub fn discard(&mut self, why: &'static str) -> Option<&'static str> {
        self.staged_drag = None;
        self.staged_press = None;
        self.staged_handles = [None, None];
        self.staged.take().map(|_| why)
    }

    /// The chart's account, each poll: a staged order from another account is discarded. The
    /// notice, if one was.
    pub fn staged_account(&mut self, account: Option<&AccountId>) -> Option<&'static str> {
        let stale = self
            .staged
            .is_some_and(|s| account.is_none_or(|a| account_key(a) != s.offer.account));
        if stale {
            self.discard(DISCARDED_ACCOUNT)
        } else {
            None
        }
    }

    /// Place pressed and the command went to the window: off the chart, held until the window
    /// answers ([`ChartTradeState::place_refused`]).
    pub fn placed(&mut self) {
        self.placing = self.staged.take();
        self.staged_drag = None;
        self.staged_handles = [None, None];
    }

    /// The window sent what Place built: it never comes back.
    pub fn place_sent(&mut self) {
        self.placing = None;
    }

    /// The window refused what Place sent (no linked DOM enabled): it goes back on the chart.
    pub fn place_refused(&mut self) {
        if let Some(s) = self.placing.take() {
            self.staged = Some(s);
        }
    }

    /// The chart's account, each poll: a group's list on another account closes. Whether it did.
    pub fn account_changed(&mut self, account: Option<&AccountId>) -> bool {
        let stale = self
            .group_menu
            .as_ref()
            .is_some_and(|m| m.account.as_ref() != account);
        if stale {
            self.group_menu = None;
        }
        stale
    }

    /// The group a tag id stands for, if that tag holds several orders.
    pub fn group_of(&self, id: u64) -> Option<&TagGroup> {
        self.groups[..self.n_groups]
            .iter()
            .find(|g| g.id == id && g.is_group())
    }

    /// The orders behind a tag (`on_plot` is sorted, a group is one run of it).
    pub fn members(&self, g: &TagGroup) -> &[GroupIn] {
        &self.on_plot[g.first..g.first + g.n]
    }

    /// The single-order tag under plot-local (`x`, `y`) for the resize control, and the control's
    /// piece if the point is on it: the drawn control first (`typing`'s tag, else the hovered
    /// tag's; it sits left of the tag, over whatever is there). A group's tag has none: no action
    /// on it may touch only one of its orders.
    pub fn resize_target(
        &self,
        x: f32,
        y: f32,
        typing: Option<u64>,
    ) -> Option<(u64, Option<ResizeHit>)> {
        let tags = &self.tags[..self.n_tags];
        if let Some(b) = typing
            .or(self.resize_hover)
            .filter(|h| self.group_of(*h).is_none())
            .and_then(|h| tags.iter().find(|b| b.id == h))
            && let Some(hit) = resize_hit(b, x, y)
        {
            return Some((b.id, Some(hit)));
        }
        tags.iter()
            .find(|b| b.contains(x, y))
            .filter(|b| self.group_of(b.id).is_none() && self.mark(b.id).is_some())
            .map(|b| (b.id, None))
    }

    /// The order behind a tag id (open, as of this frame).
    pub fn mark(&self, id: u64) -> Option<&OrderMark> {
        self.marks.iter().find(|m| m.id.0 == id)
    }

    /// The position's existing exit of a kind (`target`), if any: a drag of that kind moves it.
    pub fn existing_exit(&self, target: bool) -> Option<OrderMark> {
        let want = if target {
            MarkRole::Target
        } else {
            MarkRole::Stop
        };
        self.marks.iter().copied().find(|m| MarkRole::of(m) == want)
    }

    /// Whether the position already has a target / a stop working: `(target, stop)`.
    pub fn exits(&self) -> (bool, bool) {
        let mut out = (false, false);
        for m in &self.marks {
            match MarkRole::of(m) {
                MarkRole::Target => out.0 = true,
                MarkRole::Stop => out.1 = true,
                MarkRole::Entry => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_trading::OrderId;
    use lines_trading::order::Role;

    fn p(x: f64) -> Price {
        Price((x * 100.0).round() as i64)
    }

    fn es_contract() -> ContractKey {
        ContractKey::monthly(
            lines_feed::catalog::ES,
            2026,
            lines_core::Month::new(12).unwrap(),
        )
    }

    /// Below the market: buy limit and sell stop; above: sell limit and buy stop; at the inside:
    /// the two limits at the touch. None of them fills at once.
    #[test]
    fn a_right_click_offers_the_side_away_from_the_market() {
        let (bid, ask) = (Some(p(6012.00)), Some(p(6012.25)));
        let o = |side, stop, price| MenuOrder { side, stop, price };
        assert_eq!(
            menu_orders(p(6010.00), bid, ask, None),
            Some(vec![
                o(Side::Buy, false, p(6010.00)),
                o(Side::Sell, true, p(6010.00))
            ])
        );
        assert_eq!(
            menu_orders(p(6014.00), bid, ask, None),
            Some(vec![
                o(Side::Sell, false, p(6014.00)),
                o(Side::Buy, true, p(6014.00))
            ])
        );
        let inside = Some(vec![
            o(Side::Buy, false, p(6012.00)),
            o(Side::Sell, false, p(6012.25)),
        ]);
        assert_eq!(
            menu_orders(p(6012.00), bid, ask, None),
            inside,
            "at the bid"
        );
        assert_eq!(
            menu_orders(p(6012.25), bid, ask, None),
            inside,
            "at the ask"
        );
        // No quote: the last trade stands in for both sides.
        let last = Some(p(6012.00));
        assert_eq!(
            menu_orders(p(6011.75), None, None, last).map(|v| v[1].stop),
            Some(true)
        );
        assert_eq!(
            menu_orders(p(6012.00), None, None, last),
            Some(vec![
                o(Side::Buy, false, p(6012.00)),
                o(Side::Sell, false, p(6012.00))
            ])
        );
        assert_eq!(menu_orders(p(6012.00), None, None, None), None, "no price");
        // One-sided: a bid of 6012.00, no ask, the last trade lower. A sell limit at the bid
        // would fill: it is left out; the buy limit stays.
        assert_eq!(
            menu_orders(p(6012.00), Some(p(6012.00)), None, Some(p(6011.75))),
            Some(vec![o(Side::Buy, false, p(6011.75))])
        );
        // Every offered row passes the same test as when it is chosen, at every price.
        for at in [6011.50, 6011.75, 6012.00, 6012.25, 6012.50] {
            for (b, a, l) in [
                (bid, ask, None),
                (Some(p(6012.00)), None, Some(p(6011.75))),
                (None, Some(p(6012.25)), Some(p(6012.50))),
                (Some(p(6012.25)), Some(p(6012.00)), None),
            ] {
                for m in menu_orders(p(at), b, a, l).unwrap_or_default() {
                    assert!(!crosses(&m, b, a, l), "{at}: {m:?}");
                }
            }
        }
    }

    /// Each offered order passes the DOM's own checks: a stop is always on the far side, so it
    /// is never refused as one that would trigger at once.
    #[test]
    fn offered_orders_pass_the_doms_checks() {
        let ctx = |bid, ask| crate::dom_trade::EntryCtx {
            account: AccountId::new("paper"),
            contract: es_contract(),
            qty: Qty(2),
            position: 0,
            bracket: None,
            tick: es().price.tick,
            best_bid: Some(p(bid)),
            best_ask: Some(p(ask)),
            last: Some(p(bid)),
        };
        for at in [6000.00, 6011.75, 6012.00, 6012.25, 6012.50, 6030.00] {
            let c = ctx(6012.00, 6012.25);
            for o in menu_orders(p(at), c.best_bid, c.best_ask, c.last).unwrap() {
                let cmd =
                    menu_command(o, offer(&c), &c).unwrap_or_else(|e| panic!("{at}: {o:?}: {e:?}"));
                let TradeCommand::PlaceOrder(spec) = cmd else {
                    panic!("no bracket: a plain order")
                };
                assert_eq!((spec.side, spec.qty), (o.side, Qty(2)));
                let want = if o.stop {
                    OrderKind::Stop { trigger: o.price }
                } else {
                    OrderKind::Limit { price: o.price }
                };
                assert_eq!(spec.kind, want);
            }
        }
    }

    /// The size and bracket in the header and the rows are the chart's (the strip's).
    #[test]
    fn the_menu_names_size_and_bracket() {
        let b = crate::dom_trade::BracketPreset {
            stop_ticks: 2,
            target_ticks: 4,
        };
        let f = es().price;
        assert_eq!(
            menu_header(p(6030.25), "2", true, false, Some(b), &f),
            "At 6030.25 · 2 contracts · bracket 2/4"
        );
        assert_eq!(
            menu_header(p(6030.25), "1", true, true, None, &f),
            "At 6030.25 · 1 contract"
        );
        assert_eq!(
            menu_header(p(6030.25), "0.5", false, false, None, &f),
            "At 6030.25 · 0.5"
        );
        let o = MenuOrder {
            side: Side::Sell,
            stop: true,
            price: p(6010.00),
        };
        assert_eq!(menu_order_label(&o, "2", &f), "▼ Sell stop 2 @ 6010.00");
        // A bracket preset rides along as the DOM's would.
        let mut c = crate::dom_trade::EntryCtx {
            account: AccountId::new("paper"),
            contract: es_contract(),
            qty: Qty(2),
            position: 0,
            bracket: Some(b),
            tick: f.tick,
            best_bid: Some(p(6012.00)),
            best_ask: Some(p(6012.25)),
            last: None,
        };
        let buy = MenuOrder {
            side: Side::Buy,
            stop: false,
            price: p(6010.00),
        };
        assert!(matches!(
            menu_command(buy, offer(&c), &c),
            Ok(TradeCommand::PlaceBracket(_))
        ));
        c.qty = Qty(0);
        assert!(
            menu_command(buy, offer(&c), &c).is_err(),
            "no size: refused like the DOM"
        );
    }

    fn menu_ctx(position: i64, bracket: bool) -> crate::dom_trade::EntryCtx {
        crate::dom_trade::EntryCtx {
            account: AccountId::new("paper"),
            contract: es_contract(),
            qty: Qty(2),
            position,
            bracket: bracket.then_some(crate::dom_trade::BracketPreset {
                stop_ticks: 2,
                target_ticks: 4,
            }),
            tick: 25,
            best_bid: Some(p(6012.00)),
            best_ask: Some(p(6012.25)),
            last: Some(p(6012.00)),
        }
    }

    /// The section's gating: none off the plot or on an option chart; one dim note with no
    /// feed, trading off, a stale feed or no price; else the header and the rows.
    #[test]
    fn the_order_section_says_why_it_has_no_orders() {
        use crate::pane_menu::ChartOrders;
        let (f, q) = (es().price, es().qty);
        let at = Some(p(6010.00));
        let note = |s: &str| Some(ChartOrders::Note(s.to_owned()));
        let sec = |at, option, no_market, on, c| order_section(at, option, no_market, on, c, &f, q);
        assert_eq!(sec(None, false, false, true, Ok(menu_ctx(0, false))), None);
        assert_eq!(
            sec(at, true, false, true, Ok(menu_ctx(0, false))),
            None,
            "option chart"
        );
        assert_eq!(
            sec(at, false, true, false, Ok(menu_ctx(0, false))),
            note(NO_PRICE_ROW)
        );
        assert_eq!(
            sec(at, false, false, false, Ok(menu_ctx(0, false))),
            note(TRADING_OFF_ROW)
        );
        assert_eq!(
            sec(at, false, false, true, Err(crate::trade_bar::FEED_STALE)),
            note(NO_PRICE_ROW)
        );
        assert_eq!(
            sec(at, false, false, true, Err("no account")),
            note("No account")
        );
        let mut quiet = menu_ctx(0, false);
        (quiet.best_bid, quiet.best_ask, quiet.last) = (None, None, None);
        assert_eq!(sec(at, false, false, true, Ok(quiet)), note(NO_PRICE_ROW));
        let Some(ChartOrders::Rows {
            header,
            rows,
            offer: shown,
        }) = sec(at, false, false, true, Ok(menu_ctx(0, true)))
        else {
            panic!("rows")
        };
        // The offer is what the rows show: the context's contract, size and bracket.
        assert_eq!(shown, offer(&menu_ctx(0, true)));
        assert_eq!(header, "At 6010.00 · 2 contracts · bracket 2/4");
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["▲ Buy limit 2 @ 6010.00", "▼ Sell stop 2 @ 6010.00"]
        );
        // Flat, both open a position: both can be staged with a bracket.
        assert_eq!(
            rows.iter().map(|r| r.2.as_deref()).collect::<Vec<_>>(),
            [
                Some("▲ Buy limit 2 @ 6010.00 with bracket"),
                Some("▼ Sell stop 2 @ 6010.00 with bracket")
            ]
        );
    }

    /// Not flat, the bracket rides only on the order that would add: named on that row, not in
    /// the header (a closing order gets none, as from the DOM).
    #[test]
    fn the_bracket_is_named_where_it_goes() {
        use crate::pane_menu::ChartOrders;
        let (f, q) = (es().price, es().qty);
        let Some(ChartOrders::Rows { header, rows, .. }) = order_section(
            Some(p(6010.00)),
            false,
            false,
            true,
            Ok(menu_ctx(2, true)),
            &f,
            q,
        ) else {
            panic!("rows")
        };
        assert_eq!(header, "At 6010.00 · 2 contracts");
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            [
                "▲ Buy limit 2 @ 6010.00 · bracket 2/4",
                "▼ Sell stop 2 @ 6010.00"
            ]
        );
        // The staged row is the order's plain words (the bracket is its legs, on the chart),
        // and only for the order that adds.
        assert_eq!(
            rows.iter().map(|r| r.2.as_deref()).collect::<Vec<_>>(),
            [Some("▲ Buy limit 2 @ 6010.00 with bracket"), None]
        );
    }

    fn offer(c: &crate::dom_trade::EntryCtx) -> Offer {
        Offer::of(c)
    }

    /// The size it was offered with, too: a size changed while the menu was open refuses it;
    /// and the account must be the one it was offered on.
    #[test]
    fn a_menu_order_keeps_the_size_and_account_it_showed() {
        let c = menu_ctx(0, false);
        let o = MenuOrder {
            side: Side::Buy,
            stop: false,
            price: p(6010.00),
        };
        let shown = Offer {
            qty: Qty(3),
            ..offer(&c)
        };
        assert_eq!(menu_command(o, shown, &c), Err(Refusal(CHART_CHANGED)));
        // A bracket set since the menu opened (or one it named, gone): refused too.
        let protected = Offer {
            bracket: Some(crate::dom_trade::BracketPreset {
                stop_ticks: 2,
                target_ticks: 4,
            }),
            ..offer(&c)
        };
        assert_eq!(menu_command(o, protected, &c), Err(Refusal(CHART_CHANGED)));
        let c2 = menu_ctx(0, true);
        assert_eq!(menu_command(o, offer(&c), &c2), Err(Refusal(CHART_CHANGED)));
        assert!(menu_command(o, offer(&c2), &c2).is_ok());
        let (a, b) = (AccountId::new("paper"), AccountId::new("other"));
        assert!(same_account(Some(&a), Some(&a)));
        assert!(!same_account(Some(&a), Some(&b)));
        assert!(!same_account(None, Some(&a)), "no offer open");
        assert!(!same_account(None, None));
    }

    /// A row is sent only on the account it was offered on: the chart switched to another
    /// account while the menu was open refuses it.
    #[test]
    fn a_menu_order_never_carries_to_another_account() {
        let on_a = menu_ctx(0, false);
        let o = MenuOrder {
            side: Side::Buy,
            stop: false,
            price: p(6010.00),
        };
        let shown = Offer::of(&on_a);
        assert!(menu_command(o, shown, &on_a).is_ok());
        let on_b = crate::dom_trade::EntryCtx {
            account: AccountId::new("other"),
            ..menu_ctx(0, false)
        };
        assert_eq!(menu_command(o, shown, &on_b), Err(Refusal(CHART_CHANGED)));
        assert_ne!(account_key(&on_a.account), account_key(&on_b.account));
    }

    /// A row is sent only on the contract it was offered on: a chart switched to another while
    /// the menu was open refuses it.
    #[test]
    fn a_menu_order_never_carries_to_another_contract() {
        let c = menu_ctx(0, false);
        let o = MenuOrder {
            side: Side::Buy,
            stop: false,
            price: p(6010.00),
        };
        assert!(menu_command(o, offer(&c), &c).is_ok());
        let nq = ContractKey::monthly(
            lines_feed::catalog::NQ,
            2026,
            lines_core::Month::new(12).unwrap(),
        );
        assert_eq!(
            menu_command(
                o,
                Offer {
                    contract: nq,
                    ..offer(&c)
                },
                &c
            ),
            Err(Refusal(CHART_CHANGED))
        );
    }

    /// Chosen after the market moved through it: a limit that would fill at once, or a stop
    /// now on the near side, is refused, never sent.
    #[test]
    fn a_menu_order_the_market_moved_through_is_refused() {
        let mut c = crate::dom_trade::EntryCtx {
            account: AccountId::new("paper"),
            contract: es_contract(),
            qty: Qty(1),
            position: 0,
            bracket: None,
            tick: 25,
            best_bid: Some(p(6012.00)),
            best_ask: Some(p(6012.25)),
            last: Some(p(6012.00)),
        };
        let o = |side, stop, price| MenuOrder { side, stop, price };
        let buy = o(Side::Buy, false, p(6010.00));
        let stop = o(Side::Sell, true, p(6010.00));
        assert!(
            menu_command(buy, offer(&c), &c).is_ok() && menu_command(stop, offer(&c), &c).is_ok()
        );
        // The market fell through 6010.00.
        c.best_bid = Some(p(6009.75));
        c.best_ask = Some(p(6010.00));
        assert_eq!(menu_command(buy, offer(&c), &c), Err(Refusal(MARKET_MOVED)));
        assert_eq!(
            menu_command(stop, offer(&c), &c),
            Err(Refusal(MARKET_MOVED)),
            "the stop is on the near side now"
        );
        let sell = o(Side::Sell, false, p(6009.75));
        assert_eq!(
            menu_command(sell, offer(&c), &c),
            Err(Refusal(MARKET_MOVED))
        );
        // Last only: the inside's limits at the last trade still rest.
        c.best_bid = None;
        c.best_ask = None;
        c.last = Some(p(6012.00));
        assert!(menu_command(o(Side::Buy, false, p(6012.00)), offer(&c), &c).is_ok());
        assert!(menu_command(o(Side::Sell, false, p(6012.00)), offer(&c), &c).is_ok());
        assert!(menu_command(o(Side::Buy, false, p(6012.25)), offer(&c), &c).is_err());
    }

    fn fmt() -> PriceFormat {
        es().price
    }

    fn mark(side: Side, price: f64, stop: bool, role: Role, exit: bool) -> OrderMark {
        OrderMark {
            id: OrderId(7),
            side,
            price: Price((price * 100.0).round() as i64),
            style: MarkStyle::Working,
            qty: Qty(1),
            filled: Qty(0),
            stop,
            role,
            exit,
        }
    }

    fn es() -> Product {
        lines_feed::catalog::get(lines_feed::catalog::ES).unwrap()
    }

    fn group(key: u16, qty: i64, n: usize, lo: f64, hi: f64, notional: i128) -> TagGroup {
        TagGroup {
            id: 7,
            key,
            first: 0,
            n,
            qty,
            notional,
            lo: Price((lo * 100.0).round() as i64),
            hi: Price((hi * 100.0).round() as i64),
        }
    }

    /// The owner's mock: what is left and how many, the average, the range; one price needs
    /// neither; held says so; several kinds name no role and no average.
    #[test]
    fn group_tags_read_as_the_mock() {
        let target = mark(Side::Sell, 5823.00, false, Role::TakeProfit, false);
        let tag = |m: &OrderMark, g: &TagGroup| {
            let mut s = String::new();
            write_group_tag(&mut s, m, g, &fmt()).unwrap();
            s
        };
        // 5823.00 × 171 + 5822.75 × 29 over 200: 5822.96375, rounded for the label.
        let g = group(1, 200, 20, 5822.75, 5823.00, 582_300 * 171 + 582_275 * 29);
        assert_eq!(
            tag(&target, &g),
            "Target 200 ×20 @ ~5822.96 5822.75–5823.00"
        );
        let one = group(1, 4, 2, 5820.00, 5820.00, 582_000 * 4);
        assert_eq!(tag(&target, &one), "Target 4 ×2 @ 5820.00");
        let mut held = mark(Side::Sell, 5800.00, true, Role::StopLoss, false);
        held.style = MarkStyle::Held;
        let g = group(1, 2, 2, 5800.00, 5800.00, 580_000 * 2);
        assert_eq!(tag(&held, &g), "Stop 2 ×2 @ 5800.00 · held");
        let entry = mark(Side::Buy, 5790.00, false, Role::Single, false);
        let g = group(1, 3, 3, 5790.00, 5790.00, 579_000 * 3);
        assert_eq!(tag(&entry, &g), "Buy lmt 3 ×3 @ 5790.00");
        let g = group(MIXED_KEY, 5, 5, 5790.00, 5823.00, 0);
        assert_eq!(tag(&target, &g), "Orders 5 ×5 @ 5790.00–5823.00");
    }

    /// One double-click on a market button is one order, however long the run; a press after a
    /// pause counts 1. A limit button only arms, and a second press must still disarm.
    #[test]
    fn a_double_click_sends_one_market_order_and_a_limit_can_still_disarm() {
        for o in [StripOrder::BuyMarket, StripOrder::SellMarket] {
            assert!(strip_press_acts(o, 1), "the first");
            assert!(!strip_press_acts(o, 2), "a double-click's second");
            assert!(!strip_press_acts(o, 3), "nor a third");
        }
        for o in [StripOrder::BuyLimit, StripOrder::SellLimit] {
            assert!(strip_press_acts(o, 1));
            assert!(strip_press_acts(o, 2), "a change of mind disarms");
        }
    }

    /// Cancel all takes two presses, and the second answers the count on screen: an order
    /// finishing in between disarms it.
    #[test]
    fn cancel_all_needs_a_second_press_for_the_same_orders() {
        let row = |id| GroupRow {
            mark: OrderMark {
                id: OrderId(id),
                ..mark(Side::Sell, 5820.00, false, Role::TakeProfit, false)
            },
            open: true,
        };
        let mut m = GroupMenu {
            rows: vec![row(1), row(2), row(3)],
            ..GroupMenu::default()
        };
        assert_eq!(m.cancel_all(), None, "the first press only arms");
        assert_eq!(m.confirm, Some(3));
        assert!(!m.refresh(|_| true));
        assert_eq!(m.confirm, Some(3), "nothing changed: still armed");
        assert!(m.refresh(|o| o != OrderId(2)));
        assert_eq!(m.confirm, None, "the list changed: disarmed");
        assert_eq!(
            m.rows.len(),
            3,
            "a finished order keeps its row: nothing shifts"
        );
        assert!(!m.rows[1].open);
        assert_eq!(m.open_count(), 2);
        assert_eq!(m.cancel_all(), None);
        assert_eq!(
            m.cancel_all(),
            Some(vec![OrderId(1), OrderId(3)]),
            "only what is still open"
        );
        assert_eq!(m.confirm, None);
        m.refresh(|_| false);
        assert_eq!(m.cancel_all(), None, "nothing to cancel");
        assert_eq!(m.cancel_all(), None);
    }

    /// A list belongs to its account and contract: another symbol or account closes it, so no
    /// button acts on orders the chart no longer shows.
    #[test]
    fn the_list_closes_on_another_symbol_or_account() {
        let (a, b) = (AccountId::new("a"), AccountId::new("b"));
        let open = || GroupMenu {
            account: Some(a.clone()),
            ..GroupMenu::default()
        };
        let mut s = ChartTradeState {
            group_menu: Some(open()),
            ..ChartTradeState::default()
        };
        assert!(!s.account_changed(Some(&a)));
        assert!(s.group_menu.is_some(), "the same account: still open");
        assert!(s.account_changed(Some(&b)));
        assert!(s.group_menu.is_none());
        assert!(!s.account_changed(Some(&b)), "already closed");
        s.group_menu = Some(open());
        s.feed_changed();
        assert!(s.group_menu.is_none());
    }

    #[test]
    fn the_list_title_says_what_the_numbers_mean() {
        let target = mark(Side::Sell, 5823.00, false, Role::TakeProfit, false);
        let g = group(1, 200, 20, 5822.75, 5823.00, 0);
        let mut s = String::new();
        write_group_title(&mut s, &target, &g, &fmt()).unwrap();
        assert_eq!(s, "Target: 200 in 20 orders, 5822.75–5823.00");
    }

    /// The owner's case: LONG 1 @ 5810.00 with a 20/40 bracket on ES: the stop and the target
    /// say their role in words, their quantity apart from it, and what they make or lose there.
    #[test]
    fn exit_tags_say_role_quantity_price_and_pnl() {
        let mpu = money_per_unit(&es());
        let pos = Some((1, 581_000.0));
        let stop = mark(Side::Sell, 5805.00, true, Role::StopLoss, false);
        let target = mark(Side::Sell, 5820.00, false, Role::TakeProfit, false);
        let tag = |m: &OrderMark| {
            let mut s = String::new();
            write_order_tag(&mut s, m, &fmt()).unwrap();
            s
        };
        let pnl = |m: &OrderMark, mode| {
            let mut p = String::new();
            let money = write_exit_pnl(&mut p, m, pos, 25, mpu, mode).unwrap();
            (p, money)
        };
        assert_eq!(tag(&stop), "Stop 1 @ 5805.00");
        assert_eq!(tag(&target), "Target 1 @ 5820.00");
        assert_eq!(pnl(&stop, PnlDisplay::Currency).0, "-$250.00");
        assert_eq!(pnl(&target, PnlDisplay::Both).0, "+40.0 t  +$500.00");
        assert!(
            pnl(&target, PnlDisplay::Currency).1.unwrap().0 > 0,
            "its tone: up"
        );
        assert_eq!(
            pnl(&stop, PnlDisplay::Off),
            (String::new(), None),
            "P&L off: no amount"
        );
        // An entry has no P&L at its price, and reads side and type.
        let entry = mark(Side::Buy, 5800.00, false, Role::Single, false);
        assert_eq!(tag(&entry), "Buy lmt 1 @ 5800.00");
        assert_eq!(pnl(&entry, PnlDisplay::Currency).1, None);
        // A chart-added exit (reduce-only, plain role) reads by its kind.
        let added = mark(Side::Sell, 5805.00, true, Role::Single, true);
        assert_eq!(tag(&added), "Stop 1 @ 5805.00");
    }

    #[test]
    fn edge_markers_read_like_their_tags() {
        let target = mark(Side::Sell, 5820.00, false, Role::TakeProfit, false);
        let mut s = String::new();
        write_edge(&mut s, &target, true, 1, &fmt()).unwrap();
        assert_eq!(s, "↑ Target 1 @ 5820.00");
        s.clear();
        write_edge(&mut s, &target, false, 3, &fmt()).unwrap();
        assert_eq!(s, "↓ Target 1 @ 5820.00 +2 more");
        s.clear();
        write_edge_short(&mut s, &target, true).unwrap();
        assert_eq!(s, "↑Tgt", "no bare digit beside a role letter");
        s.clear();
        write_edge_priced(&mut s, &target, true, &fmt()).unwrap();
        assert_eq!(s, "↑Tgt 5820.00");
    }

    /// Adding an exit from the position: the closing side, the whole position, reduce-only;
    /// refused when it would fill or trigger at once by the DOM's rules (inclusive, against
    /// the bid for a long and the ask for a short).
    #[test]
    fn handles_place_reduce_only_exits_on_the_safe_side() {
        let a = AccountId::new("paper");
        let k = lines_trading::types::ContractDoc {
            product: 1,
            year: Some(2026),
            month: Some(12),
            equity: false,
            option: None,
        }
        .to_key()
        .unwrap();
        let p = |v: f64| Price((v * 100.0).round() as i64);
        // Bid 5809.75, ask 5810.00, last 5810.00 (traded at the ask).
        let mkt = (Some(p(5809.75)), Some(p(5810.00)), Some(p(5810.00)));
        let TradeCommand::PlaceExit(s) = exit_command(&a, k, 2, false, p(5805.0), mkt).unwrap()
        else {
            panic!("an exit")
        };
        assert_eq!((s.side, s.qty), (Side::Sell, Qty(2)));
        assert_eq!(s.kind, OrderKind::Stop { trigger: p(5805.0) });
        // Code review: a long's stop at the bid (one tick under the last) triggers on the next
        // print at the bid: refused; so is a target at the bid.
        assert!(
            exit_command(&a, k, 2, false, p(5809.75), mkt).is_err(),
            "stop at the bid"
        );
        assert!(
            exit_command(&a, k, 2, true, p(5809.75), mkt).is_err(),
            "target at the bid"
        );
        assert!(
            exit_command(&a, k, 2, true, p(5810.00), mkt).is_ok(),
            "above the bid"
        );
        // A short: the stop above the ask, the target below it, buying.
        let TradeCommand::PlaceExit(s) = exit_command(&a, k, -1, false, p(5815.0), mkt).unwrap()
        else {
            panic!("an exit")
        };
        assert_eq!((s.side, s.qty), (Side::Buy, Qty(1)));
        assert!(
            exit_command(&a, k, -1, false, p(5810.00), mkt).is_err(),
            "stop at the ask"
        );
        assert!(
            exit_command(&a, k, -1, true, p(5810.00), mkt).is_err(),
            "target at the ask"
        );
        assert!(
            exit_command(&a, k, 1, true, p(5815.0), (None, None, None)).is_err(),
            "no market"
        );
    }

    /// Review: a click (under 4 px) places nothing; released at the average, nothing; a kind the
    /// position already has is moved, never doubled.
    #[test]
    fn releasing_an_add_drag_places_moves_or_does_nothing() {
        use lines_trading::OrderId;
        let a = AddExit {
            target: true,
            to: Price(582_000),
        };
        let none = |_: bool| None;
        assert_eq!(
            release_action(None, 40.0, none),
            Release::Nothing,
            "at the average"
        );
        assert_eq!(
            release_action(Some(a), 2.0, none),
            Release::Nothing,
            "a click"
        );
        assert_eq!(release_action(Some(a), 40.0, none), Release::Place(a));
        let has_target = |t: bool| t.then_some(OrderId(9));
        assert_eq!(
            release_action(Some(a), 40.0, has_target),
            Release::Move {
                order: OrderId(9),
                to: Price(582_000)
            }
        );
        let stop = AddExit { target: false, ..a };
        assert_eq!(
            release_action(Some(stop), 40.0, has_target),
            Release::Place(stop)
        );
    }

    /// A chart shows no switch: its exits go out only through a linked DOM's switch, on the
    /// same account and contract; unlinked panes never share it.
    #[test]
    fn chart_exits_need_a_linked_enabled_dom() {
        let a = AccountId::new("paper");
        let b = AccountId::new("other");
        let k = lines_trading::types::ContractDoc {
            product: 1,
            year: Some(2026),
            month: Some(12),
            equity: false,
            option: None,
        }
        .to_key()
        .unwrap();
        assert!(linked_dom_enabled(
            Some(0),
            &a,
            k,
            [(Some(0), Some(&a), Some(k), true)]
        ));
        assert!(
            !linked_dom_enabled(Some(0), &a, k, [(Some(0), Some(&a), Some(k), false)]),
            "off"
        );
        assert!(
            !linked_dom_enabled(Some(0), &a, k, [(Some(1), Some(&a), Some(k), true)]),
            "other group"
        );
        assert!(
            !linked_dom_enabled(Some(0), &a, k, [(Some(0), Some(&b), Some(k), true)]),
            "other account"
        );
        assert!(
            !linked_dom_enabled(None, &a, k, [(None, Some(&a), Some(k), true)]),
            "unlinked"
        );
    }

    fn ctx() -> crate::dom_trade::EntryCtx {
        crate::dom_trade::EntryCtx {
            account: AccountId::new("paper"),
            contract: lines_core::ContractKey::monthly(
                lines_feed::catalog::ES,
                2026,
                lines_core::Month::new(12).unwrap(),
            ),
            qty: Qty(2),
            position: 0,
            bracket: None,
            tick: 25,
            best_bid: Some(Price(580_000)),
            best_ask: Some(Price(580_025)),
            last: Some(Price(580_000)),
        }
    }

    /// The strip builds exactly the DOM's commands: market as BUY/SELL MKT, a limit as a
    /// bid/ask column click at the crosshair price, with the chart's quantity and bracket.
    #[test]
    fn strip_orders_are_the_doms_intents() {
        use crate::dom_trade::{Arming, BracketPreset, DomColumn, QuickOrder, click, quick};
        let on = Arming::new(true);
        let mut c = ctx();
        let at = Some(Price(579_500));
        assert_eq!(
            strip_command(StripOrder::BuyMarket, at, &c),
            quick(QuickOrder::BuyMarket, on, &c)
        );
        assert_eq!(
            strip_command(StripOrder::SellMarket, None, &c),
            quick(QuickOrder::SellMarket, on, &c),
            "market needs no crosshair"
        );
        assert_eq!(
            strip_command(StripOrder::BuyLimit, at, &c),
            click(DomColumn::Bid, Price(579_500), false, on, &c)
        );
        let Ok(TradeCommand::PlaceOrder(o)) = strip_command(StripOrder::SellLimit, at, &c) else {
            panic!("a plain limit");
        };
        assert_eq!(
            (o.side, o.qty, o.kind),
            (
                Side::Sell,
                Qty(2),
                OrderKind::Limit {
                    price: Price(579_500)
                }
            )
        );
        assert_eq!(
            strip_command(StripOrder::BuyLimit, None, &c),
            Err(Refusal(PICK_A_PRICE))
        );
        // A bracket preset attaches as on the DOM.
        c.bracket = Some(BracketPreset {
            stop_ticks: 8,
            target_ticks: 16,
        });
        assert!(matches!(
            strip_command(StripOrder::BuyLimit, at, &c),
            Ok(TradeCommand::PlaceBracket(_))
        ));
        c.qty = Qty(0);
        assert!(
            strip_command(StripOrder::BuyMarket, at, &c).is_err(),
            "no quantity"
        );
    }

    #[test]
    fn the_resize_control_sits_left_of_its_tag() {
        let b = TagBox {
            id: 7,
            price: Price(600_000),
            line_y: 110.0,
            left: 200.0,
            top: 100.0,
            w: 150.0,
            h: lines_core::geometry::order_tags::TAG_H,
            close_left: 330.0,
        };
        assert_eq!(resize_hit(&b, 121.0, 105.0), Some(ResizeHit::Down));
        assert_eq!(resize_hit(&b, 160.0, 105.0), Some(ResizeHit::Value));
        assert_eq!(resize_hit(&b, 199.0, 105.0), Some(ResizeHit::Up));
        assert_eq!(resize_hit(&b, 119.0, 105.0), None, "left of it");
        assert_eq!(resize_hit(&b, 200.0, 105.0), None, "the tag's own");
        assert_eq!(resize_hit(&b, 160.0, 99.0), None, "above it");
        let edge = TagBox { left: 60.0, ..b };
        assert_eq!(resize_hit(&edge, 30.0, 105.0), None, "no room: none");
        // At 150 % UI zoom (a 30 px tag): 30, 60 and 30 px, hit the full height.
        let z = TagBox { h: 30.0, ..b };
        assert_eq!(resize_w(&z), 120.0);
        assert_eq!(resize_hit(&z, 81.0, 128.0), Some(ResizeHit::Down));
        assert_eq!(resize_hit(&z, 111.0, 105.0), Some(ResizeHit::Value));
        assert_eq!(resize_hit(&z, 171.0, 105.0), Some(ResizeHit::Up));
        assert_eq!(resize_hit(&z, 79.0, 105.0), None, "left of it");
        assert_eq!(resize_hit(&z, 160.0, 130.0), None, "below it");
    }

    #[test]
    fn a_limit_pick_arms_toggles_switches_and_drops_on_a_new_symbol() {
        let mut s = ChartTradeState::default();
        s.arm_pick(StripOrder::BuyLimit);
        assert_eq!(s.strip_pick, Some(StripOrder::BuyLimit));
        s.arm_pick(StripOrder::SellLimit);
        assert_eq!(
            s.strip_pick,
            Some(StripOrder::SellLimit),
            "the other side replaces it"
        );
        s.arm_pick(StripOrder::SellLimit);
        assert_eq!(s.strip_pick, None, "the same button again disarms");
        s.arm_pick(StripOrder::BuyLimit);
        s.feed_changed();
        assert_eq!(s.strip_pick, None, "another symbol drops it");
    }

    #[test]
    fn a_pick_sends_on_a_click_and_a_drag_pans() {
        assert!(is_click(0.0, 0.0));
        assert!(is_click(2.0, 3.0), "jitter under 4 px");
        assert!(!is_click(0.0, DRAG_PX), "4 px is a drag");
        assert!(!is_click(3.0, 3.0), "diagonal past 4 px");
    }

    #[test]
    fn the_strip_fits_the_plot_or_drops_its_limits_or_hides() {
        let full = strip_box(1000.0, 600.0, 1.0).unwrap();
        assert!(full.limits);
        assert_eq!(
            (full.x, full.y),
            (STRIP_MARGIN, 600.0 - STRIP_MARGIN - STRIP_H)
        );
        assert!(full.contains(full.x + 1.0, full.y + 1.0));
        assert!(!full.contains(full.x + full.w, full.y + 1.0));
        assert!(!full.contains(full.x + 1.0, full.y - 1.0));
        let narrow = strip_box(full.w, 600.0, 1.0).unwrap();
        assert!(!narrow.limits && narrow.w < full.w, "market only");
        assert_eq!(strip_box(narrow.w, 600.0, 1.0), None, "too narrow: hidden");
        assert_eq!(strip_box(1000.0, 30.0, 1.0), None, "too short: hidden");
    }

    /// The strip zooms with the UI (its text and its pointer box alike), never under 100 %.
    #[test]
    fn the_strip_zooms_with_the_ui() {
        let at = |z| strip_box(2000.0, 600.0, z).unwrap();
        let (one, big) = (at(1.0), at(1.5));
        assert_eq!(big.h(), (STRIP_H * 1.5).round());
        assert!(big.w > one.w * 1.45 && big.w < one.w * 1.55);
        assert!(big.contains(big.x + 1.0, big.y + big.h() - 1.0));
        assert_eq!(at(0.8), one, "a hit target never shrinks");
    }

    // ---- staged orders ----------------------------------------------------------------------

    fn buy_at(price: f64) -> MenuOrder {
        MenuOrder {
            side: Side::Buy,
            stop: false,
            price: p(price),
        }
    }

    fn sell_at(price: f64) -> MenuOrder {
        MenuOrder {
            side: Side::Sell,
            stop: false,
            price: p(price),
        }
    }

    /// Bid 6012.00, ask 6012.25; 2 lots; a 2/4 preset when `bracket`.
    fn staged(position: i64, bracket: bool, o: MenuOrder) -> (Staged, crate::dom_trade::EntryCtx) {
        let c = menu_ctx(position, bracket);
        (stage(o, Offer::of(&c), &c).unwrap(), c)
    }

    /// The legs start at the preset's distances (the DOM's math), or none without one.
    #[test]
    fn staging_starts_its_legs_at_the_preset() {
        let (s, _) = staged(0, true, buy_at(6010.00));
        assert_eq!((s.stop, s.target), (Some(p(6009.50)), Some(p(6011.00))));
        assert!(s.bracketable);
        assert_eq!(s.wanted_handles(), [false, false]);
        let (s, _) = staged(0, true, sell_at(6014.00));
        assert_eq!((s.stop, s.target), (Some(p(6014.50)), Some(p(6013.00))));
        // No preset: no legs, and both handles (a buy's target above, stop below).
        let (s, _) = staged(0, false, buy_at(6010.00));
        assert_eq!((s.stop, s.target), (None, None));
        assert_eq!(s.wanted_handles(), [true, true]);
        assert_eq!(s.handle_part(true), StagedPart::Target);
        assert_eq!(s.handle_part(false), StagedPart::Stop);
        let (s, _) = staged(0, false, sell_at(6014.00));
        assert_eq!(s.handle_part(true), StagedPart::Stop);
    }

    /// A reducing order stages with no legs and no handles, as the menu attaches no bracket.
    #[test]
    fn a_reducing_order_stages_without_legs() {
        let (mut s, c) = staged(2, true, sell_at(6014.00));
        assert!(!s.bracketable);
        assert_eq!((s.stop, s.target), (None, None));
        assert_eq!(s.wanted_handles(), [false, false]);
        s.move_part(StagedPart::Stop, p(6016.00));
        assert_eq!(s.stop, None, "no leg can be dragged out");
        assert!(matches!(
            staged_command(&s, &c),
            Ok(TradeCommand::PlaceOrder(_))
        ));
    }

    /// Staging is refused as the menu row is: the chart changed, or the market moved.
    #[test]
    fn staging_is_refused_like_the_row() {
        let c = menu_ctx(0, true);
        let other = Offer {
            qty: Qty(3),
            ..Offer::of(&c)
        };
        assert_eq!(
            stage(buy_at(6010.00), other, &c),
            Err(Refusal(CHART_CHANGED))
        );
        assert_eq!(
            stage(buy_at(6012.25), Offer::of(&c), &c),
            Err(Refusal(MARKET_MOVED)),
            "a buy limit at the ask would fill at once"
        );
    }

    /// Dragging snaps to ticks (the chart's `snap_price`); the entry carries its legs.
    #[test]
    fn dragging_snaps_and_the_entry_takes_its_legs() {
        let vp = lines_core::Viewport {
            price_lo: 600_000.0,
            price_hi: 602_000.0,
            height: 400.0,
            ..lines_core::Viewport::default()
        };
        let (mut s, _) = staged(0, true, buy_at(6010.00));
        // 5 units a px: 6.3 px from the top is off the grid, and lands on a 25-unit tick.
        let to = lines_core::geometry::snap_price(&vp, 6.3, 25).unwrap();
        assert_eq!(to.0 % 25, 0);
        s.move_part(StagedPart::Target, to);
        assert_eq!(s.target, Some(to));
        s.move_part(StagedPart::Stop, p(6008.00));
        s.move_part(StagedPart::Entry, p(6009.00));
        assert_eq!(s.order.price, p(6009.00));
        assert_eq!(s.stop, Some(p(6007.00)));
        assert_eq!(s.target, Some(Price(to.0 - 100)));
    }

    /// × on a leg drops it and its handle comes back; × on the entry discards the order.
    #[test]
    fn x_on_a_leg_drops_it() {
        let (mut s, c) = staged(0, true, buy_at(6010.00));
        assert!(!s.remove(StagedPart::Target));
        assert_eq!(s.target, None);
        assert_eq!(s.wanted_handles(), [true, false]);
        let Ok(TradeCommand::PlaceBracket(b)) = staged_command(&s, &c) else {
            panic!("a stop is still there: a bracket")
        };
        assert_eq!((b.stop_loss, b.take_profit), (Some(p(6009.50)), None));
        assert!(!s.remove(StagedPart::Stop));
        assert!(matches!(
            staged_command(&s, &c),
            Ok(TradeCommand::PlaceOrder(_))
        ));
        assert!(s.remove(StagedPart::Entry), "the entry's × discards");
    }

    /// Place sends one bracket with exactly the chart's legs (never the preset's), or a plain
    /// order with none; the entry is the DOM's click (a limit here, a stop with its far side).
    #[test]
    fn place_builds_one_command() {
        let (mut s, c) = staged(0, true, buy_at(6010.00));
        s.move_part(StagedPart::Target, p(6018.00));
        s.move_part(StagedPart::Stop, p(6006.00));
        let Ok(TradeCommand::PlaceBracket(b)) = staged_command(&s, &c) else {
            panic!("bracket")
        };
        assert_eq!(b.take_profit, Some(p(6018.00)));
        assert_eq!(b.stop_loss, Some(p(6006.00)));
        assert_eq!(
            (b.entry.side, b.entry.qty, b.entry.kind),
            (Side::Buy, Qty(2), OrderKind::Limit { price: p(6010.00) })
        );
        let stop_entry = MenuOrder {
            side: Side::Buy,
            stop: true,
            price: p(6014.00),
        };
        let (s, c) = staged(0, false, stop_entry);
        let Ok(TradeCommand::PlaceOrder(spec)) = staged_command(&s, &c) else {
            panic!("plain")
        };
        assert_eq!(
            spec.kind,
            OrderKind::Stop {
                trigger: p(6014.00)
            }
        );
    }

    /// Each refusal at Place, nothing built: the contract, size or account changed; the market
    /// moved through the entry; a leg on the wrong side; legs on an order that now reduces.
    #[test]
    fn place_refuses_what_changed() {
        let (s, c) = staged(0, true, buy_at(6010.00));
        let mut other = c.clone();
        other.contract = ContractKey::monthly(
            lines_feed::catalog::ES,
            2027,
            lines_core::Month::new(3).unwrap(),
        );
        assert_eq!(staged_command(&s, &other), Err(Refusal(CHART_CHANGED)));
        let mut size = c.clone();
        size.qty = Qty(1);
        assert_eq!(staged_command(&s, &size), Err(Refusal(CHART_CHANGED)));
        let mut acct = c.clone();
        acct.account = AccountId::new("other");
        assert_eq!(staged_command(&s, &acct), Err(Refusal(CHART_CHANGED)));
        // A preset changed since staging doesn't matter: the legs are the chart's.
        let mut preset = c.clone();
        preset.bracket = None;
        assert!(staged_command(&s, &preset).is_ok());
        let mut moved = c.clone();
        (moved.best_bid, moved.best_ask) = (Some(p(6009.75)), Some(p(6010.00)));
        assert_eq!(staged_command(&s, &moved), Err(Refusal(STAGED_CROSSES)));
        // Dragged through the market by the trader: the same, and dragging it back places.
        let mut dragged = s;
        dragged.order.price = p(6012.50);
        assert_eq!(staged_command(&dragged, &c), Err(Refusal(STAGED_CROSSES)));
        let mut wrong = s;
        wrong.stop = Some(p(6010.25));
        assert_eq!(staged_command(&wrong, &c), Err(Refusal(STOP_ABOVE)));
        wrong.stop = Some(p(6010.00));
        assert_eq!(
            staged_command(&wrong, &c),
            Err(Refusal(STOP_ABOVE)),
            "at it"
        );
        let mut wrong = s;
        wrong.target = Some(p(6009.00));
        assert_eq!(staged_command(&wrong, &c), Err(Refusal(TARGET_BELOW)));
        let (mut ss, sc) = staged(0, true, sell_at(6014.00));
        ss.stop = Some(p(6013.00));
        assert_eq!(staged_command(&ss, &sc), Err(Refusal(STOP_BELOW)));
        ss.stop = Some(p(6015.00));
        ss.target = Some(p(6015.00));
        assert_eq!(staged_command(&ss, &sc), Err(Refusal(TARGET_ABOVE)));
        // A fill since staging: the buy now reduces a short, so its legs can't go.
        let mut short = c.clone();
        short.position = -3;
        assert_eq!(staged_command(&s, &short), Err(Refusal(POSITION_CHANGED)));
    }

    /// The tags' words, P&L on (ticks and money, as the display shows them) and off (no money
    /// and no ticks, the ratio kept).
    #[test]
    fn staged_text_follows_the_pnl_display() {
        let e = es();
        let mpu = money_per_unit(&e);
        let (mut s, _) = staged(0, true, buy_at(6010.00));
        s.stop = Some(p(6006.00));
        s.target = Some(p(6018.00));
        let entry = |s: &Staged, mode| {
            let mut out = String::new();
            write_staged_entry(&mut out, s, &e.price, e.qty, mpu, mode).unwrap();
            out
        };
        assert_eq!(
            entry(&s, PnlDisplay::Both),
            "▲ Buy limit 2 @ 6010.00 · risk -16.0 t  -$400.00 · 1 : 2.0"
        );
        assert_eq!(
            entry(&s, PnlDisplay::Currency),
            "▲ Buy limit 2 @ 6010.00 · risk -$400.00 · 1 : 2.0"
        );
        assert_eq!(
            entry(&s, PnlDisplay::Off),
            "▲ Buy limit 2 @ 6010.00 · 1 : 2.0"
        );
        let leg = |part, mode| {
            let (mut text, mut pnl) = (String::new(), String::new());
            let money = write_staged_leg(&mut text, &mut pnl, &s, part, &e.price, e.qty, mpu, mode)
                .unwrap();
            (text, pnl, money)
        };
        assert_eq!(
            leg(StagedPart::Target, PnlDisplay::Both),
            (
                "Target 2 @ 6018.00".to_owned(),
                "+32.0 t  +$800.00".to_owned(),
                Some(Money(800 * Money::UNITS_PER_WHOLE))
            )
        );
        assert_eq!(leg(StagedPart::Stop, PnlDisplay::Ticks).1, "-16.0 t");
        assert_eq!(
            leg(StagedPart::Stop, PnlDisplay::Off),
            ("Stop 2 @ 6006.00".to_owned(), String::new(), None)
        );
        // One leg: no ratio.
        let mut one = s;
        one.target = None;
        assert_eq!(
            entry(&one, PnlDisplay::Ticks),
            "▲ Buy limit 2 @ 6010.00 · risk -16.0 t"
        );
    }

    /// The entry's tag ends in Place, then × Esc; a leg's in its ×. Other tags aren't staged.
    #[test]
    fn staged_tags_hit_their_buttons() {
        let tag = |id, top: f32| TagBox {
            id,
            price: p(6010.00),
            line_y: top + 10.0,
            left: 100.0,
            top,
            w: 200.0,
            h: 20.0,
            close_left: 278.0,
        };
        let tags = [
            tag(STAGED_ENTRY_ID, 100.0),
            tag(STAGED_TARGET_ID, 50.0),
            tag(7, 150.0),
        ];
        let b = (60.0, 40.0);
        assert_eq!(
            staged_hit(&tags, b, 110.0, 105.0),
            Some(StagedHit::Body(StagedPart::Entry))
        );
        assert_eq!(staged_hit(&tags, b, 210.0, 105.0), Some(StagedHit::Place));
        assert_eq!(
            staged_hit(&tags, b, 270.0, 105.0),
            Some(StagedHit::Close(StagedPart::Entry))
        );
        assert_eq!(
            staged_hit(&tags, b, 290.0, 55.0),
            Some(StagedHit::Close(StagedPart::Target))
        );
        assert_eq!(
            staged_hit(&tags, b, 110.0, 55.0),
            Some(StagedHit::Body(StagedPart::Target))
        );
        assert_eq!(
            staged_hit(&tags, b, 110.0, 155.0),
            None,
            "a working order's tag"
        );
        assert_eq!(staged_hit(&tags, b, 50.0, 105.0), None);
    }

    /// Nothing reaches the broker before Place: staging, dragging and discarding build no
    /// command; a symbol or account change discards with its notice; Place takes it off the
    /// chart, and a refusal from the window puts it back.
    #[test]
    fn nothing_is_sent_before_place() {
        let c = menu_ctx(0, true);
        let fresh = || stage(buy_at(6010.00), Offer::of(&c), &c).unwrap();
        let mut st = ChartTradeState {
            staged: Some(fresh()),
            ..ChartTradeState::default()
        };
        if let Some(s) = st.staged.as_mut() {
            s.move_part(StagedPart::Stop, p(6008.00));
        }
        assert_eq!(st.discard(DISCARDED_HIDDEN), Some(DISCARDED_HIDDEN));
        assert_eq!(st.staged, None);
        assert_eq!(
            st.discard(DISCARDED_HIDDEN),
            None,
            "nothing staged: no notice"
        );
        st.staged = Some(fresh());
        assert_eq!(st.staged_account(Some(&c.account)), None);
        assert_eq!(
            st.staged_account(Some(&AccountId::new("other"))),
            Some(DISCARDED_ACCOUNT)
        );
        st.staged = Some(fresh());
        assert_eq!(st.feed_changed(), Some(DISCARDED_SYMBOL));
        st.staged = Some(fresh());
        st.placed();
        assert!(st.staged.is_none() && st.placing.is_some());
        st.place_refused();
        assert!(st.staged.is_some() && st.placing.is_none());
        // Sent: a refusal after that (another chart order's) never brings it back.
        st.placed();
        st.place_sent();
        st.place_refused();
        assert!(st.staged.is_none() && st.placing.is_none());
    }

    #[test]
    fn a_text_field_keeps_its_own_enter_and_esc() {
        for key in ["enter", "escape"] {
            assert!(staged_takes_key(true, key, true, false));
            // A size, a resize, a note or a typed timeframe open: the key is the field's, so
            // a stray Enter never places and Esc never discards.
            assert!(
                !staged_takes_key(true, key, true, true),
                "{key} while typing"
            );
            assert!(
                !staged_takes_key(true, key, false, false),
                "{key} with a modifier"
            );
            assert!(
                !staged_takes_key(false, key, true, false),
                "{key} with nothing staged"
            );
        }
        assert!(!staged_takes_key(true, "a", true, false));
    }

    #[test]
    fn a_stage_spec_parses() {
        let f = es().price;
        let parse = |s: &str| parse_stage_spec(s, |x| f.scale.parse_decimal(x).ok().map(Price));
        assert_eq!(
            parse("buy limit 6010.00 stop 6006.00 target 6018.00"),
            Ok(StageSpec {
                order: buy_at(6010.00),
                stop: Some(p(6006.00)),
                target: Some(p(6018.00)),
            })
        );
        assert!(parse("sell stop 6000.00").unwrap().order.stop);
        assert!(parse("buy limit").is_err());
        assert!(parse("buy limit 6010.00 stop").is_err());
        assert!(parse("hold limit 6010.00").is_err());
    }
}
