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
    /// Plot-local top of the tag pressed (the list opens under it).
    pub top: f32,
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
}

/// The resize control left of a tag: −, the size, + (each at least 20 px, a tag's height).
pub const RESIZE_BTN_W: f32 = 20.0;
pub const RESIZE_VALUE_W: f32 = 40.0;
pub const RESIZE_W: f32 = 2.0 * RESIZE_BTN_W + RESIZE_VALUE_W;

/// The piece of the resize control of tag `b` at plot-local (`x`, `y`); `None` off it, or when
/// the tag sits too near the plot's left edge to have one.
pub fn resize_hit(b: &TagBox, x: f32, y: f32) -> Option<ResizeHit> {
    let left = b.left - RESIZE_W;
    if left < 0.0
        || y < b.top
        || y >= b.top + lines_core::geometry::order_tags::TAG_H
        || x < left
        || x >= b.left
    {
        return None;
    }
    Some(if x < left + RESIZE_BTN_W {
        ResizeHit::Down
    } else if x < left + RESIZE_BTN_W + RESIZE_VALUE_W {
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
            if let Some(b) = ctx.bracket
                && !flat
                && crate::dom_trade::opens_or_adds(o.side, ctx.position)
            {
                let _ = write!(label, " · bracket {}/{}", b.stop_ticks, b.target_ticks);
            }
            (label, *o)
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

/// Whether a press released `dx`, `dy` px from where it started was a click (a pick sends) or a
/// drag (a pan: the pick stays armed), by the chart's click rule ([`DRAG_PX`]).
pub fn is_click(dx: f32, dy: f32) -> bool {
    dx.hypot(dy) < DRAG_PX
}

/// Strip height, margin from the plot's bottom-left corner, and its pieces' widths (fixed, so
/// the pointer test needs no layout): quantity, two market buttons, two limit buttons.
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
}

impl StripBox {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + STRIP_H
    }
}

/// The strip's box in a `plot_w` x `plot_h` plot: bottom-left, inside the margin; `None` when
/// the plot is too small for it.
pub fn strip_box(plot_w: f32, plot_h: f32) -> Option<StripBox> {
    let market = STRIP_PAD * 2.0 + STRIP_QTY_W + 2.0 * (STRIP_GAP + STRIP_MKT_W);
    let full = market + 2.0 * (STRIP_GAP + STRIP_LMT_W);
    let room = plot_w - 2.0 * STRIP_MARGIN;
    let w = if room >= full {
        full
    } else if room >= market {
        market
    } else {
        return None;
    };
    let y = plot_h - STRIP_MARGIN - STRIP_H;
    (y >= STRIP_MARGIN).then_some(StripBox {
        x: STRIP_MARGIN,
        y,
        w,
        limits: w == full,
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
            tag_lines: Vec::with_capacity(MAX_TAGS),
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
    pub fn feed_changed(&mut self) {
        self.strip_pick = None;
        self.group_menu = None;
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
        let full = strip_box(1000.0, 600.0).unwrap();
        assert!(full.limits);
        assert_eq!(
            (full.x, full.y),
            (STRIP_MARGIN, 600.0 - STRIP_MARGIN - STRIP_H)
        );
        assert!(full.contains(full.x + 1.0, full.y + 1.0));
        assert!(!full.contains(full.x + full.w, full.y + 1.0));
        assert!(!full.contains(full.x + 1.0, full.y - 1.0));
        let narrow = strip_box(full.w, 600.0).unwrap();
        assert!(!narrow.limits && narrow.w < full.w, "market only");
        assert_eq!(strip_box(narrow.w, 600.0), None, "too narrow: hidden");
        assert_eq!(strip_box(1000.0, 30.0), None, "too short: hidden");
    }
}
