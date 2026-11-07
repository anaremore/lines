//! DOM order entry, without GPUI (roadmap 7): what a click, drag or key on the ladder means,
//! how working orders look, and when the DOM is armed. The DOM view only measures where the
//! pointer is and draws what this module decides, so every rule here is unit-tested.
//!
//! **Click → command** ([`click`]): in an armed DOM, a click in the bid column is a buy and in
//! the ask column a sell, at the row's price; plain click = limit, Shift+click = stop market.
//! A stop must be on the trigger side of the market (a buy stop above the best ask, a sell stop
//! below the best bid): one that would trigger at once is refused with the reason.
//!
//! **Grouped rows**: a row covers `grouping` ticks, `[base, base + (grouping − 1) × tick]`, and
//! is labelled with its `base` (`lines_core::ladder`: `row = floor(price / (grouping × tick))`).
//! A click uses exactly that **displayed** price, the row's lowest, for both sides and both
//! order types: the trader gets the price they read, never one they can't see. Orders and
//! the position marker are shown on the row that contains their price.
//!
//! **Brackets** ([`BracketPreset`], ATM-style, in ticks): attached to a click's order only when
//! it opens or adds to the position (flat, or the same side). An order that reduces the
//! position is an exit and never gets exits of its own. The paper broker holds the exits
//! server-side and activates them as the entry fills (OCO).
//!
//! **Arming** ([`Arming`]): a DOM starts disarmed; clicks on a disarmed DOM never trade. Only an
//! explicit toggle (header or hotkey) arms it. It disarms on a symbol or contract change and
//! when trading becomes unavailable, always; on focus loss (including the window going
//! inactive) and on an account change, if configured (both on by default).

use lines_core::{ContractKey, Price, Qty};
use lines_trading::order::{Order, OrderState, Role};
use lines_trading::{
    AccountId, BracketSpec, Command as TradeCommand, CommandId, OrderId, OrderKind, OrderSpec,
    Reject, Side,
};
use serde::{Deserialize, Serialize};

/// The ladder's columns, as far as order entry is concerned.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DomColumn {
    /// The trader's own working orders (drag and right-click act here).
    Orders,
    Bid,
    Price,
    Ask,
    Volume,
}

// ---- arming ------------------------------------------------------------------------------

/// What disarms a DOM besides the trader (Settings → Trading, decision 0021).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmPrefs {
    pub disarm_on_focus_loss: bool,
    pub disarm_on_account_change: bool,
}

impl Default for ArmPrefs {
    fn default() -> Self {
        ArmPrefs {
            disarm_on_focus_loss: true,
            disarm_on_account_change: true,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmEvent {
    /// The header toggle or its hotkey.
    Toggle,
    /// The DOM lost keyboard focus, or its window went inactive.
    FocusLost,
    AccountChanged,
    /// Another symbol or contract (a roll, a picker switch, a linked pane).
    ContractChanged,
    /// No account, no contract, or the feed is unavailable.
    Unavailable,
}

/// One-click trading's safety catch. Never saved: every DOM starts disarmed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Arming {
    armed: bool,
}

impl Arming {
    pub fn is_armed(self) -> bool {
        self.armed
    }

    /// Apply an event; `can_trade`: an account and a contract are there (arming is refused
    /// otherwise). Returns whether the state changed.
    pub fn apply(&mut self, ev: ArmEvent, prefs: ArmPrefs, can_trade: bool) -> bool {
        let next = match ev {
            ArmEvent::Toggle => !self.armed && can_trade,
            ArmEvent::ContractChanged | ArmEvent::Unavailable => false,
            ArmEvent::FocusLost => self.armed && !prefs.disarm_on_focus_loss,
            ArmEvent::AccountChanged => self.armed && !prefs.disarm_on_account_change,
        };
        let changed = next != self.armed;
        self.armed = next;
        changed
    }
}

// ---- brackets ----------------------------------------------------------------------------

/// A stop and a target, in ticks from the entry price.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BracketPreset {
    pub stop_ticks: u32,
    pub target_ticks: u32,
}

/// The header's bracket choices, in cycling order (`None` = no bracket).
pub const BRACKET_PRESETS: [Option<BracketPreset>; 4] = [
    None,
    Some(BracketPreset {
        stop_ticks: 8,
        target_ticks: 16,
    }),
    Some(BracketPreset {
        stop_ticks: 12,
        target_ticks: 24,
    }),
    Some(BracketPreset {
        stop_ticks: 20,
        target_ticks: 40,
    }),
];

/// The next preset after `b` (a custom one rejoins the list at the start).
pub fn next_bracket(b: Option<BracketPreset>) -> Option<BracketPreset> {
    let i = BRACKET_PRESETS.iter().position(|p| *p == b);
    BRACKET_PRESETS[i.map_or(0, |i| (i + 1) % BRACKET_PRESETS.len())]
}

/// Whether an order on `side` opens or adds to `position` (signed): brackets attach only then.
pub fn opens_or_adds(side: Side, position: i64) -> bool {
    position == 0 || position.signum() == side.sign()
}

// ---- quantity ----------------------------------------------------------------------------

/// The header's quantity presets (custom values are typed).
pub const QTY_PRESETS: [i64; 5] = [1, 2, 3, 5, 10];

/// The next (or previous) preset from `q`; custom values rejoin the presets.
pub fn step_qty(q: i64, up: bool) -> i64 {
    if up {
        QTY_PRESETS
            .iter()
            .copied()
            .find(|&p| p > q)
            .unwrap_or(QTY_PRESETS[QTY_PRESETS.len() - 1].max(q))
    } else {
        QTY_PRESETS
            .iter()
            .rev()
            .copied()
            .find(|&p| p < q)
            .unwrap_or(QTY_PRESETS[0])
    }
}

/// A typed quantity: digits only, 1..=9999 (the risk layer has the real limit).
pub fn parse_qty(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|&q| q > 0)
}

