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
use crate::pnl::PnlDisplay;

/// Shown when a handle is dragged with no linked DOM enabled (a chart shows no switch).
pub const ENABLE_TO_ADD: &str = "Enable trading on the linked DOM to add a stop or target";

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
        write!(out, " +{}", n - 1)?;
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

use lines_core::geometry::order_tags::{EdgeMarker, Handle, LineIn, MAX_TAGS, TagBox};

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
    pub lines: Vec<LineIn>,
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

/// Strip height, margin from the plot's bottom-left corner, and its pieces' widths (fixed, so
/// the pointer test needs no layout): quantity, two market buttons, two limit buttons.
pub const STRIP_H: f32 = 26.0;
pub const STRIP_MARGIN: f32 = 8.0;
pub const STRIP_PAD: f32 = 3.0;
pub const STRIP_GAP: f32 = 4.0;
pub const STRIP_QTY_W: f32 = 56.0;
pub const STRIP_MKT_W: f32 = 76.0;
pub const STRIP_LMT_W: f32 = 136.0;

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

impl Default for ChartTradeState {
    fn default() -> Self {
        ChartTradeState {
            marks: Vec::with_capacity(MAX_TAGS),
            texts: Vec::with_capacity(MAX_TAGS),
            lines: Vec::with_capacity(MAX_TAGS),
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
        self.marks.extend(
            s.open_orders(account, contract)
                .filter_map(order_mark)
                .take(MAX_TAGS),
        );
        self.position = s
            .position(account, contract)
            .and_then(|p| p.avg_price.map(|a| (p.qty, a)))
            .filter(|p| p.0 != 0);
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
    /// its click never sends at a price read on another product's scale.
    pub fn feed_changed(&mut self) {
        self.strip_pick = None;
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
        assert_eq!(s, "↓ Target 1 @ 5820.00 +2");
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
            working: 0,
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
