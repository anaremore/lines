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
//! **Trading switch** ([`Arming`], shown as "Trading: Enabled / Disabled"): clicks on a DOM
//! with trading disabled never trade; only the trader enables it (header switch or hotkey).
//! It is sticky and saved with the pane: focus, account and contract changes leave it as it
//! is unless the trader opted in ([`ArmPrefs`]), and impossible trading (no feed, market
//! closed, a lockout) denies orders without flipping it.

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

/// What turns DOM trading off besides the trader (Settings → Trading, decision 0021). All off
/// by default: the switch is sticky (owner decision, trade panel v2); these are opt-in.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmPrefs {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disarm_on_focus_loss: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disarm_on_account_change: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disarm_on_contract_change: bool,
}

impl ArmPrefs {
    pub fn is_default(&self) -> bool {
        *self == ArmPrefs::default()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmEvent {
    /// The header switch or its hotkey.
    Toggle,
    /// The DOM lost keyboard focus, or its window went inactive.
    FocusLost,
    AccountChanged,
    /// Another symbol or contract (a roll, a picker switch, a linked pane).
    ContractChanged,
}

/// One-click trading's switch ("Trading: Enabled / Disabled"). Sticky: it changes only when
/// the trader flips it (or an opt-in [`ArmPrefs`] event), never because trading is impossible
/// right now (no feed, market closed, a lockout, a risk denial): those deny the order with
/// their reason and leave the switch as it is. Saved per pane; new panes start disabled.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Arming {
    armed: bool,
}

impl Arming {
    /// Enabled (restored from a saved pane) or disabled.
    pub fn new(enabled: bool) -> Arming {
        Arming { armed: enabled }
    }

    pub fn is_armed(self) -> bool {
        self.armed
    }