// ---- click → command ---------------------------------------------------------------------

/// What the click needs to know (from the pane's account, feed and header).
#[derive(Clone, Debug)]
pub struct EntryCtx {
    pub account: AccountId,
    pub contract: ContractKey,
    pub qty: Qty,
    /// Signed current position in the contract.
    pub position: i64,
    /// Orders open on the contract (bracket exits included).
    pub working: usize,
    pub bracket: Option<BracketPreset>,
    /// Price units per tick.
    pub tick: i64,
    pub best_bid: Option<Price>,
    pub best_ask: Option<Price>,
    pub last: Option<Price>,
}

/// Reverse refused while orders work on the contract.
pub const REVERSE_WORKING: &str =
    "cancel working orders first: their fills would add to the reversed position";

/// Why a click did nothing (shown inline in the DOM, briefly).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal(pub &'static str);

/// The command for a click at `price` (the row's displayed price) in `col`.
pub fn click(
    col: DomColumn,
    price: Price,
    shift: bool,
    arming: Arming,
    ctx: &EntryCtx,
) -> Result<TradeCommand, Refusal> {
    let side = match col {
        DomColumn::Bid => Side::Buy,
        DomColumn::Ask => Side::Sell,
        _ => {
            return Err(Refusal(
                "click the bid column to buy or the ask column to sell",
            ));
        }
    };
    if !arming.is_armed() {
        return Err(Refusal("DOM disarmed: arm it to trade with one click"));
    }
    if ctx.qty.0 <= 0 {
        return Err(Refusal("set a quantity first"));
    }
    let kind = if shift {
        // A stop must sit on the far side of the market, or it triggers at once.
        let market = match side {
            Side::Buy => ctx.best_ask.or(ctx.last),
            Side::Sell => ctx.best_bid.or(ctx.last),
        };
        match (side, market) {
            (_, None) => return Err(Refusal("no market price yet to place a stop against")),
            (Side::Buy, Some(m)) if price.0 <= m.0 => {
                return Err(Refusal("a buy stop must be above the market"));
            }
            (Side::Sell, Some(m)) if price.0 >= m.0 => {
                return Err(Refusal("a sell stop must be below the market"));
            }
            _ => OrderKind::Stop { trigger: price },
        }
    } else {
        OrderKind::Limit { price }
    };
    let entry = OrderSpec {
        account: ctx.account.clone(),
        contract: ctx.contract,
        side,
        qty: ctx.qty,
        kind,
    };
    Ok(match ctx.bracket {
        Some(b) if opens_or_adds(side, ctx.position) => {
            let s = side.sign();
            let (stop, target) = (
                b.stop_ticks as i64 * ctx.tick,
                b.target_ticks as i64 * ctx.tick,
            );
            TradeCommand::PlaceBracket(BracketSpec {
                entry,
                take_profit: Some(Price(price.0 + s * target)),
                stop_loss: Some(Price(price.0 - s * stop)),
            })
        }
        _ => TradeCommand::PlaceOrder(entry),
    })
}

// ---- header order buttons ----------------------------------------------------------------

/// The header's order buttons (and their hotkeys): market buy/sell, join the best bid/ask,
/// reverse. Gated by arming exactly like ladder clicks; the risk layer and prop rules apply
/// in the core as for any order. Flatten and Cancel All are separate and never gated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QuickOrder {
    BuyMarket,
    SellMarket,
    /// A buy limit at the best bid.
    JoinBid,
    /// A sell limit at the best ask.
    JoinAsk,
    /// One market order for twice the position, on the other side: long 2 becomes short 2.
    Reverse,
}

/// What a disarmed DOM says when a header order button (or its key) is used.
pub const ARM_TO_TRADE: &str = "Arm to trade";

