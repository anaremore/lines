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

/// Shown when a handle is dragged with trading off.
pub const ENABLE_TO_ADD: &str = "Enable trading to add a stop or target";

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

/// An order's tag: `S1 · 5806.50`, `T1 · 5820.00`, `BUY LMT 2 · 5999.50`, `SELL STP 1/2 · …`
/// (partially filled), then ` · held` for a bracket exit waiting on its entry, then the exit's
/// P&L at its price (`pnl`, already written per the P&L display; empty: none). No allocation.
pub fn write_order_tag(
    out: &mut impl Write,
    m: &OrderMark,
    fmt: &PriceFormat,
    pnl: &str,
) -> fmt::Result {
    let left = m.qty.0 - m.filled.0;
    match MarkRole::of(m) {
        MarkRole::Target => write!(out, "T{left}")?,
        MarkRole::Stop => write!(out, "S{left}")?,
        MarkRole::Entry => {
            let side = if m.side == Side::Buy { "BUY" } else { "SELL" };
            let kind = if m.stop { "STP" } else { "LMT" };
            if m.style == MarkStyle::Partial {
                write!(out, "{side} {kind} {}/{}", m.filled.0, m.qty.0)?;
            } else {
                write!(out, "{side} {kind} {left}")?;
            }
        }
    }
    out.write_str(" · ")?;
    fmt.write(m.price, out)?;
    if m.style == MarkStyle::Held {
        out.write_str(" · held")?;
    }
    if !pnl.is_empty() {
        out.write_str(" · ")?;
        out.write_str(pnl)?;
    }
    Ok(())
}

/// An exit's P&L at its price as the P&L display shows it (ticks, currency, both; nothing when
/// off or for an order that doesn't close the position).
#[allow(clippy::too_many_arguments)]
pub fn write_exit_pnl(
    out: &mut impl Write,
    m: &OrderMark,
    position: Option<(i64, f64)>,
    tick: i64,
    mpu: Option<i128>,
    mode: PnlDisplay,
) -> Result<bool, fmt::Error> {
    let (Some((pos, avg)), Some(mpu)) = (position, mpu) else {
        return Ok(false);
    };
    // Only orders that close (part of) the position: the opposite side.
    if pos == 0 || (pos > 0) == (m.side == Side::Buy) {
        return Ok(false);
    }
    let qty = (m.qty.0 - m.filled.0).min(pos.abs());
    let (ticks, money) = exit_pnl(pos, avg, m.price, qty, tick, mpu);
    crate::pnl::write_pnl(out, mode, Some(ticks), money)
}

/// The edge marker for an order off the plot: `↑ T1 5820.00`, `↓ S1 5795.00`, and `+N` for
/// more beyond that edge. No allocation.
pub fn write_edge(
    out: &mut impl Write,
    m: &OrderMark,
    above: bool,
    n: usize,
    fmt: &PriceFormat,
) -> fmt::Result {
    out.write_str(if above { "↑ " } else { "↓ " })?;
    let left = m.qty.0 - m.filled.0;
    match MarkRole::of(m) {
        MarkRole::Target => write!(out, "T{left} ")?,
        MarkRole::Stop => write!(out, "S{left} ")?,
        MarkRole::Entry => write!(
            out,
            "{} {left} ",
            if m.side == Side::Buy { "BUY" } else { "SELL" }
        )?,
    }
    fmt.write(m.price, out)?;
    if n > 1 {
        write!(out, " +{}", n - 1)?;
    }
    Ok(())
}

