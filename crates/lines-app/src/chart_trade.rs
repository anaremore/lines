//! Chart trading, without GPUI (roadmap 8): what a Ctrl+click, a menu choice or a tag means on
//! the chart, and what the order and position tags say. The chart view measures and draws;
//! `lines_core::order_lines` lays out and hit-tests; orders are built by
//! [`crate::dom_trade::click`], so brackets, stop sides and arming follow exactly the DOM's
//! rules.
//!
//! **Placing** (armed only): Ctrl+click is a limit at the snapped price (the crosshair's):
//! above the market a sell, below it a buy. Ctrl+Shift+click is a stop on the other side: above
//! the market a buy stop, below it a sell stop. The right-click menu offers the same choices at
//! the pointer's price. "The market" is the last trade, else the mid.

use std::fmt::{self, Write};

use lines_core::{Price, PriceFormat};
use lines_trading::{Command as TradeCommand, Side};
use serde::{Deserialize, Serialize};

use crate::dom_trade::{
    ARM_TO_TRADE, Arming, DomColumn, EntryCtx, MarkRole, MarkStyle, OrderMark, Refusal, click,
};

/// An order type at a price, as the chart's menu offers it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderPick {
    BuyLimit,
    SellLimit,
    BuyStop,
    SellStop,
}

impl OrderPick {
    pub fn label(self) -> &'static str {
        match self {
            OrderPick::BuyLimit => "Buy limit",
            OrderPick::SellLimit => "Sell limit",
            OrderPick::BuyStop => "Buy stop",
            OrderPick::SellStop => "Sell stop",
        }
    }
}

/// The market a chart order is placed against: the last trade, else the mid.
pub fn reference(ctx: &EntryCtx) -> Option<Price> {
    ctx.last.or(match (ctx.best_bid, ctx.best_ask) {
        (Some(b), Some(a)) => Some(Price((b.0 + a.0).div_euclid(2))),
        (b, a) => b.or(a),
    })
}

/// What Ctrl+click (`stop`: with Shift) at `price` places.
pub fn pick_for_click(price: Price, market: Price, stop: bool) -> Result<OrderPick, Refusal> {
    let above = price.0 > market.0;
    if price == market {
        return Err(Refusal("at the market: use Buy/Sell Mkt"));
    }
    Ok(match (above, stop) {
        (true, false) => OrderPick::SellLimit,
        (false, false) => OrderPick::BuyLimit,
        (true, true) => OrderPick::BuyStop,
        (false, true) => OrderPick::SellStop,
    })
}

/// The menu's choices at `price`: the limit and the stop that make sense on that side of the
/// market (at the market: both limits).
pub fn menu_picks(price: Price, market: Price) -> [OrderPick; 2] {
    match price.0.cmp(&market.0) {
        std::cmp::Ordering::Greater => [OrderPick::SellLimit, OrderPick::BuyStop],
        std::cmp::Ordering::Less => [OrderPick::BuyLimit, OrderPick::SellStop],
        std::cmp::Ordering::Equal => [OrderPick::BuyLimit, OrderPick::SellLimit],
    }
}

/// The command for `pick` at `price`. Disarmed: "Arm to trade", nothing built.
pub fn pick_command(
    pick: OrderPick,
    price: Price,
    arming: Arming,
    ctx: &EntryCtx,
) -> Result<TradeCommand, Refusal> {
    if !arming.is_armed() {
        return Err(Refusal(ARM_TO_TRADE));
    }
    let (col, stop) = match pick {
        OrderPick::BuyLimit => (DomColumn::Bid, false),
        OrderPick::SellLimit => (DomColumn::Ask, false),
        OrderPick::BuyStop => (DomColumn::Bid, true),
        OrderPick::SellStop => (DomColumn::Ask, true),
    };
    click(col, price, stop, arming, ctx)
}