/// The command for a header order button. Brackets attach as for clicks (only to orders that
/// open or add): a market order's exits are placed from the price it is expected to fill at
/// (the best ask for a buy, the best bid for a sell, else the last trade). Reverse never
/// carries a bracket: it closes the position before it opens the other side.
pub fn quick(q: QuickOrder, arming: Arming, ctx: &EntryCtx) -> Result<TradeCommand, Refusal> {
    if !arming.is_armed() {
        return Err(Refusal(ARM_TO_TRADE));
    }
    let (side, qty, kind, bracket_at) = match q {
        QuickOrder::BuyMarket => (
            Side::Buy,
            ctx.qty,
            OrderKind::Market,
            ctx.best_ask.or(ctx.last),
        ),
        QuickOrder::SellMarket => (
            Side::Sell,
            ctx.qty,
            OrderKind::Market,
            ctx.best_bid.or(ctx.last),
        ),
        QuickOrder::JoinBid => {
            let p = ctx.best_bid.ok_or(Refusal("no bid to join"))?;
            (Side::Buy, ctx.qty, OrderKind::Limit { price: p }, Some(p))
        }
        QuickOrder::JoinAsk => {
            let p = ctx.best_ask.ok_or(Refusal("no ask to join"))?;
            (Side::Sell, ctx.qty, OrderKind::Limit { price: p }, Some(p))
        }
        QuickOrder::Reverse => {
            if ctx.position == 0 {
                return Err(Refusal("no position to reverse"));
            }
            // A working order (a bracket exit above all) could fill after the reversal and
            // add to the new position: the trader cancels first (Cancel all), then reverses.
            if ctx.working > 0 {
                return Err(Refusal(REVERSE_WORKING));
            }
            let side = if ctx.position > 0 {
                Side::Sell
            } else {
                Side::Buy
            };
            let entry = OrderSpec {
                account: ctx.account.clone(),
                contract: ctx.contract,
                side,
                qty: Qty(ctx.position.abs() * 2),
                kind: OrderKind::Market,
            };
            return Ok(TradeCommand::PlaceOrder(entry));
        }
    };
    if qty.0 <= 0 {
        return Err(Refusal("set a quantity first"));
    }
    let entry = OrderSpec {
        account: ctx.account.clone(),
        contract: ctx.contract,
        side,
        qty,
        kind,
    };
    Ok(match (ctx.bracket, bracket_at) {
        (Some(b), Some(at)) if opens_or_adds(side, ctx.position) => {
            let s = side.sign();
            let (stop, target) = (
                b.stop_ticks as i64 * ctx.tick,
                b.target_ticks as i64 * ctx.tick,
            );
            TradeCommand::PlaceBracket(BracketSpec {
                entry,
                take_profit: Some(Price(at.0 + s * target)),
                stop_loss: Some(Price(at.0 - s * stop)),
            })
        }
        (Some(_), None) if opens_or_adds(side, ctx.position) => {
            return Err(Refusal("no market price yet to place the bracket from"));
        }
        _ => TradeCommand::PlaceOrder(entry),
    })
}

// ---- working orders ----------------------------------------------------------------------

/// How an order is drawn in the orders column (docs/design.md §3: distinct by shape, never
/// borrowing each other's look).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MarkStyle {
    /// Sent, not acknowledged: an outline only.
    Pending,
    /// Acknowledged but not active: a bracket exit waiting for its entry to fill. A dashed
    /// outline.
    Held,
    /// Working: a solid block.
    Working,
    /// Partially filled: a block filled in proportion to the filled quantity, with the rest
    /// outlined.
    Partial,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderMark {
    pub id: OrderId,
    pub side: Side,
    /// The price the order rests at (limit price, stop trigger, or a triggered stop-limit's
    /// limit).
    pub price: Price,
    pub style: MarkStyle,
    pub qty: Qty,
    pub filled: Qty,
    pub stop: bool,
    pub role: Role,
}

/// What an order is for, as the orders column shows it (docs/design.md §3: by text, colour
/// and shape, never colour alone).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MarkRole {
    /// An entry or a plain order: the side's colour, a plain block.
    Entry,
    /// A bracket's take-profit: `T`, the target colour, a notch on its top edge.
    Target,
    /// A bracket's stop-loss: `S`, the stop colour, a notch on its bottom edge.
    Stop,
}

impl MarkRole {
    pub fn of(m: &OrderMark) -> MarkRole {
        match m.role {
            Role::TakeProfit => MarkRole::Target,
            Role::StopLoss => MarkRole::Stop,
            _ => MarkRole::Entry,
        }
    }
}

/// A mark's text: role or stop prefix (`T`, `S`), then what is left (`T1`, `S1`, `2`), or
/// `filled/qty` while partially filled. Writes into a fixed buffer (paint path, no
/// allocation).
pub fn mark_text(m: &OrderMark, out: &mut impl std::fmt::Write) -> std::fmt::Result {
    let prefix = match MarkRole::of(m) {
        MarkRole::Target => "T",
        MarkRole::Stop => "S",
        MarkRole::Entry if m.stop => "S",
        MarkRole::Entry => "",
    };
    out.write_str(prefix)?;
    if m.style == MarkStyle::Partial {
        write!(out, "{}/{}", m.filled.0, m.qty.0)
    } else {
        write!(out, "{}", m.qty.0 - m.filled.0)
    }
}

/// The price an order is shown (and dragged) at; `None` for market orders.
pub fn order_price(o: &Order) -> Option<Price> {
    match o.kind {
        OrderKind::Market => None,
        OrderKind::Limit { price } => Some(price),
        OrderKind::Stop { trigger } => Some(trigger),
        OrderKind::StopLimit { trigger, limit } => Some(if o.triggered { limit } else { trigger }),
    }
}

/// How an open order is drawn, if at all (terminal orders aren't; rejects flash separately).
pub fn order_mark(o: &Order) -> Option<OrderMark> {
    let style = match o.state {
        OrderState::Submitted => MarkStyle::Pending,
        OrderState::Acked => MarkStyle::Held,
        OrderState::Working => MarkStyle::Working,
        OrderState::PartiallyFilled => MarkStyle::Partial,
        // Terminal, or unresolved (sent with no known outcome: listed in the header, never
        // drawn as working).
        OrderState::Filled
        | OrderState::Cancelled
        | OrderState::Rejected { .. }
        | OrderState::Unresolved => return None,
    };
    // A bracket exit with nothing open to close (its sibling closed it while the entry still
    // works) is inactive at the broker: drawn held.
    let style = if o.remaining().0 == 0 {
        MarkStyle::Held
    } else {
        style
    };
    Some(OrderMark {
        id: o.id,
        side: o.side,
        price: order_price(o)?,
        style,
        qty: o.qty,
        filled: o.filled,
        stop: matches!(o.kind, OrderKind::Stop { .. } | OrderKind::StopLimit { .. }),
        role: o.role,
    })
}