    /// Apply an event. Returns whether the state changed.
    pub fn apply(&mut self, ev: ArmEvent, prefs: ArmPrefs) -> bool {
        let next = match ev {
            ArmEvent::Toggle => !self.armed,
            ArmEvent::FocusLost => self.armed && !prefs.disarm_on_focus_loss,
            ArmEvent::AccountChanged => self.armed && !prefs.disarm_on_account_change,
            ArmEvent::ContractChanged => self.armed && !prefs.disarm_on_contract_change,
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

/// The order size for a symbol: its shared size, else Settings' default, else 1.
pub fn order_size(
    sizes: &std::collections::BTreeMap<String, i64>,
    symbol: &str,
    default: Option<u32>,
) -> i64 {
    sizes
        .get(symbol)
        .copied()
        .filter(|&q| q > 0)
        .or(default.map(i64::from).filter(|&q| q > 0))
        .unwrap_or(1)
}

/// What the UI says when a size is over the account's order maximum: before the click, and
/// as the click's refusal.
pub fn over_max(qty: i64, max: Option<i64>) -> Option<String> {
    max.filter(|&m| qty > m).map(|m| format!("max {m}"))
}

/// [`over_max`] for a built order (the core would deny it; this says so in the same words as
/// the size control). Anything else passes.
pub fn command_over_max(cmd: &TradeCommand, max: Option<i64>) -> Option<String> {
    let qty = match cmd {
        TradeCommand::PlaceOrder(o) => o.qty,
        TradeCommand::PlaceBracket(b) => b.entry.qty,
        _ => return None,
    };
    over_max(qty.0, max)
}

/// One step of the size per this much wheel travel (logical px), so a trackpad's many small
/// events don't race through the presets.
pub const QTY_SCROLL_STEP: f32 = 24.0;

/// Add wheel travel `dy` to `acc`: `Some(up)` when it makes a step (up: wheel away from the
/// trader, positive `dy`). A reversal starts over.
pub fn scroll_qty(acc: &mut f32, dy: f32) -> Option<bool> {
    if dy == 0.0 {
        return None;
    }
    if (*acc > 0.0) != (dy > 0.0) {
        *acc = 0.0;
    }
    *acc += dy;
    if acc.abs() < QTY_SCROLL_STEP {
        return None;
    }
    let up = *acc > 0.0;
    *acc = 0.0;
    Some(up)
}

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
        return Err(Refusal(ENABLE_TO_TRADE));
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
    /// A buy limit at the best ask (takes the offer, never paying more than it).
    BuyAsk,
    /// A sell limit at the best bid (hits the bid, never selling for less).
    SellBid,
}

/// What an order key says while the DOM's order hotkeys are off.
pub const HOTKEYS_OFF: &str = "Order keys off: enable hotkeys (Ctrl+Shift+Y)";

/// What an order key says while a short pane has no room for the Keys chip.
pub const KEYS_HIDDEN: &str = "Order keys off: pane too short to show Keys";

/// The command for an order key (`commands::TradeCmd::needs_hotkeys`). Order keys need the
/// DOM's own hotkey switch ("Keys on", off by default, sticky like the trading switch) on top
/// of everything a button needs ([`quick`]): with it off nothing is built or sent. Its chip must
/// be in view too (`chip_shown`): keys that can send an order are never out of sight. `ctx` is
/// read only once these allow it.
pub fn key_order(
    q: QuickOrder,
    hotkeys: bool,
    chip_shown: bool,
    arming: Arming,
    ctx: impl FnOnce() -> Result<EntryCtx, Refusal>,
) -> Result<TradeCommand, Refusal> {
    if !hotkeys {
        return Err(Refusal(HOTKEYS_OFF));
    }
    if !chip_shown {
        return Err(Refusal(KEYS_HIDDEN));
    }
    quick(q, arming, &ctx()?)
}

/// What a DOM with trading disabled says when an entry is tried (click, button or key).
pub const ENABLE_TO_TRADE: &str = "Entries locked: enable trading (Ctrl+Shift+Space)";

/// The command for a header order button. Brackets attach as for clicks (only to orders that
/// open or add): a market order's exits are placed from the price it is expected to fill at
/// (the best ask for a buy, the best bid for a sell, else the last trade). Reverse never
/// carries a bracket: it closes the position before it opens the other side.
pub fn quick(q: QuickOrder, arming: Arming, ctx: &EntryCtx) -> Result<TradeCommand, Refusal> {
    if !arming.is_armed() {
        return Err(Refusal(ENABLE_TO_TRADE));
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
        QuickOrder::BuyAsk => {
            let p = ctx.best_ask.ok_or(Refusal("no ask to buy at"))?;
            (Side::Buy, ctx.qty, OrderKind::Limit { price: p }, Some(p))
        }
        QuickOrder::SellBid => {
            let p = ctx.best_bid.ok_or(Refusal("no bid to sell at"))?;
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
    /// A reduce-only order (an exit added for the position, or placed while the account may
    /// only reduce): shown by its kind as a stop or target.
    pub exit: bool,
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
            // A reduce-only exit (added on the chart): a stop or a target by its kind.
            Role::Single if m.exit && m.stop => MarkRole::Stop,
            Role::Single if m.exit => MarkRole::Target,
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

/// Which orders may share a tag or a DOM mark: same role, side and order type, and the same
/// state class (working with partly filled; held and pending never mix with working).
pub fn group_key(m: &OrderMark) -> u16 {
    let role = match MarkRole::of(m) {
        MarkRole::Target => 0,
        MarkRole::Stop => 1,
        MarkRole::Entry => 2,
    };
    let class = match m.style {
        MarkStyle::Working | MarkStyle::Partial => 0,
        MarkStyle::Held => 1,
        MarkStyle::Pending => 2,
    };
    role | u16::from(m.side == Side::Sell) << 2 | u16::from(m.stop) << 3 | class << 4
}

/// A DOM mark for several orders of one kind on a row: `S 200 ×20` (what is left, how many), or
/// with `short` just `S ×20` for a cell too narrow to say more (never a cut number).
pub fn mark_group_text(
    m: &OrderMark,
    left: i64,
    n: usize,
    short: bool,
    out: &mut impl std::fmt::Write,
) -> std::fmt::Result {
    let prefix = match MarkRole::of(m) {
        MarkRole::Target => "T ",
        MarkRole::Stop => "S ",
        MarkRole::Entry if m.stop => "S ",
        MarkRole::Entry => "",
    };
    out.write_str(prefix)?;
    if short {
        write!(out, "×{n}")
    } else {
        write!(out, "{left} ×{n}")
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
        exit: o.reduce_only,
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

/// Why moving stop `o` to `to` is refused: a stop must stay on the far side of the market
/// (a buy stop above the ask, a sell stop below the bid; the last trade without a book), or it
/// triggers at once, as for a shift-click. `None` for other orders and allowed moves.
pub fn stop_move_refusal(
    o: &Order,
    to: Price,
    bid: Option<Price>,
    ask: Option<Price>,
    last: Option<Price>,
) -> Option<&'static str> {
    let stop = match o.kind {
        OrderKind::Stop { .. } => true,
        OrderKind::StopLimit { .. } => !o.triggered,
        _ => false,
    };
    // A bracket exit held for its entry isn't in the market yet: it can't trigger, and it
    // moves anywhere (a breakout's stop above the market until the entry fills).
    let held = matches!(o.role, Role::TakeProfit | Role::StopLoss)
        && matches!(o.state, OrderState::Submitted | OrderState::Acked);
    if !stop || held {
        return None;
    }
    let market = match o.side {
        Side::Buy => ask.or(last),
        Side::Sell => bid.or(last),
    };
    match (o.side, market) {
        (_, None) => Some("no market price yet to move the stop against"),
        (Side::Buy, Some(m)) if to.0 <= m.0 => Some("a buy stop must stay above the market"),
        (Side::Sell, Some(m)) if to.0 >= m.0 => Some("a sell stop must stay below the market"),
        _ => None,
    }
}

/// A dragged order's new price, drawn as a ghost until the broker confirms it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ghost {
    pub order: OrderId,
    pub price: Price,
    pub command: CommandId,
    /// A resize: the order's new total. Acked by that, not by a price it already rests at; and
    /// nothing is drawn for it (the mark or tag updates when the order reports it).
    pub qty: Option<Qty>,
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
        // A resize's order finished meanwhile: nothing was refused.
        None if g.qty.is_some() => GhostOutcome::Acked,
        Some(o) if g.qty.is_some() && !o.is_open() => GhostOutcome::Acked,
        None => GhostOutcome::Rejected("the order is gone".into()),
        Some(o) if !o.is_open() => GhostOutcome::Rejected(format!("order {}", o.state.label())),
        Some(o) if g.qty == Some(o.qty) => GhostOutcome::Acked,
        Some(_) if g.qty.is_some() => GhostOutcome::Pending,
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
        ("entries locked", "disabled"),
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

/// Refusals at one edge of the ladder, off its visible rows.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeRefusals {
    pub n: usize,
    /// "denied" or "rejected" when they are all one kind, else "refused".
    pub word: &'static str,
}

/// Where the off-screen refusal count goes in the orders column's title cell (`ow` wide): beside
/// the right-aligned title (`title_w`) when the count (`count_w`, padding included) fits left of
/// it, else in the title's place while it lasts (the default column is too narrow for both),
/// else nowhere (a column narrower than the words).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HiddenPlacement {
    Beside,
    InsteadOfTitle,
    None,
}

pub fn hidden_placement(ow: f32, title_w: f32, count_w: f32) -> HiddenPlacement {
    if count_w + 2.0 <= ow - title_w - 16.0 {
        HiddenPlacement::Beside
    } else if count_w + 4.0 <= ow {
        HiddenPlacement::InsteadOfTitle
    } else {
        HiddenPlacement::None
    }
}

/// Orders off the ladder's visible rows: `[above, below]`, each the nearest such order and how
/// many orders are beyond that edge (rows as `price / units` floored). Pure and
/// allocation-free.
pub fn offscreen_orders(
    marks: &[OrderMark],
    top_row: i64,
    bottom_row: i64,
    units: i64,
) -> [Option<(OrderMark, usize)>; 2] {
    let mut out: [Option<(OrderMark, usize)>; 2] = [None, None];
    for m in marks {
        let row = m.price.0.div_euclid(units.max(1));
        let k = if row > top_row {
            0
        } else if row < bottom_row {
            1
        } else {
            continue;
        };
        out[k] = Some(match out[k] {
            None => (*m, 1),
            Some((near, n)) => {
                let closer = if k == 0 {
                    m.price.0 < near.price.0
                } else {
                    m.price.0 > near.price.0
                };
                (if closer { *m } else { near }, n + 1)
            }
        });
    }
    out
}

/// Refused prices above the top row and below the bottom row (rows as `price / units`
/// floored), each with its word: `[above, below]`. Warnings are not refusals. Pure and
/// allocation-free.
pub fn offscreen_refusals(
    flashes: impl Iterator<Item = (Price, &'static str)>,
    top_row: i64,
    bottom_row: i64,
    units: i64,
) -> [EdgeRefusals; 2] {
    let mut out = [EdgeRefusals::default(); 2];
    for (price, label) in flashes {
        let row = price.0.div_euclid(units.max(1));
        let k = if row > top_row {
            0
        } else if row < bottom_row {
            1
        } else {
            continue;
        };
        let word = match label {
            "DENIED" => "denied",
            "REJECTED" => "rejected",
            _ => continue,
        };
        let e = &mut out[k];
        e.word = if e.n == 0 || e.word == word {
            word
        } else {
            "refused"
        };
        e.n += 1;
    }
    out
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
            equity: false,
            option: None,
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
        assert!(a.apply(ArmEvent::Toggle, ArmPrefs::default()));
        a
    }

    fn spec(c: &TradeCommand) -> &OrderSpec {
        match c {
            TradeCommand::PlaceOrder(o) => o,
            TradeCommand::PlaceBracket(b) => &b.entry,
            other => panic!("not an order: {other:?}"),
        }
    }

    /// Focused review: at the orders column's default width (~84 px, sized for two marks) the
    /// count didn't fit beside "Ord" and never showed. It takes the title's place instead.
    #[test]
    fn hidden_count_shows_at_the_default_column_width() {
        // "1 hidden" bold at 12 px with padding ~ 56 px; "Ord" ~ 20 px.
        assert_eq!(
            hidden_placement(84.0, 20.0, 56.0),
            HiddenPlacement::InsteadOfTitle
        );
        assert_eq!(hidden_placement(110.0, 20.0, 56.0), HiddenPlacement::Beside);
        assert_eq!(hidden_placement(50.0, 20.0, 56.0), HiddenPlacement::None);
    }

    /// The owner's case: a target off the DOM's visible rows was invisible everywhere. The
    /// nearest order beyond each edge is found (and how many there are).
    #[test]
    fn offscreen_orders_name_the_nearest_at_each_edge() {
        let mk = |row: i64, role| OrderMark {
            id: OrderId(row as u64),
            side: Side::Sell,
            price: Price(row * 25),
            style: MarkStyle::Working,
            qty: Qty(1),
            filled: Qty(0),
            stop: false,
            role,
            exit: false,
        };
        let marks = [
            mk(14, Role::TakeProfit),
            mk(12, Role::TakeProfit),
            mk(8, Role::Single),
            mk(3, Role::StopLoss),
        ];
        let e = offscreen_orders(&marks, 10, 6, 25);
        let (top, n) = e[0].unwrap();
        assert_eq!(
            (top.price, n),
            (Price(12 * 25), 2),
            "the nearer of two above"
        );
        let (bottom, n) = e[1].unwrap();
        assert_eq!((bottom.price, n), (Price(3 * 25), 1));
        assert_eq!(offscreen_orders(&marks[2..3], 10, 6, 25), [None, None]);
    }

    /// The ladder accounts for every refusal the header counts: ones off the visible rows are
    /// counted (and warnings are not refusals).
    #[test]
    fn offscreen_refusals_are_counted_at_their_edge() {
        let at = |row: i64| Price(row * 25);
        let none = offscreen_refusals(
            [(at(10), "DENIED"), (at(6), "REJECTED")].into_iter(),
            10,
            6,
            25,
        );
        assert_eq!(
            none,
            [EdgeRefusals::default(); 2],
            "on screen: tagged on their rows"
        );
        let e = offscreen_refusals(
            [
                (at(12), "DENIED"),
                (at(11), "DENIED"),
                (at(5), "REJECTED"),
                (at(2), "DENIED"),
                (at(1), "WARNING"),
            ]
            .into_iter(),
            10,
            6,
            25,
        );
        assert_eq!(
            e[0],
            EdgeRefusals {
                n: 2,
                word: "denied"
            }
        );
        assert_eq!(
            e[1],
            EdgeRefusals {
                n: 2,
                word: "refused"
            },
            "mixed kinds; no warning"
        );
        let g = offscreen_refusals([(Price(6 * 50 + 25), "REJECTED")].into_iter(), 10, 6, 50);
        assert_eq!(g[1].n, 0, "inside the bottom grouped row");
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
            Err(Refusal(ENABLE_TO_TRADE))
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
                Err(Refusal(ENABLE_TO_TRADE)),
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

    /// The switch is sticky: only the trader flips it. Focus, account and contract changes
    /// leave it enabled unless opted in.
    #[test]
    fn trading_switch_is_sticky() {
        let p = ArmPrefs::default();
        let mut a = Arming::default();
        assert!(!a.is_armed(), "new panes start disabled");
        assert!(a.apply(ArmEvent::Toggle, p) && a.is_armed());
        for ev in [
            ArmEvent::FocusLost,
            ArmEvent::AccountChanged,
            ArmEvent::ContractChanged,
        ] {
            assert!(!a.apply(ev, p) && a.is_armed(), "{ev:?} keeps it enabled");
        }
        assert!(
            a.apply(ArmEvent::Toggle, p) && !a.is_armed(),
            "toggle disables"
        );
        // Restored from a saved pane.
        assert!(Arming::new(true).is_armed());
        // Opt-in: each event disables when configured.
        let all = ArmPrefs {
            disarm_on_focus_loss: true,
            disarm_on_account_change: true,
            disarm_on_contract_change: true,
        };
        for ev in [
            ArmEvent::FocusLost,
            ArmEvent::AccountChanged,
            ArmEvent::ContractChanged,
        ] {
            let mut a = armed();
            assert!(a.apply(ev, all) && !a.is_armed(), "{ev:?}");
        }
        assert_eq!(serde_json::to_string(&ArmPrefs::default()).unwrap(), "{}");
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
    fn the_size_is_per_symbol_then_the_default_then_one() {
        let mut sizes = std::collections::BTreeMap::new();
        assert_eq!(order_size(&sizes, "ES", None), 1);
        assert_eq!(order_size(&sizes, "ES", Some(3)), 3);
        sizes.insert("ES".to_owned(), 5);
        assert_eq!(order_size(&sizes, "ES", Some(3)), 5);
        assert_eq!(
            order_size(&sizes, "NQ", Some(3)),
            3,
            "another symbol's is its own"
        );
        assert_eq!(
            order_size(&sizes, "CL", Some(0)),
            1,
            "a bad saved default never trades 0"
        );
        sizes.insert("NQ".to_owned(), 0);
        assert_eq!(
            order_size(&sizes, "NQ", None),
            1,
            "a bad saved size never trades 0"
        );
    }

    #[test]
    fn a_size_over_the_maximum_says_so_and_its_order_is_refused() {
        assert_eq!(over_max(3, None), None);
        assert_eq!(over_max(3, Some(3)), None, "at the maximum is fine");
        assert_eq!(over_max(4, Some(3)).as_deref(), Some("max 3"));
        let spec = |q| OrderSpec {
            account: AccountId::new("paper-1"),
            contract: esz6(),
            side: Side::Buy,
            qty: Qty(q),
            kind: OrderKind::Market,
        };
        let place = TradeCommand::PlaceOrder(spec(4));
        assert_eq!(command_over_max(&place, Some(3)).as_deref(), Some("max 3"));
        assert_eq!(command_over_max(&place, Some(4)), None);
        let bracket = TradeCommand::PlaceBracket(BracketSpec {
            entry: spec(4),
            take_profit: Some(Price(600_000)),
            stop_loss: Some(Price(599_000)),
        });
        assert_eq!(
            command_over_max(&bracket, Some(3)).as_deref(),
            Some("max 3")
        );
        // Ways out are never capped here.
        let flatten = TradeCommand::Flatten {
            account: AccountId::new("paper-1"),
            contract: None,
        };
        assert_eq!(command_over_max(&flatten, Some(0)), None);
    }

    #[test]
    fn wheel_travel_steps_the_size_once_per_step() {
        let mut acc = 0.0;
        assert_eq!(scroll_qty(&mut acc, 0.0), None);
        assert_eq!(scroll_qty(&mut acc, 10.0), None, "a trackpad's small event");
        assert_eq!(scroll_qty(&mut acc, 10.0), None);
        assert_eq!(scroll_qty(&mut acc, 10.0), Some(true));
        assert_eq!(acc, 0.0, "the step used it up");
        assert_eq!(
            scroll_qty(&mut acc, -QTY_SCROLL_STEP),
            Some(false),
            "a wheel notch"
        );
        assert_eq!(scroll_qty(&mut acc, 20.0), None);
        assert_eq!(scroll_qty(&mut acc, -20.0), None, "a reversal starts over");
        assert_eq!(scroll_qty(&mut acc, 20.0), None);
    }

    #[test]
    fn a_stop_is_never_dragged_across_the_market() {
        let (bid, ask) = (Some(Price(600_000)), Some(Price(600_025)));
        let mut sell = order(
            OrderState::Working,
            OrderKind::Stop {
                trigger: Price(599_500),
            },
        );
        sell.side = Side::Sell;
        assert_eq!(
            stop_move_refusal(&sell, Price(599_975), bid, ask, None),
            None
        );
        assert!(
            stop_move_refusal(&sell, Price(600_000), bid, ask, None).is_some(),
            "at the bid"
        );
        assert!(stop_move_refusal(&sell, Price(601_000), bid, ask, None).is_some());
        let buy = order(
            OrderState::Working,
            OrderKind::Stop {
                trigger: Price(601_000),
            },
        );
        assert_eq!(
            stop_move_refusal(&buy, Price(600_050), bid, ask, None),
            None
        );
        assert!(
            stop_move_refusal(&buy, Price(600_025), bid, ask, None).is_some(),
            "at the ask"
        );
        // No book: the last trade; nothing at all: refused.
        assert!(
            stop_move_refusal(&buy, Price(599_000), None, None, Some(Price(600_000))).is_some()
        );
        assert!(stop_move_refusal(&buy, Price(602_000), None, None, None).is_some());
        // Limits move anywhere (a marketable limit is the trader's choice, as on a click); a
        // triggered stop-limit is a resting limit.
        let lim = order(
            OrderState::Working,
            OrderKind::Limit {
                price: Price(599_950),
            },
        );
        assert_eq!(
            stop_move_refusal(&lim, Price(601_000), bid, ask, None),
            None
        );
        let mut sl = order(
            OrderState::Working,
            OrderKind::StopLimit {
                trigger: Price(601_000),
                limit: Price(601_025),
            },
        );
        sl.triggered = true;
        assert_eq!(stop_move_refusal(&sl, Price(599_000), bid, ask, None), None);
        // A bracket's stop held for its entry (not in the market) moves anywhere; live, it
        // follows the rule.
        let mut held = order(
            OrderState::Acked,
            OrderKind::Stop {
                trigger: Price(600_800),
            },
        );
        held.side = Side::Sell;
        held.role = Role::StopLoss;
        assert_eq!(
            stop_move_refusal(&held, Price(600_900), bid, ask, None),
            None
        );
        held.state = OrderState::Working;
        assert!(stop_move_refusal(&held, Price(600_900), bid, ask, None).is_some());
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
            qty: None,
        };
        // A resize: the order already rests at its price, so only its quantity acks it, and a
        // refusal still shows.
        let sized = Ghost {
            order: OrderId(7),
            price: Price(599_950),
            command: CommandId(2),
            qty: Some(Qty(4)),
        };
        assert_eq!(ghost_outcome(&sized, Some(&o), None), GhostOutcome::Pending);
        let mut resized = o.clone();
        resized.qty = Qty(4);
        assert_eq!(
            ghost_outcome(&sized, Some(&resized), None),
            GhostOutcome::Acked
        );
        let refused = Err(Reject::new(RejectCode::Risk, "over the maximum"));
        assert_eq!(
            ghost_outcome(&sized, Some(&o), Some(&refused)),
            GhostOutcome::Rejected("over the maximum".into())
        );
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
            short_reason("bracket entry not placed: ES market is closed"),
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
        assert_eq!(short_reason(ENABLE_TO_TRADE), "disabled");
        assert_eq!(short_reason("insufficient margin"), "insuffici");
        assert_eq!(short_reason(""), "");
        for m in [
            "bracket entry not placed: ES market is closed",
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

#[cfg(test)]
#[path = "dom_keys_tests.rs"]
mod keys_tests;