/// An order's tag: `BUY LMT 2 @ 5999.50`, `SELL STP 1 @ …`; bracket exits `T 1 @ …`, `S 1 @ …`;
/// partially filled `filled/qty`; held (bracket exit waiting) `held`. No allocation.
pub fn write_order_tag(out: &mut impl Write, m: &OrderMark, fmt: &PriceFormat) -> fmt::Result {
    let left = m.qty.0 - m.filled.0;
    match MarkRole::of(m) {
        MarkRole::Target => write!(out, "T {left}")?,
        MarkRole::Stop => write!(out, "S {left}")?,
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
    out.write_str(" @ ")?;
    fmt.write(m.price, out)?;
    if m.style == MarkStyle::Held {
        out.write_str(" held")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{DisplayStyle, PriceScale, Qty};
    use lines_trading::order::Role;
    use lines_trading::{AccountId, OrderId, OrderKind, types::ContractDoc};

    const ES: i64 = 25;

    fn ctx() -> EntryCtx {
        EntryCtx {
            account: AccountId::new("paper"),
            contract: ContractDoc {
                product: 1,
                year: Some(2026),
                month: Some(12),
            }
            .to_key()
            .unwrap(),
            qty: Qty(1),
            position: 0,
            bracket: None,
            tick: ES,
            best_bid: Some(Price(600_000)),
            best_ask: Some(Price(600_025)),
            last: Some(Price(600_000)),
        }
    }

    fn armed() -> Arming {
        let mut a = Arming::default();
        a.apply(
            crate::dom_trade::ArmEvent::Toggle,
            crate::dom_trade::ArmPrefs::default(),
            true,
        );
        a
    }

    fn kind_side(c: &TradeCommand) -> (Side, OrderKind) {
        match c {
            TradeCommand::PlaceOrder(o) => (o.side, o.kind),
            TradeCommand::PlaceBracket(b) => (b.entry.side, b.entry.kind),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ctrl_click_sells_above_buys_below_and_shift_stops_the_other_side() {
        let m = Price(600_000);
        assert_eq!(
            pick_for_click(Price(600_100), m, false),
            Ok(OrderPick::SellLimit)
        );
        assert_eq!(
            pick_for_click(Price(599_900), m, false),
            Ok(OrderPick::BuyLimit)
        );
        assert_eq!(
            pick_for_click(Price(600_100), m, true),
            Ok(OrderPick::BuyStop)
        );
        assert_eq!(
            pick_for_click(Price(599_900), m, true),
            Ok(OrderPick::SellStop)
        );
        assert!(pick_for_click(m, m, false).is_err(), "at the market");
        // The commands: exactly the DOM's (limit at the price, stop at the price).
        let c = ctx();
        let k = |p, price| kind_side(&pick_command(p, Price(price), armed(), &c).unwrap());
        assert_eq!(
            k(OrderPick::SellLimit, 600_100),
            (
                Side::Sell,
                OrderKind::Limit {
                    price: Price(600_100)
                }
            )
        );
        assert_eq!(
            k(OrderPick::BuyLimit, 599_900),
            (
                Side::Buy,
                OrderKind::Limit {
                    price: Price(599_900)
                }
            )
        );
        assert_eq!(
            k(OrderPick::BuyStop, 600_100),
            (
                Side::Buy,
                OrderKind::Stop {
                    trigger: Price(600_100)
                }
            )
        );
        assert_eq!(
            k(OrderPick::SellStop, 599_900),
            (
                Side::Sell,
                OrderKind::Stop {
                    trigger: Price(599_900)
                }
            )
        );
        // A stop on the wrong side of the market is refused (as in the DOM).
        assert!(pick_command(OrderPick::BuyStop, Price(599_900), armed(), &c).is_err());
    }

    #[test]
    fn chart_orders_need_arming() {
        let c = ctx();
        assert_eq!(
            pick_command(OrderPick::BuyLimit, Price(599_900), Arming::default(), &c),
            Err(Refusal(ARM_TO_TRADE))
        );
    }

    #[test]
    fn menu_offers_the_sensible_pair_and_the_market_is_last_else_mid() {
        let m = Price(600_000);
        assert_eq!(
            menu_picks(Price(600_100), m),
            [OrderPick::SellLimit, OrderPick::BuyStop]
        );
        assert_eq!(
            menu_picks(Price(599_900), m),
            [OrderPick::BuyLimit, OrderPick::SellStop]
        );
        let mut c = ctx();
        assert_eq!(reference(&c), Some(Price(600_000)));
        c.last = None;
        assert_eq!(reference(&c), Some(Price(600_012)), "mid, floored");
        (c.best_bid, c.best_ask) = (None, None);
        assert_eq!(reference(&c), None);
    }

    #[test]
    fn tags_name_side_type_qty_and_price() {
        let fmt = PriceFormat::new(
            PriceScale::decimal(2),
            DisplayStyle::Decimal { digits: 2 },
            25,
        );
        let mark = |role, stop, style, qty, filled, side| OrderMark {
            id: OrderId(1),
            side,
            price: Price(599_950),
            style,
            qty: Qty(qty),
            filled: Qty(filled),
            stop,
            role,
        };
        let text = |m: OrderMark| {
            let mut s = String::new();
            write_order_tag(&mut s, &m, &fmt).unwrap();
            s
        };
        assert_eq!(
            text(mark(
                Role::Single,
                false,
                MarkStyle::Working,
                2,
                0,
                Side::Buy
            )),
            "BUY LMT 2 @ 5999.50"
        );
        assert_eq!(
            text(mark(
                Role::Single,
                true,
                MarkStyle::Pending,
                1,
                0,
                Side::Sell
            )),
            "SELL STP 1 @ 5999.50"
        );
        assert_eq!(
            text(mark(
                Role::Single,
                false,
                MarkStyle::Partial,
                3,
                1,
                Side::Buy
            )),
            "BUY LMT 1/3 @ 5999.50"
        );
        assert_eq!(
            text(mark(
                Role::TakeProfit,
                false,
                MarkStyle::Working,
                1,
                0,
                Side::Sell
            )),
            "T 1 @ 5999.50"
        );
        assert_eq!(
            text(mark(
                Role::StopLoss,
                true,
                MarkStyle::Held,
                1,
                0,
                Side::Sell
            )),
            "S 1 @ 5999.50 held"
        );
    }
}

// ---- the chart's per-frame trading state ------------------------------------------------

use lines_core::order_lines::{LineIn, MAX_TAGS, TagBox};
use lines_core::{ContractKey, FixedStr, UtcMillis};
use lines_trading::AccountId;

use crate::dom_orders::DragOrder;
use crate::dom_trade::{Ghost, GhostOutcome, ghost_outcome, order_mark};
use crate::trading::TradeCache;

/// The position's tag id (orders use their order ids).
pub const POSITION_TAG: u64 = u64::MAX;

/// One tag's text and look, by tag id.
#[derive(Clone, Copy)]
pub struct TagText {
    pub id: u64,
    /// `BUY LMT 2 @ 5999.50`, `T 1 @ …`, `LONG 2 @ 5999.625`.
    pub head: FixedStr<64>,
    /// The position's P&L (`+2.5 t  +$125.00`), empty for orders and with P&L off.
    pub tail: FixedStr<48>,
    pub mark: Option<OrderMark>,
}

/// What the chart draws of its account's trading this frame, copied out of the trading
/// cache (`try_lock`: a busy pump keeps the last frame's copy), into buffers allocated once:
/// paint never allocates here.
pub struct ChartTradeState {
    pub marks: Vec<OrderMark>,
    pub texts: Vec<TagText>,
    pub lines: Vec<LineIn>,
    pub tags: [TagBox; MAX_TAGS],
    pub n_tags: usize,
    /// Fills: time, price, side.
    pub fills: Vec<(UtcMillis, Price, Side)>,
    /// Refusals with a price: where and the short reason (`DENIED: closed`).
    pub flashes: Vec<(Price, FixedStr<48>)>,
    /// The open position: signed qty, average price.
    pub position: Option<(i64, f64)>,
    /// Drags waiting for the broker, drawn at their new price.
    pub ghosts: Vec<Ghost>,
    pub drag: Option<DragOrder>,
    /// A Ctrl(+Shift)+press in the plot that becomes an order if the pointer doesn't move:
    /// where, when, with Shift.
    pub pending_click: Option<((f64, f64), std::time::Instant, bool)>,
    /// The price under the pointer at the last right-click (the menu's order choices).
    pub menu_price: Option<Price>,
}

impl Default for ChartTradeState {
    fn default() -> Self {
        ChartTradeState {
            marks: Vec::with_capacity(MAX_TAGS),
            texts: Vec::with_capacity(MAX_TAGS),
            lines: Vec::with_capacity(MAX_TAGS),
            tags: [TagBox::default(); MAX_TAGS],
            n_tags: 0,
            fills: Vec::with_capacity(256),
            flashes: Vec::with_capacity(8),
            position: None,
            ghosts: Vec::with_capacity(8),
            drag: None,
            pending_click: None,
            menu_price: None,
        }
    }
}

impl ChartTradeState {
    /// Copy this frame's orders, position, fills and refusals for `account` / `contract`.
    /// Returns refused drags (rare; allocates only then).
    pub fn prepare(
        &mut self,
        cache: &TradeCache,
        account: Option<&AccountId>,
        contract: Option<ContractKey>,
    ) -> Vec<(Price, String)> {
        let mut refused = Vec::new();
        let (Some(account), Some(contract)) = (account, contract) else {
            self.marks.clear();
            self.fills.clear();
            self.flashes.clear();
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
                .take(MAX_TAGS - 1),
        );
        self.position = s
            .position(account, contract)
            .and_then(|p| p.avg_price.map(|a| (p.qty, a)));
        self.fills.clear();
        let all = s.fills(account, contract);
        for f in all {
            if self.fills.len() == self.fills.capacity() {
                self.fills.remove(0);
            }
            self.fills.push((f.at, f.price, f.side));
        }
        self.flashes.clear();
        for f in s.flashes(account, Some(contract), std::time::Instant::now()) {
            if let Some(p) = f.price
                && self.flashes.len() < self.flashes.capacity()
            {
                let mut t = FixedStr::<48>::new();
                let _ = write!(
                    t,
                    "{}: {}",
                    f.kind.label(),
                    crate::dom_trade::short_reason(&f.message)
                );
                self.flashes.push((p, t));
            }
        }
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
}