/// The ladder row (in `units` = grouping × tick) that contains `price`.
pub fn row_of(price: i64, units: i64) -> i64 {
    price.div_euclid(units.max(1))
}

// ---- drag to modify ----------------------------------------------------------------------

/// The modify command for dragging order `o` to `to` (a row's displayed price); `None` when it
/// didn't move or can't be moved (a market order).
pub fn drag_command(o: &Order, to: Price) -> Option<TradeCommand> {
    let from = order_price(o)?;
    (o.is_open() && from != to).then(|| TradeCommand::ModifyOrder {
        account: o.account.clone(),
        order: o.id,
        qty: None,
        price: Some(to),
    })
}

/// A dragged order's new price, drawn as a ghost until the broker confirms it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ghost {
    pub order: OrderId,
    pub price: Price,
    pub command: CommandId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GhostOutcome {
    /// Still waiting: draw the ghost at the new price, the order where it was.
    Pending,
    /// The order now rests at the new price: drop the ghost.
    Acked,
    /// Refused (risk, broker) or the order finished meanwhile: drop the ghost, show why.
    Rejected(String),
}

/// What became of a drag: from the order as the core now reports it and the command's
/// result, if it arrived.
pub fn ghost_outcome(
    g: &Ghost,
    order: Option<&Order>,
    result: Option<&Result<Vec<OrderId>, Reject>>,
) -> GhostOutcome {
    if let Some(Err(r)) = result {
        return GhostOutcome::Rejected(r.message.clone());
    }
    match order {
        None => GhostOutcome::Rejected("the order is gone".into()),
        Some(o) if !o.is_open() => GhostOutcome::Rejected(format!("order {}", o.state.label())),
        Some(o) if order_price(o) == Some(g.price) => GhostOutcome::Acked,
        Some(_) => GhostOutcome::Pending,
    }
}

// ---- position ----------------------------------------------------------------------------

/// Open P&L in ticks per contract (positive = winning), from the average price.
pub fn open_ticks(qty: i64, avg_price: f64, mark: Price, tick: i64) -> Option<f64> {
    (qty != 0 && tick > 0).then(|| (mark.0 as f64 - avg_price) / tick as f64 * qty.signum() as f64)
}

/// The row of the position marker (the row containing the average price, rounded to the
/// nearest tick first).
pub fn position_row(avg_price: f64, tick: i64, units: i64) -> i64 {
    let t = tick.max(1) as f64;
    let on_tick = ((avg_price / t).round() * t) as i64;
    row_of(on_tick, units)
}

// ---- rejections on a row ---------------------------------------------------------------

/// A refusal's reason in a word or two, for the tag on its row in the orders column (the
/// full message stays in the header). Known reasons by their wording, else the message's
/// first word, at most 9 characters.
pub fn short_reason(message: &str) -> &str {
    const KNOWN: [(&str, &str); 16] = [
        ("market is closed", "closed"),
        ("tick grid", "tick"),
        ("trading halted", "halted"),
        ("reducing only", "reduce"),
        ("daily loss", "day loss"),
        ("drawdown", "drawdown"),
        ("micro-equivalent", "max size"),
        ("position could reach", "max pos"),
        ("over the maximum", "max qty"),
        ("ticks from the market", "far"),
        ("order value", "notional"),
        ("in a second", "rate"),
        ("no market price", "no price"),
        ("liquidity", "no liq"),
        ("disarmed", "disarmed"),
        ("filled", "filled"),
    ];
    let lower = message.to_ascii_lowercase();
    for (needle, short) in KNOWN {
        if lower.contains(needle) {
            return short;
        }
    }
    let word = message.split_whitespace().next().unwrap_or("");
    let end = word.char_indices().nth(9).map_or(word.len(), |(i, _)| i);
    &word[..end]
}

// ---- rule strip --------------------------------------------------------------------------

/// How close an account is to a prop rule's limit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RuleLevel {
    Ok,
    /// Within [`NEAR_LIMIT`] of the limit: the strip turns warning-coloured.
    Near,
    Breached,
}

/// "Near" = less than this share of the limit left.
pub const NEAR_LIMIT: f64 = 0.25;