/// The exit a position handle places at `price`: a target (limit) or a stop for the whole
/// position, reduce-only (`PlaceExit`). Refused when trading is off, or when it would fill or
/// trigger at once: a long's target must be above the market and its stop below (a short's
/// the other way round). `market`: the last trade, else the mid.
#[allow(clippy::too_many_arguments)]
pub fn exit_command(
    enabled: bool,
    account: &AccountId,
    contract: ContractKey,
    position: i64,
    target: bool,
    price: Price,
    market: Option<Price>,
) -> Result<TradeCommand, Refusal> {
    if !enabled {
        return Err(Refusal(ENABLE_TO_ADD));
    }
    if position == 0 {
        return Err(Refusal("no position to protect"));
    }
    let long = position > 0;
    let market = market.ok_or(Refusal("no market price yet"))?;
    // Profit side: above the market for a long, below for a short.
    let profit_side = if long { price > market } else { price < market };
    match (target, profit_side) {
        (true, false) => {
            return Err(Refusal(if long {
                "a target must be above the market"
            } else {
                "a target must be below the market"
            }));
        }
        (false, true) => {
            return Err(Refusal(if long {
                "a stop must be below the market"
            } else {
                "a stop must be above the market"
            }));
        }
        _ => {}
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
    /// `S1 · 5806.50 · -$175.00`.
    pub text: FixedStr<96>,
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

    /// The order behind a tag id (open, as of this frame).
    pub fn mark(&self, id: u64) -> Option<&OrderMark> {
        self.marks.iter().find(|m| m.id.0 == id)
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

    /// The owner's case: LONG 1 @ 5810.00 with a 20/40 bracket on ES: the stop at 5805.00 and
    /// the target at 5820.00 say what they make or lose there.
    #[test]
    fn exit_tags_say_role_price_and_pnl_at_that_price() {
        let mpu = money_per_unit(&es());
        let pos = Some((1, 581_000.0));
        let stop = mark(Side::Sell, 5805.00, true, Role::StopLoss, false);
        let target = mark(Side::Sell, 5820.00, false, Role::TakeProfit, false);
        let tag = |m: &OrderMark, mode| {
            let mut p = String::new();
            write_exit_pnl(&mut p, m, pos, 25, mpu, mode).unwrap();
            let mut s = String::new();
            write_order_tag(&mut s, m, &fmt(), &p).unwrap();
            s
        };
        assert_eq!(tag(&stop, PnlDisplay::Currency), "S1 · 5805.00 · -$250.00");
        assert_eq!(
            tag(&target, PnlDisplay::Currency),
            "T1 · 5820.00 · +$500.00"
        );
        assert_eq!(
            tag(&target, PnlDisplay::Both),
            "T1 · 5820.00 · +40.0 t  +$500.00"
        );
        assert_eq!(
            tag(&stop, PnlDisplay::Off),
            "S1 · 5805.00",
            "P&L off: no amount"
        );
        // An entry has no P&L at its price.
        let entry = mark(Side::Buy, 5800.00, false, Role::Single, false);
        assert_eq!(tag(&entry, PnlDisplay::Currency), "BUY LMT 1 · 5800.00");
        // A chart-added exit (reduce-only, plain role) reads by its kind: stop S, limit T.
        let added = mark(Side::Sell, 5805.00, true, Role::Single, true);
        assert_eq!(tag(&added, PnlDisplay::Off), "S1 · 5805.00");
    }

    #[test]
    fn edge_markers_name_the_order_and_count_the_rest() {
        let target = mark(Side::Sell, 5820.00, false, Role::TakeProfit, false);
        let mut s = String::new();
        write_edge(&mut s, &target, true, 1, &fmt()).unwrap();
        assert_eq!(s, "↑ T1 5820.00");
        s.clear();
        write_edge(&mut s, &target, false, 3, &fmt()).unwrap();
        assert_eq!(s, "↓ T1 5820.00 +2");
    }

    /// Adding an exit from the position: trading enabled, the closing side, the whole
    /// position, reduce-only; refused when it would fill or trigger at once.
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
        let m = Some(p(5810.00));
        assert_eq!(
            exit_command(false, &a, k, 1, false, p(5805.0), m),
            Err(Refusal(ENABLE_TO_ADD))
        );
        let TradeCommand::PlaceExit(s) = exit_command(true, &a, k, 2, false, p(5805.0), m).unwrap()
        else {
            panic!("an exit")
        };
        assert_eq!((s.side, s.qty), (Side::Sell, Qty(2)));
        assert_eq!(s.kind, OrderKind::Stop { trigger: p(5805.0) });
        let TradeCommand::PlaceExit(s) = exit_command(true, &a, k, 2, true, p(5820.0), m).unwrap()
        else {
            panic!("an exit")
        };
        assert_eq!(s.kind, OrderKind::Limit { price: p(5820.0) });
        // Would fill or trigger at once: refused.
        assert!(exit_command(true, &a, k, 2, true, p(5805.0), m).is_err());
        assert!(exit_command(true, &a, k, 2, false, p(5815.0), m).is_err());
        // A short: the stop above, the target below, buying.
        let TradeCommand::PlaceExit(s) =
            exit_command(true, &a, k, -1, false, p(5815.0), m).unwrap()
        else {
            panic!("an exit")
        };
        assert_eq!((s.side, s.qty), (Side::Buy, Qty(1)));
        assert!(exit_command(true, &a, k, -1, true, p(5815.0), m).is_err());
        assert!(
            exit_command(true, &a, k, 1, true, p(5815.0), None).is_err(),
            "no market"
        );
    }
}