/// `left`: how much may still be lost; `limit`: the whole allowance (both in money units).
pub fn rule_level(left: i128, limit: i128) -> RuleLevel {
    if left <= 0 {
        RuleLevel::Breached
    } else if limit > 0 && (left as f64) < limit as f64 * NEAR_LIMIT {
        RuleLevel::Near
    } else {
        RuleLevel::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::UtcMillis;
    use lines_trading::{RejectCode, types::ContractDoc};

    const ES: i64 = 25; // ES tick: 25 price units ($0.25 at 1/100)

    fn esz6() -> ContractKey {
        ContractDoc {
            product: 1,
            year: Some(2026),
            month: Some(12),
        }
        .to_key()
        .unwrap()
    }

    fn ctx() -> EntryCtx {
        EntryCtx {
            account: AccountId::new("paper-1"),
            contract: esz6(),
            qty: Qty(2),
            position: 0,
            working: 0,
            bracket: None,
            tick: ES,
            best_bid: Some(Price(600_000)),
            best_ask: Some(Price(600_025)),
            last: Some(Price(600_000)),
        }
    }

    fn armed() -> Arming {
        let mut a = Arming::default();
        assert!(a.apply(ArmEvent::Toggle, ArmPrefs::default(), true));
        a
    }

    fn spec(c: &TradeCommand) -> &OrderSpec {
        match c {
            TradeCommand::PlaceOrder(o) => o,
            TradeCommand::PlaceBracket(b) => &b.entry,
            other => panic!("not an order: {other:?}"),
        }
    }

    #[test]
    fn columns_map_to_side_and_type_at_the_rows_price() {
        let c = ctx();
        let buy = click(DomColumn::Bid, Price(599_950), false, armed(), &c).unwrap();
        let s = spec(&buy);
        assert_eq!(
            (s.side, s.kind, s.qty, s.contract),
            (
                Side::Buy,
                OrderKind::Limit {
                    price: Price(599_950)
                },
                Qty(2),
                esz6()
            )
        );
        assert_eq!(s.account, AccountId::new("paper-1"));
        let sell = click(DomColumn::Ask, Price(600_100), false, armed(), &c).unwrap();
        assert_eq!(
            (spec(&sell).side, spec(&sell).kind),
            (
                Side::Sell,
                OrderKind::Limit {
                    price: Price(600_100)
                }
            )
        );
        // Shift: stop market at that price, on the far side of the market.
        let bstop = click(DomColumn::Bid, Price(600_100), true, armed(), &c).unwrap();
        assert_eq!(
            (spec(&bstop).side, spec(&bstop).kind),
            (
                Side::Buy,
                OrderKind::Stop {
                    trigger: Price(600_100)
                }
            )
        );
        let sstop = click(DomColumn::Ask, Price(599_900), true, armed(), &c).unwrap();
        assert_eq!(
            spec(&sstop).kind,
            OrderKind::Stop {
                trigger: Price(599_900)
            }
        );
        assert_eq!(spec(&sstop).side, Side::Sell);
        // Price, volume and orders columns don't place orders.
        for col in [DomColumn::Price, DomColumn::Volume, DomColumn::Orders] {
            assert!(click(col, Price(600_000), false, armed(), &c).is_err());
        }
    }

    #[test]
    fn stops_that_would_trigger_at_once_are_refused() {
        let c = ctx();
        // Buy stop at or below the best ask, sell stop at or above the best bid.
        assert_eq!(
            click(DomColumn::Bid, Price(600_025), true, armed(), &c),
            Err(Refusal("a buy stop must be above the market"))
        );
        assert_eq!(
            click(DomColumn::Ask, Price(600_000), true, armed(), &c),
            Err(Refusal("a sell stop must be below the market"))
        );
        let mut empty = c.clone();
        (empty.best_bid, empty.best_ask, empty.last) = (None, None, None);
        assert!(click(DomColumn::Bid, Price(600_100), true, armed(), &empty).is_err());
        // Limits through the market are allowed (marketable; the risk band still applies).
        assert!(click(DomColumn::Bid, Price(600_100), false, armed(), &c).is_ok());
    }

    #[test]
    fn grouped_rows_use_the_displayed_price() {
        // Grouping 4 ticks = 100 units: the row labelled 6000.00 covers 6000.00..=6000.75.
        let units = 4 * ES;
        let base = row_of(600_075, units) * units;
        assert_eq!(
            base, 600_000,
            "a price inside the row maps to the row's base"
        );
        assert_eq!(row_of(600_000, units), row_of(600_075, units));
        assert_ne!(row_of(599_975, units), row_of(600_000, units));
        // Negative prices (spreads) floor too.
        assert_eq!(row_of(-25, units), -1);
        // A click on that row buys or sells exactly at the base, never at a hidden price.
        let c = ctx();
        let buy = click(DomColumn::Bid, Price(base), false, armed(), &c).unwrap();
        assert_eq!(
            spec(&buy).kind,
            OrderKind::Limit {
                price: Price(600_000)
            }
        );
        let sell = click(DomColumn::Ask, Price(base), false, armed(), &c).unwrap();
        assert_eq!(
            spec(&sell).kind,
            OrderKind::Limit {
                price: Price(600_000)
            }
        );
    }

    #[test]
    fn disarmed_dom_never_trades() {
        let c = ctx();
        assert_eq!(
            click(DomColumn::Bid, Price(599_950), false, Arming::default(), &c),
            Err(Refusal("DOM disarmed: arm it to trade with one click"))
        );
        let mut zero = c;
        zero.qty = Qty(0);
        assert!(click(DomColumn::Bid, Price(599_950), false, armed(), &zero).is_err());
    }

    #[test]
    fn header_order_buttons() {
        let c = ctx(); // bid 6000.00, ask 6000.25, qty 2, flat
        let k = |q| quick(q, armed(), &c).unwrap();
        // Market buy/sell at the selected quantity.
        assert_eq!(
            (
                spec(&k(QuickOrder::BuyMarket)).side,
                spec(&k(QuickOrder::BuyMarket)).kind
            ),
            (Side::Buy, OrderKind::Market)
        );
        assert_eq!(spec(&k(QuickOrder::SellMarket)).side, Side::Sell);
        assert_eq!(spec(&k(QuickOrder::SellMarket)).qty, Qty(2));
        // Join: a limit at the best bid (buy) / best ask (sell).
        assert_eq!(
            spec(&k(QuickOrder::JoinBid)).kind,
            OrderKind::Limit {
                price: Price(600_000)
            }
        );
        assert_eq!(spec(&k(QuickOrder::JoinBid)).side, Side::Buy);
        assert_eq!(
            spec(&k(QuickOrder::JoinAsk)).kind,
            OrderKind::Limit {
                price: Price(600_025)
            }
        );
        let mut no_book = c.clone();
        (no_book.best_bid, no_book.best_ask) = (None, None);
        assert_eq!(
            quick(QuickOrder::JoinBid, armed(), &no_book),
            Err(Refusal("no bid to join"))
        );
        // Disarmed: nothing is built, and the DOM says why.
        for q in [
            QuickOrder::BuyMarket,
            QuickOrder::SellMarket,
            QuickOrder::JoinBid,
            QuickOrder::JoinAsk,
            QuickOrder::Reverse,
        ] {
            assert_eq!(
                quick(q, Arming::default(), &c),
                Err(Refusal(ARM_TO_TRADE)),
                "{q:?}"
            );
        }
    }

    #[test]
    fn reverse_flips_with_one_market_order_for_twice_the_position() {
        let mut c = ctx();
        assert_eq!(
            quick(QuickOrder::Reverse, armed(), &c),
            Err(Refusal("no position to reverse"))
        );
        c.position = 2;
        c.bracket = Some(BracketPreset {
            stop_ticks: 8,
            target_ticks: 16,
        });
        let r = quick(QuickOrder::Reverse, armed(), &c).unwrap();
        assert!(
            matches!(r, TradeCommand::PlaceOrder(_)),
            "no bracket on a reverse"
        );
        let s = spec(&r);
        assert_eq!(
            (s.side, s.qty, s.kind),
            (Side::Sell, Qty(4), OrderKind::Market)
        );
        c.position = -3;
        let s2 = quick(QuickOrder::Reverse, armed(), &c).unwrap();
        assert_eq!((spec(&s2).side, spec(&s2).qty), (Side::Buy, Qty(6)));
    }

    /// Review round 1: a bracketed position's exits keep working after a reversal (they
    /// aren't the reversal's exits), and a later exit fill would double the new position.
    /// Reverse is refused while any order works on the contract.
    #[test]
    fn reverse_is_refused_while_orders_work() {
        let mut c = ctx();
        c.position = 2;
        c.working = 2; // the bracket's target and stop
        assert_eq!(
            quick(QuickOrder::Reverse, armed(), &c),
            Err(Refusal(REVERSE_WORKING))
        );
        c.working = 0;
        assert!(quick(QuickOrder::Reverse, armed(), &c).is_ok());
    }

    #[test]
    fn header_buttons_attach_brackets_like_clicks() {
        let mut c = ctx();
        c.bracket = Some(BracketPreset {
            stop_ticks: 8,
            target_ticks: 16,
        });
        // Flat: a market buy is bracketed from the best ask it should fill at.
        let TradeCommand::PlaceBracket(b) = quick(QuickOrder::BuyMarket, armed(), &c).unwrap()
        else {
            panic!("expected a bracket")
        };
        assert_eq!(b.take_profit, Some(Price(600_025 + 16 * ES)));
        assert_eq!(b.stop_loss, Some(Price(600_025 - 8 * ES)));
        // A market sell from the best bid.
        let TradeCommand::PlaceBracket(b) = quick(QuickOrder::SellMarket, armed(), &c).unwrap()
        else {
            panic!("expected a bracket")
        };
        assert_eq!(b.take_profit, Some(Price(600_000 - 16 * ES)));
        // Join bid: from the bid.
        let TradeCommand::PlaceBracket(b) = quick(QuickOrder::JoinBid, armed(), &c).unwrap() else {
            panic!("expected a bracket")
        };
        assert_eq!(b.stop_loss, Some(Price(600_000 - 8 * ES)));
        // Long: a sell is an exit, never bracketed.
        c.position = 2;
        assert!(matches!(
            quick(QuickOrder::SellMarket, armed(), &c).unwrap(),
            TradeCommand::PlaceOrder(_)
        ));
        // No price at all: a bracketed market order is refused rather than sent unprotected.
        let mut blind = ctx();
        blind.bracket = c.bracket;
        (blind.best_bid, blind.best_ask, blind.last) = (None, None, None);
        assert!(quick(QuickOrder::BuyMarket, armed(), &blind).is_err());
    }

    #[test]
    fn arming_state_machine() {
        let p = ArmPrefs::default();
        let mut a = Arming::default();
        assert!(!a.is_armed(), "starts disarmed");
        // Can't arm without an account and a contract.
        assert!(!a.apply(ArmEvent::Toggle, p, false));
        assert!(!a.is_armed());
        assert!(a.apply(ArmEvent::Toggle, p, true) && a.is_armed());
        assert!(
            a.apply(ArmEvent::Toggle, p, true) && !a.is_armed(),
            "toggle disarms"
        );
        for ev in [
            ArmEvent::FocusLost,
            ArmEvent::AccountChanged,
            ArmEvent::ContractChanged,
            ArmEvent::Unavailable,
        ] {
            let mut a = armed();
            assert!(a.apply(ev, p, true), "{ev:?} disarms by default");
            assert!(!a.is_armed());
            // Disarmed stays disarmed.
            assert!(!a.apply(ev, p, true));
        }
        // Configured off: focus loss and account change keep it armed; a contract change
        // and unavailability always disarm.
        let keep = ArmPrefs {
            disarm_on_focus_loss: false,
            disarm_on_account_change: false,
        };
        for ev in [ArmEvent::FocusLost, ArmEvent::AccountChanged] {
            let mut a = armed();
            assert!(!a.apply(ev, keep, true) && a.is_armed(), "{ev:?}");
        }
        for ev in [ArmEvent::ContractChanged, ArmEvent::Unavailable] {
            let mut a = armed();
            assert!(a.apply(ev, keep, true) && !a.is_armed(), "{ev:?}");
        }
    }

    #[test]
    fn brackets_attach_to_entries_only() {
        let mut c = ctx();
        c.bracket = Some(BracketPreset {
            stop_ticks: 8,
            target_ticks: 16,
        });
        // Flat: a buy limit at 5999.50 gets a target 16 ticks above and a stop 8 below.
        let cmd = click(DomColumn::Bid, Price(599_950), false, armed(), &c).unwrap();
        let TradeCommand::PlaceBracket(b) = cmd else {
            panic!("expected a bracket")
        };
        assert_eq!(b.take_profit, Some(Price(599_950 + 16 * ES)));
        assert_eq!(b.stop_loss, Some(Price(599_950 - 8 * ES)));
        // A sell entry mirrors it.
        let cmd = click(DomColumn::Ask, Price(600_100), false, armed(), &c).unwrap();
        let TradeCommand::PlaceBracket(b) = cmd else {
            panic!("expected a bracket")
        };
        assert_eq!(b.take_profit, Some(Price(600_100 - 16 * ES)));
        assert_eq!(b.stop_loss, Some(Price(600_100 + 8 * ES)));
        // Long 2: another buy adds (bracketed); a sell is an exit (plain order).
        c.position = 2;
        assert!(matches!(
            click(DomColumn::Bid, Price(599_950), false, armed(), &c).unwrap(),
            TradeCommand::PlaceBracket(_)
        ));
        assert!(matches!(
            click(DomColumn::Ask, Price(600_100), false, armed(), &c).unwrap(),
            TradeCommand::PlaceOrder(_)
        ));
        assert!(opens_or_adds(Side::Sell, -1) && !opens_or_adds(Side::Buy, -1));
        // Presets cycle and come back to "no bracket".
        let mut b = None;
        for _ in 0..BRACKET_PRESETS.len() {
            b = next_bracket(b);
        }
        assert_eq!(b, None);
        assert_eq!(next_bracket(None), BRACKET_PRESETS[1]);
    }

    #[test]
    fn quantity_presets_and_typing() {
        assert_eq!(step_qty(1, true), 2);
        assert_eq!(step_qty(4, true), 5, "custom rejoins the presets");
        assert_eq!(step_qty(10, true), 10);
        assert_eq!(step_qty(1, false), 1);
        assert_eq!(step_qty(7, false), 5);
        assert_eq!(parse_qty("12"), Some(12));
        for bad in ["", "0", "-1", "1.5", "abc", "12345"] {
            assert_eq!(parse_qty(bad), None, "{bad:?}");
        }
    }

    fn order(state: OrderState, kind: OrderKind) -> Order {
        Order {
            id: OrderId(7),
            account: AccountId::new("paper-1"),
            contract: esz6(),
            side: Side::Buy,
            qty: Qty(3),
            kind,
            state,
            filled: Qty(0),
            fill_value: 0,
            role: Role::Single,
            parent: None,
            oco: None,
            triggered: false,
            created: UtcMillis(0),
            updated: UtcMillis(0),
            reduce_only: false,
            overfill: Qty(0),
        }
    }

    #[test]
    fn order_states_render_distinctly() {
        let lim = OrderKind::Limit {
            price: Price(599_950),
        };
        let style = |s| order_mark(&order(s, lim)).map(|m| m.style);
        assert_eq!(style(OrderState::Submitted), Some(MarkStyle::Pending));
        assert_eq!(style(OrderState::Acked), Some(MarkStyle::Held));
        assert_eq!(style(OrderState::Working), Some(MarkStyle::Working));
        assert_eq!(style(OrderState::PartiallyFilled), Some(MarkStyle::Partial));
        for s in [
            OrderState::Filled,
            OrderState::Cancelled,
            OrderState::Rejected { reason: "x".into() },
            OrderState::Unresolved,
        ] {
            assert_eq!(
                style(s),
                None,
                "terminal and unresolved orders aren't drawn"
            );
        }
        // Every drawn state looks different.
        let all = [
            MarkStyle::Pending,
            MarkStyle::Held,
            MarkStyle::Working,
            MarkStyle::Partial,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Where it's drawn: limit price, stop trigger, a triggered stop-limit's limit.
        let stop = order(
            OrderState::Working,
            OrderKind::Stop {
                trigger: Price(600_200),
            },
        );
        let m = order_mark(&stop).unwrap();
        assert_eq!((m.price, m.stop), (Price(600_200), true));
        let mut sl = order(
            OrderState::Working,
            OrderKind::StopLimit {
                trigger: Price(600_200),
                limit: Price(600_225),
            },
        );
        assert_eq!(order_price(&sl), Some(Price(600_200)));
        sl.triggered = true;
        assert_eq!(order_price(&sl), Some(Price(600_225)));
        assert_eq!(
            order_mark(&order(OrderState::Working, OrderKind::Market)),
            None
        );
    }

    #[test]
    fn drag_modify_flow() {
        let lim = OrderKind::Limit {
            price: Price(599_950),
        };
        let o = order(OrderState::Working, lim);
        // Same row: nothing to send.
        assert_eq!(drag_command(&o, Price(599_950)), None);
        let cmd = drag_command(&o, Price(599_900)).unwrap();
        assert_eq!(
            cmd,
            TradeCommand::ModifyOrder {
                account: AccountId::new("paper-1"),
                order: OrderId(7),
                qty: None,
                price: Some(Price(599_900)),
            }
        );
        let g = Ghost {
            order: OrderId(7),
            price: Price(599_900),
            command: CommandId(1),
        };
        // Before the ack: pending (the order still reports its old price).
        assert_eq!(ghost_outcome(&g, Some(&o), None), GhostOutcome::Pending);
        assert_eq!(
            ghost_outcome(&g, Some(&o), Some(&Ok(vec![OrderId(7)]))),
            GhostOutcome::Pending
        );
        // The broker moved it: acked.
        let moved = order(
            OrderState::Working,
            OrderKind::Limit {
                price: Price(599_900),
            },
        );
        assert_eq!(ghost_outcome(&g, Some(&moved), None), GhostOutcome::Acked);
        // Refused: the reason.
        let rej = Err(Reject::new(
            RejectCode::Risk,
            "price is more than 20 ticks away",
        ));
        assert_eq!(
            ghost_outcome(&g, Some(&o), Some(&rej)),
            GhostOutcome::Rejected("price is more than 20 ticks away".into())
        );
        // Filled while dragging.
        let filled = order(OrderState::Filled, lim);
        assert_eq!(
            ghost_outcome(&g, Some(&filled), None),
            GhostOutcome::Rejected("order filled".into())
        );
        // Terminal orders can't be dragged.
        assert_eq!(drag_command(&filled, Price(599_900)), None);
    }

    #[test]
    fn position_marker_and_open_ticks() {
        // Long 2 @ 5999.625 (avg of 5999.50 and 5999.75): marker on the 5999.75 row at
        // grouping 1 (rounded to the nearest tick, ties away), open P&L at 6000.25 = 2.5 ticks.
        let avg = 599_962.5;
        assert_eq!(position_row(avg, ES, ES), row_of(599_975, ES));
        assert_eq!(open_ticks(2, avg, Price(600_025), ES), Some(2.5));
        assert_eq!(open_ticks(-1, 600_000.0, Price(600_025), ES), Some(-1.0));
        assert_eq!(open_ticks(0, 600_000.0, Price(600_025), ES), None);
        // Grouped: the marker sits on the row containing the average.
        assert_eq!(position_row(avg, ES, 4 * ES), row_of(599_975, 4 * ES));
    }

    #[test]
    fn bracket_exits_are_labelled_and_told_apart() {
        let mk = |role, kind| {
            let mut o = order(OrderState::Working, kind);
            o.role = role;
            o.qty = Qty(1);
            order_mark(&o).unwrap()
        };
        let lim = OrderKind::Limit {
            price: Price(600_400),
        };
        let stp = OrderKind::Stop {
            trigger: Price(599_800),
        };
        let text = |m: &OrderMark| {
            let mut s = String::new();
            mark_text(m, &mut s).unwrap();
            s
        };
        let target = mk(Role::TakeProfit, lim);
        let stop = mk(Role::StopLoss, stp);
        let entry = mk(Role::Entry, lim);
        // The owner's case: the target read "1" and the stop "S1", in one colour.
        assert_eq!(text(&target), "T1");
        assert_eq!(text(&stop), "S1");
        assert_eq!(text(&entry), "1");
        assert_eq!(
            text(&mk(Role::Single, stp)),
            "S1",
            "a stop entry is still a stop"
        );
        // Three roles, each with its own look (colour token and edge notch follow the role).
        let roles = [
            MarkRole::of(&target),
            MarkRole::of(&stop),
            MarkRole::of(&entry),
        ];
        assert_eq!(roles, [MarkRole::Target, MarkRole::Stop, MarkRole::Entry]);
        // Partial: filled/qty, with the prefix.
        let mut p = target;
        p.style = MarkStyle::Partial;
        p.qty = Qty(3);
        p.filled = Qty(1);
        assert_eq!(text(&p), "T1/3");
    }

    #[test]
    fn row_tags_are_short() {
        // The owner's case: a bracket on a closed market.
        assert_eq!(
            short_reason("bracket entry rejected: ES market is closed"),
            "closed"
        );
        assert_eq!(
            short_reason("price 6000.10 is not on the ES tick grid"),
            "tick"
        );
        assert_eq!(
            short_reason("price 6100.00 is more than 40 ticks from the market"),
            "far"
        );
        assert_eq!(
            short_reason("reducing only: daily loss limit reached; ..."),
            "reduce"
        );
        // Unknown: the first word, at most 9 characters.
        assert_eq!(short_reason("insufficient margin"), "insuffici");
        assert_eq!(short_reason(""), "");
        for m in [
            "bracket entry rejected: ES market is closed",
            "insufficient margin for this order at this time",
        ] {
            assert!(short_reason(m).chars().count() <= 9, "{m}");
        }
    }

    #[test]
    fn rule_levels() {
        assert_eq!(rule_level(1000, 1000), RuleLevel::Ok);
        assert_eq!(rule_level(250, 1000), RuleLevel::Ok);
        assert_eq!(rule_level(249, 1000), RuleLevel::Near);
        assert_eq!(rule_level(0, 1000), RuleLevel::Breached);
        assert_eq!(rule_level(-5, 1000), RuleLevel::Breached);
    }
}
