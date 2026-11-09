//! The chain pane's order ticket (decision 0028): what it holds and what it would send, apart
//! from drawing it (`chain_view.rs`).
//!
//! Prices are exact and stay what the trader sees: the ticket takes the mid when it opens and
//! never moves with the quote, so the send button always says what Enter sends. The core
//! decides (`risk::check_option`); [`refusal`] asks the same check first, so the reason shows
//! before the press and in the core's words.

use lines_core::{ContractKey, OptionKey, OptionQuote, OptionRules, Price, Product, Qty, Right};
use lines_trading::risk::{
    OptionGuard, OptionQuoteView, PROP_OPTIONS_REFUSAL, check_option, check_option_exit,
};
use lines_trading::types::ContractSpec;
use lines_trading::{AccountId, Command, CommandId, Money, OrderKind, OrderSpec, Side};

/// The settings key of an underlying's option order size: shared by its chain panes (decision
/// 0028), never by its shares. 100 shares of SPY is a normal size; 100 contracts is 10,000
/// shares.
pub fn size_key(symbol: &str) -> String {
    format!("{symbol} options")
}

/// Whether a press on a quote cell sends at once: One-click on, in a pane already focused (the
/// click that only brings the pane forward sends nothing), and only a double-click's first.
pub fn click_sends(one_click: bool, focused: bool, clicks: usize) -> bool {
    one_click && focused && clicks <= 1
}

/// Whether a quote is past the guard's limit: nothing about it can be used (nor shown as live).
pub fn quote_stale(q: &OptionQuote, guard: &OptionGuard, now: lines_core::UtcMillis) -> bool {
    now.0 - q.time.0 > guard.stale_after_ms
}

/// A cell of the quote: which side of the market a click or Enter takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    Bid,
    Ask,
}

impl Cell {
    /// Clicking an ask buys, clicking a bid sells.
    pub fn side(self) -> Side {
        match self {
            Cell::Bid => Side::Sell,
            Cell::Ask => Side::Buy,
        }
    }

    pub fn other(self) -> Cell {
        match self {
            Cell::Bid => Cell::Ask,
            Cell::Ask => Cell::Bid,
        }
    }
}

/// The usable sides of a quote: a missing, zero or crossed side is none (the core's rule).
pub fn sides(q: Option<&OptionQuote>) -> (Option<Price>, Option<Price>) {
    q.map_or((None, None), |q| {
        lines_trading::market::quote_sides(q.bid, q.ask)
    })
}

/// The mid as a limit on the option's tick grid, never inside out: a half tick or an off-grid
/// mid rounds away from the market (a buy down, a sell up), and stays between the bid and ask.
pub fn mid_limit(side: Side, bid: Price, ask: Price, rules: OptionRules) -> Price {
    let sum = bid.0 + ask.0;
    let floor = sum.div_euclid(2);
    let tick = rules.tick.at(Price(floor)).max(1);
    let below = floor - floor.rem_euclid(tick);
    let p = match side {
        Side::Buy => below,
        Side::Sell if below == floor && sum.rem_euclid(2) == 0 => below,
        Side::Sell => below + tick,
    };
    Price(p.clamp(bid.0, ask.0))
}

/// The limit a ticket opens with: the mid; `None` without a two-sided quote.
pub fn default_limit(side: Side, q: Option<&OptionQuote>, rules: OptionRules) -> Option<Price> {
    match sides(q) {
        (Some(b), Some(a)) => Some(mid_limit(side, b, a, rules)),
        _ => None,
    }
}

/// The limit a close opens with: the mid, or with only its own side of the quote, that side,
/// which the core takes with no mid (`risk::check_option_exit`).
pub fn close_limit(side: Side, q: Option<&OptionQuote>, rules: OptionRules) -> Option<Price> {
    default_limit(side, q, rules).or_else(|| {
        let (bid, ask) = sides(q);
        match side {
            Side::Sell => bid,
            Side::Buy => ask,
        }
    })
}

/// One tick up or down the option's grid, and never to zero.
pub fn step_price(p: Price, up: bool, rules: OptionRules) -> Price {
    if up {
        let tick = rules.tick.at(p).max(1);
        Price(p.0 - p.0.rem_euclid(tick) + tick)
    } else {
        // The tick below this price: at a split's threshold it's the smaller one.
        let tick = rules.tick.at(Price(p.0 - 1)).max(1);
        let rem = p.0.rem_euclid(tick);
        let to = p.0 - if rem == 0 { tick } else { rem };
        Price(if to > 0 { to } else { p.0 })
    }
}

/// The ticket docked at the bottom of the pane.
#[derive(Clone, Debug)]
pub struct Ticket {
    pub key: OptionKey,
    pub side: Side,
    pub price: Option<Price>,
    /// The price field being typed (replaces `price` once it reads as a price).
    pub typed: Option<String>,
    /// Reduce-only: it closes the position, in full.
    pub close: bool,
    /// Sent, waiting for the core's answer: a second Enter sends nothing.
    pub sent: Option<CommandId>,
    /// The core's refusal of what was sent, until the ticket changes.
    pub refused: Option<String>,
    /// "When SPY": an entry trigger at this price of the underlying rather than an order now
    /// (decision 0029), and `price` is its max. A buy that doesn't close only.
    pub when: Option<Price>,
    /// Keys go to the level rather than the max, and what's typed so far.
    pub on_level: bool,
    pub level_typed: Option<String>,
}

impl Ticket {
    pub fn new(key: OptionKey, side: Side, q: Option<&OptionQuote>, rules: OptionRules) -> Ticket {
        Ticket {
            key,
            side,
            price: default_limit(side, q, rules),
            typed: None,
            close: false,
            sent: None,
            refused: None,
            when: None,
            on_level: false,
            level_typed: None,
        }
    }

    /// Whether this ticket may be "When SPY": an entry only buys, and a close is an order now.
    pub fn can_be_when(&self) -> bool {
        self.side == Side::Buy && !self.close
    }

    /// To "When SPY" at `level`, its max starting at the ask; or back to now, at the mid.
    pub fn set_when(&mut self, level: Option<Price>, q: Option<&OptionQuote>, rules: OptionRules) {
        self.when = level;
        self.on_level = level.is_some();
        self.level_typed = None;
        self.typed = None;
        self.refused = None;
        self.price = if level.is_some() {
            sides(q).1.or(self.price)
        } else {
            default_limit(self.side, q, rules)
        };
    }

    pub fn set_level(&mut self, level: Price) {
        self.when = Some(level);
        self.level_typed = None;
        self.refused = None;
    }

    /// A move of the level by the arrow keys: `step` units, on the underlying's price.
    pub fn step_level(&mut self, up: bool, step: i64) {
        if let Some(l) = self.when {
            let to = if up { l.0 + step } else { l.0 - step };
            self.set_level(Price(to.max(step)));
        }
    }

    pub fn set_price(&mut self, p: Price) {
        self.price = Some(p);
        self.typed = None;
        self.refused = None;
    }

    pub fn step(&mut self, up: bool, rules: OptionRules) {
        if let Some(p) = self.price {
            self.set_price(step_price(p, up, rules));
        }
    }

    /// A key in the price field: the first replaces what's there. Other characters aren't taken.
    pub fn type_char(&mut self, c: char, product: &Product) -> bool {
        if !c.is_ascii_digit() && c != '.' {
            return false;
        }
        if self.when.is_some() && self.on_level {
            let t = self.level_typed.get_or_insert_with(String::new);
            if c != '.' || !t.contains('.') {
                t.push(c);
            }
            self.read_level(product);
            return true;
        }
        let t = self.typed.get_or_insert_with(String::new);
        if c == '.' && t.contains('.') {
            return true;
        }
        t.push(c);
        self.read_typed(product);
        true
    }

    /// Take the last typed character off; `false` when nothing is being typed.
    pub fn backspace(&mut self, product: &Product) -> bool {
        if self.when.is_some() && self.on_level {
            let Some(t) = self.level_typed.as_mut() else {
                return false;
            };
            t.pop();
            self.read_level(product);
            return true;
        }
        let Some(t) = self.typed.as_mut() else {
            return false;
        };
        t.pop();
        self.read_typed(product);
        true
    }

    /// What's typed so far, if it reads; the last good level otherwise, so a line never jumps.
    fn read_level(&mut self, product: &Product) {
        self.refused = None;
        if let Some(l) = self
            .level_typed
            .as_deref()
            .and_then(|t| product.price.scale.parse_decimal(t).ok())
            .filter(|p| *p > 0)
        {
            self.when = Some(Price(l));
        }
    }

    fn read_typed(&mut self, product: &Product) {
        self.refused = None;
        self.price = self
            .typed
            .as_deref()
            .and_then(|t| product.price.scale.parse_decimal(t).ok())
            .map(Price);
    }

    /// Close the position instead of trading `side`, priced at the mid again (or back).
    pub fn set_close(&mut self, close: bool, q: Option<&OptionQuote>, rules: OptionRules) {
        self.close = close;
        self.when = None;
        self.level_typed = None;
        self.price = if close {
            close_limit(self.side, q, rules)
        } else {
            default_limit(self.side, q, rules)
        };
        self.typed = None;
        self.refused = None;
    }
}

/// Whether a position can be closed by trading `side`, and how many that takes.
pub fn closes(position: i64, side: Side) -> Option<i64> {
    (position != 0 && position.signum() == -side.sign()).then(|| position.abs())
}

/// Whether an order opens, so a guardrail lockout stops it: as the core counts an exit, what it
/// can close is the position less everything already working on that side.
pub fn order_opens(qty: i64, position: i64, side: Side, working: i64) -> bool {
    let closable = closes(position, side).unwrap_or(0) - working;
    qty > closable.max(0)
}

/// What decides a ticket: the account's standing and the guard's inputs.
pub struct Check<'a> {
    pub product: &'a Product,
    pub contract: ContractKey,
    pub guard: &'a OptionGuard,
    pub quote: Option<&'a OptionQuote>,
    pub now: Option<lines_core::UtcMillis>,
    /// The account has prop rules.
    pub prop: bool,
    /// Outside the option's session: the broker would reject it on arrival.
    pub closed: bool,
    /// A guardrail lockout or cooldown, for an order that opens: exits always pass.
    pub locked: Option<&'a str>,
    /// Why the account can't be traded through Lines.
    pub read_only: Option<&'a str>,
}

/// Why this limit won't go, in the core's words (`None`: it will, as far as the UI can tell).
pub fn refusal(check: &Check<'_>, price: Option<Price>, close: Option<Side>) -> Option<String> {
    let closing = close.is_some();
    if let Some(r) = check.read_only {
        return Some(r.to_owned());
    }
    if check.prop && !closing {
        return Some(PROP_OPTIONS_REFUSAL.to_owned());
    }
    if check.closed {
        return Some(format!("{} market is closed", check.product.symbol));
    }
    if let Some(l) = check.locked {
        return Some(l.to_owned());
    }
    let spec = ContractSpec::new(check.product.clone(), check.contract, Money::ZERO)?;
    let (bid, ask) = sides(check.quote);
    let view = OptionQuoteView {
        bid,
        ask,
        at: check.quote.map(|q| q.time),
    };
    // A price the check would refuse anyway stands in, so its quote reasons still read first.
    let rules_tick = check.product.options().map_or(1, |r| r.tick.at(Price(1)));
    let (priced, stand_in) = match price {
        Some(p) => (p, false),
        None => (Price(rules_tick.max(1)), true),
    };
    let now = check.now.unwrap_or(lines_core::UtcMillis(0));
    // A close at its own side of the quote passes however wide it is, as in the core.
    let kind = OrderKind::Limit { price: priced };
    let checked = match close {
        Some(side) => check_option_exit(check.guard, &spec, side, &kind, view, now),
        None => check_option(check.guard, &spec, &kind, view, now),
    };
    match checked {
        Err(e) if !stand_in || bid.is_none() || ask.is_none() => Some(e.message),
        Err(e) if e.message.contains("stale") => Some(e.message),
        _ if stand_in => Some("type a limit price".to_owned()),
        _ => None,
    }
}

/// The order a ticket sends: reduce-only when it closes.
pub fn command(
    account: AccountId,
    contract: ContractKey,
    side: Side,
    qty: i64,
    price: Price,
    close: bool,
) -> Command {
    let spec = OrderSpec {
        account,
        contract,
        side,
        qty: Qty(qty),
        kind: OrderKind::Limit { price },
    };
    if close {
        Command::PlaceExit(spec)
    } else {
        Command::PlaceOrder(spec)
    }
}

/// The order a ticket's send is: the shared size, or in Close mode the whole position, which the
/// ticket's side must reduce. Its reason otherwise. The view adds the guard's check and the send.
pub fn ticket_order(
    t: &Ticket,
    account: AccountId,
    contract: ContractKey,
    size: i64,
    position: i64,
    closing: bool,
) -> Result<Command, String> {
    let price = t.price.ok_or_else(|| "type a limit price".to_owned())?;
    let qty = if t.close {
        if closing {
            return Err("a close is already working; Cancel it first".to_owned());
        }
        closes(position, t.side).ok_or_else(|| "no position to close".to_owned())?
    } else {
        size
    };
    Ok(command(account, contract, t.side, qty, price, t.close))
}

/// What a "When SPY" ticket arms: the same ticket and size as an order now, as an entry trigger.
/// No order is sent until it fires, and then the core asks every new-order check again.
pub fn when_trigger(
    t: &Ticket,
    account: AccountId,
    contract: ContractKey,
    size: i64,
) -> Result<Command, String> {
    let level = t
        .when
        .ok_or_else(|| "set the price of the underlying".to_owned())?;
    let max_price = t.price.ok_or_else(|| "type a max price".to_owned())?;
    if !t.can_be_when() {
        return Err("only a buy can wait for the underlying".to_owned());
    }
    if size <= 0 {
        return Err("quantity must be positive".to_owned());
    }
    Ok(Command::ArmTrigger(lines_trading::trigger::TriggerSpec {
        account,
        contract,
        kind: lines_trading::trigger::TriggerKind::Entry {
            qty: Qty(size),
            max_price,
        },
        level,
    }))
}

/// The order a One-click press is: the clicked side's own price; `None` on a missing side.
pub fn click_order(
    account: AccountId,
    contract: ContractKey,
    cell: Cell,
    q: Option<&OptionQuote>,
    qty: i64,
) -> Option<Command> {
    let price = click_limit(cell, q)?;
    Some(command(account, contract, cell.side(), qty, price, false))
}

/// Every option series of an underlying the account holds or works, on every expiry.
pub fn held_contracts(
    st: &crate::trading::TradeState,
    account: &AccountId,
    product: lines_core::ProductId,
) -> Vec<ContractKey> {
    let mut out = Vec::new();
    let contracts = st
        .positions_of(account)
        .map(|p| p.contract)
        .chain(st.open_orders_of(account).map(|o| o.contract))
        // Triggers count as orders (the owner's ruling): Cancel all takes them too.
        .chain(
            st.triggers()
                .filter(|t| &t.account == account)
                .map(|t| t.contract),
        );
    for c in contracts {
        if c.product == product && c.option_key().is_some() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Flatten (cancel the orders, close the position at market) or Cancel All for each series: the
/// DOM's own commands, one per contract, because a chain shows many.
pub fn exit_commands(
    account: &AccountId,
    contracts: &[ContractKey],
    flatten: bool,
) -> Vec<Command> {
    contracts
        .iter()
        .map(|c| {
            let (account, contract) = (account.clone(), Some(*c));
            if flatten {
                Command::Flatten { account, contract }
            } else {
                Command::CancelAll { account, contract }
            }
        })
        .collect()
}

/// A DOM's exits act on its contract; these act on the underlying's options, and say so.
pub fn exit_label(symbol: &str, flatten: bool) -> String {
    let word = if flatten { "Flatten" } else { "Cancel" };
    format!("{word} {symbol} options")
}

/// The row's one-press Close: reduce-only for the whole position, at the mid. Its side always
/// reduces, and the core holds `PlaceExit` to the position, so it can't flip or open.
pub fn close_command(
    account: AccountId,
    contract: ContractKey,
    position: i64,
    q: Option<&OptionQuote>,
    rules: OptionRules,
) -> Option<Command> {
    if position == 0 {
        return None;
    }
    let side = if position > 0 { Side::Sell } else { Side::Buy };
    let price = close_limit(side, q, rules)?;
    Some(command(
        account,
        contract,
        side,
        position.abs(),
        price,
        true,
    ))
}

/// What a One-click press takes: the clicked side's own price, if it has one.
pub fn click_limit(cell: Cell, q: Option<&OptionQuote>) -> Option<Price> {
    let (bid, ask) = sides(q);
    match cell {
        Cell::Bid => bid,
        Cell::Ask => ask,
    }
}

/// `▲ BUY` / `▼ SELL`: a word and an arrow, never color alone.
pub fn side_word(side: Side) -> &'static str {
    match side {
        Side::Buy => "▲ BUY",
        Side::Sell => "▼ SELL",
    }
}

/// `684`, or `682.50`: a strike without a whole number's zeros.
pub fn strike_text(product: &Product, strike: Price) -> String {
    let s = product.price.format::<32>(strike);
    let s = s.as_str();
    s.strip_suffix(".00").unwrap_or(s).to_owned()
}

/// `SPY Nov 9 684 Call`.
pub fn contract_name(product: &Product, key: OptionKey, expiry: &str) -> String {
    let right = match key.right {
        Right::Call => "Call",
        Right::Put => "Put",
    };
    format!(
        "{} {expiry} {} {right}",
        product.symbol,
        strike_text(product, key.strike)
    )
}

/// The send button: `Buy 1 at 2.41`, `Close 2 at 3.10`.
pub fn send_label(
    product: &Product,
    side: Side,
    close: bool,
    qty: i64,
    p: Option<Price>,
) -> String {
    let word = match (close, side) {
        (true, _) => "Close",
        (false, Side::Buy) => "Buy",
        (false, Side::Sell) => "Sell",
    };
    match p {
        Some(p) => format!("{word} {qty} at {}", product.price.format::<32>(p).as_str()),
        None => format!("{word} {qty}"),
    }
}

/// Premium for the order: what a buy costs or a sell brings in, with the multiplier. Exact.
pub fn premium(product: &Product, contract: ContractKey, qty: i64, p: Price) -> Option<Money> {
    let spec = ContractSpec::new(product.clone(), contract, Money::ZERO)?;
    Some(spec.money(i128::from(p.0), i128::from(qty)))
}

/// A position on its row: `+2 @ 3.10`, `-1 @ 0.88`.
pub fn write_position(
    out: &mut impl std::fmt::Write,
    product: &Product,
    qty: i64,
    avg: Option<f64>,
) -> std::fmt::Result {
    write!(out, "{qty:+}")?;
    if let Some(a) = avg {
        out.write_str(" @ ")?;
        crate::trade_bar::write_avg(out, product, a)?;
    }
    Ok(())
}

/// The account's prop rules in force now: they change from Settings, so this is the core's
/// latest report, and the account's starting preset only until the first one.
pub fn has_rules(st: &crate::trading::TradeState, account: &AccountId, at_start: bool) -> bool {
    st.account(account)
        .map_or(at_start, |v| v.limits.rules_in_force.is_some())
}

/// A venue account's refusal: option orders are paper only until a live broker is enabled.
pub fn paper_only(venue: &str) -> String {
    format!("{venue} takes no option orders here: paper only")
}

/// Working orders beside a position: `S 2`, `S 2 +1`.
pub fn write_order_short(
    out: &mut impl std::fmt::Write,
    side: Side,
    open: i64,
    more: u32,
) -> std::fmt::Result {
    out.write_char(side_letter(side))?;
    write!(out, " {open}")?;
    if more > 0 {
        write!(out, " +{more}")?;
    }
    Ok(())
}

/// What a narrow row has room for: `+3`, `-1`, or an order's `S2`.
pub fn write_compact(
    out: &mut impl std::fmt::Write,
    position: i64,
    order: Option<(Side, i64)>,
) -> std::fmt::Result {
    match (position, order) {
        (0, Some((side, open))) => write!(out, "{}{open}", side_letter(side)),
        (p, _) => write!(out, "{p:+}"),
    }
}

fn side_letter(side: Side) -> char {
    match side {
        Side::Buy => 'B',
        Side::Sell => 'S',
    }
}

/// A working order on its row: `S 2 @ 1.60`, `B 1 @ 2.40`.
pub fn write_order(
    out: &mut impl std::fmt::Write,
    product: &Product,
    side: Side,
    open: i64,
    p: Option<Price>,
) -> std::fmt::Result {
    out.write_char(side_letter(side))?;
    write!(out, " {open}")?;
    if let Some(p) = p {
        out.write_str(" @ ")?;
        product.price.write(p, out)?;
    }
    Ok(())
}

/// The screenshot fixture `--chain-ticket`: a ticket open on the soonest expiry, One-click on,
/// or both. `buy 684 call`, `sell 690 put at 9.00`, `sell 684 call close`, `one-click`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShotTicket {
    pub open: Option<(Cell, String, Right)>,
    pub at: Option<String>,
    pub close: bool,
    pub one_click: bool,
    /// The ticket is "When SPY" at this level.
    pub when: Option<String>,
    /// A line of this kind (`stop`, `target`, `entry`) is being dragged to this level.
    pub drag: Option<(String, String)>,
    /// A Stop or Target not armed yet, on the open ticket's series.
    pub draft: Option<(String, String)>,
    /// Armed rows read as after a restart.
    pub restored: bool,
}

#[cfg(any(test, feature = "screenshot"))]
pub fn parse_shot(s: &str) -> Result<ShotTicket, String> {
    let mut out = ShotTicket::default();
    let w: Vec<&str> = s.split_whitespace().collect();
    let mut rest = w.as_slice();
    if let [
        side @ ("buy" | "sell"),
        strike,
        right @ ("call" | "put"),
        tail @ ..,
    ] = rest
    {
        let cell = if *side == "buy" { Cell::Ask } else { Cell::Bid };
        let right = if *right == "call" {
            Right::Call
        } else {
            Right::Put
        };
        out.open = Some((cell, (*strike).to_owned(), right));
        rest = tail;
    }
    while let [first, tail @ ..] = rest {
        match (*first, tail) {
            ("at", [p, more @ ..]) => {
                out.at = Some((*p).to_owned());
                rest = more;
            }
            ("close", more) => {
                out.close = true;
                rest = more;
            }
            ("one-click", more) => {
                out.one_click = true;
                rest = more;
            }
            ("when", [p, more @ ..]) => {
                out.when = Some((*p).to_owned());
                rest = more;
            }
            ("drag", [kind, p, more @ ..]) => {
                out.drag = Some(((*kind).to_owned(), (*p).to_owned()));
                rest = more;
            }
            ("draft", [kind, p, more @ ..]) => {
                out.draft = Some(((*kind).to_owned(), (*p).to_owned()));
                rest = more;
            }
            ("restored", more) => {
                out.restored = true;
                rest = more;
            }
            _ => return Err(format!("--chain-ticket {s:?}: unexpected {first:?}")),
        }
    }
    if out.open.is_none() && !out.one_click && out.drag.is_none() && !out.restored {
        return Err(format!(
            "--chain-ticket {s:?}: expected `buy|sell STRIKE call|put [at PRICE] [close] [when LEVEL] [draft KIND LEVEL]`, `drag KIND LEVEL`, `restored` or `one-click`"
        ));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "chain_ticket_tests.rs"]
mod path_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{CivilDate, OptionTick};
    use lines_feed::catalog;

    fn spy() -> Product {
        catalog::get(catalog::SPY).unwrap()
    }

    fn rules() -> OptionRules {
        spy().options().unwrap()
    }

    fn nickel() -> OptionRules {
        OptionRules {
            tick: OptionTick::Flat(5),
            ..rules()
        }
    }

    fn split() -> OptionRules {
        OptionRules {
            tick: OptionTick::Split {
                below: 1,
                threshold: 300,
                above: 5,
            },
            ..rules()
        }
    }

    fn key() -> OptionKey {
        OptionKey {
            expiry: CivilDate::new(2026, 11, 9).unwrap(),
            strike: Price(68_400),
            right: Right::Call,
        }
    }

    fn contract() -> ContractKey {
        ContractKey::option(catalog::SPY, key().expiry, key().strike, key().right)
    }

    fn quote(bid: Option<i64>, ask: Option<i64>) -> OptionQuote {
        OptionQuote {
            bid: bid.map(Price),
            ask: ask.map(Price),
            time: lines_core::UtcMillis(1_000_000),
            ..Default::default()
        }
    }

    fn check<'a>(p: &'a Product, g: &'a OptionGuard, q: Option<&'a OptionQuote>) -> Check<'a> {
        Check {
            product: p,
            contract: contract(),
            guard: g,
            quote: q,
            now: Some(lines_core::UtcMillis(1_000_500)),
            prop: false,
            closed: false,
            locked: None,
            read_only: None,
        }
    }

    /// Clicking an ask buys and a bid sells, and Enter on a selected cell does the same.
    #[test]
    fn an_ask_buys_and_a_bid_sells() {
        assert_eq!(Cell::Ask.side(), Side::Buy);
        assert_eq!(Cell::Bid.side(), Side::Sell);
        assert_eq!(Cell::Ask.other(), Cell::Bid);
    }

    #[test]
    fn the_default_limit_is_the_mid_on_the_grid() {
        let m = |s, b, a, r| mid_limit(s, Price(b), Price(a), r).0;
        assert_eq!(m(Side::Buy, 240, 242, rules()), 241);
        assert_eq!(m(Side::Sell, 240, 242, rules()), 241);
        // A half tick rounds away from the market.
        assert_eq!(m(Side::Buy, 240, 241, rules()), 240);
        assert_eq!(m(Side::Sell, 240, 241, rules()), 241);
        // A coarser grid, never outside the quote.
        assert_eq!(m(Side::Buy, 240, 250, nickel()), 245);
        assert_eq!(m(Side::Sell, 241, 244, nickel()), 244);
        assert_eq!(m(Side::Buy, 241, 244, nickel()), 241);
        // No mid without both sides, and a crossed quote has neither.
        let one = quote(Some(240), None);
        assert_eq!(default_limit(Side::Buy, Some(&one), rules()), None);
        let crossed = quote(Some(250), Some(240));
        assert_eq!(default_limit(Side::Buy, Some(&crossed), rules()), None);
        assert_eq!(default_limit(Side::Buy, None, rules()), None);
    }

    #[test]
    fn arrows_move_by_the_options_tick_and_never_to_zero() {
        let s = |p, up, r| step_price(Price(p), up, r).0;
        assert_eq!(s(241, true, rules()), 242);
        assert_eq!(s(241, false, rules()), 240);
        assert_eq!(s(1, false, rules()), 1, "not to zero");
        // Across a split: 0.01 below 3.00, 0.05 from there on.
        assert_eq!(s(299, true, split()), 300);
        assert_eq!(s(300, true, split()), 305);
        assert_eq!(s(305, false, split()), 300);
        assert_eq!(s(300, false, split()), 299);
        // Off the grid: onto it first.
        assert_eq!(s(302, true, nickel()), 305);
        assert_eq!(s(302, false, nickel()), 300);
    }

    #[test]
    fn the_ticket_keeps_its_price_and_the_field_types() {
        let q = quote(Some(240), Some(242));
        let p = spy();
        let mut t = Ticket::new(key(), Side::Buy, Some(&q), rules());
        assert_eq!(t.price, Some(Price(241)), "the mid");
        t.step(true, rules());
        assert_eq!(t.price, Some(Price(242)));
        // The first key replaces, digits build, a second point is ignored.
        assert!(t.type_char('2', &p));
        assert_eq!(t.price, Some(Price(200)));
        assert!(t.type_char('.', &p));
        assert!(t.type_char('.', &p));
        assert!(t.type_char('5', &p));
        assert_eq!(t.typed.as_deref(), Some("2.5"));
        assert_eq!(t.price, Some(Price(250)));
        assert!(!t.type_char('x', &p), "letters aren't the field's");
        assert!(t.backspace(&p));
        assert!(t.backspace(&p));
        assert!(t.backspace(&p));
        assert_eq!(t.price, None, "nothing typed: no price");
        // An answer stands until the ticket changes.
        t.refused = Some("no".into());
        t.set_price(Price(241));
        assert_eq!(t.refused, None);
        assert_eq!(t.typed, None);
        assert!(!t.backspace(&p), "nothing being typed");
    }

    /// A lockout stops what opens; an exit always passes, as the core counts one.
    #[test]
    fn what_opens_is_counted_as_the_core_does() {
        assert!(order_opens(1, 0, Side::Buy, 0), "flat");
        assert!(order_opens(1, 2, Side::Buy, 0), "adds");
        assert!(!order_opens(2, 2, Side::Sell, 0), "closes");
        assert!(order_opens(3, 2, Side::Sell, 0), "goes through flat");
        assert!(
            order_opens(2, 2, Side::Sell, 1),
            "one of them already works"
        );
        assert!(
            order_opens(1, 5, Side::Sell, 5),
            "a close works for all of it"
        );
        assert!(
            !order_opens(1, -3, Side::Buy, 2),
            "closes what's left of a short"
        );
    }

    /// Close prices at the mid again, and a ticket only offers it against the position.
    #[test]
    fn close_is_against_the_position_at_the_mid() {
        assert_eq!(closes(2, Side::Sell), Some(2));
        assert_eq!(closes(-3, Side::Buy), Some(3));
        assert_eq!(closes(2, Side::Buy), None, "adds");
        assert_eq!(closes(-3, Side::Sell), None, "adds");
        assert_eq!(closes(0, Side::Sell), None);
        let q = quote(Some(240), Some(242));
        let mut t = Ticket::new(key(), Side::Sell, Some(&q), rules());
        t.set_price(Price(300));
        t.set_close(true, Some(&q), rules());
        assert!(t.close);
        assert_eq!(t.price, Some(Price(241)));
    }

    /// The row's Close is reduce-only for the whole position on the reducing side, and sends
    /// nothing without a mid or a position.
    #[test]
    fn the_rows_close_is_reduce_only() {
        let a = AccountId::new("paper");
        let q = quote(Some(240), Some(242));
        let long = close_command(a.clone(), contract(), 2, Some(&q), rules()).unwrap();
        let Command::PlaceExit(s) = long else {
            panic!("{long:?}");
        };
        assert_eq!((s.side, s.qty), (Side::Sell, Qty(2)));
        assert_eq!(s.kind, OrderKind::Limit { price: Price(241) });
        let short = close_command(a.clone(), contract(), -3, Some(&q), rules()).unwrap();
        let Command::PlaceExit(s) = short else {
            panic!("{short:?}");
        };
        assert_eq!((s.side, s.qty), (Side::Buy, Qty(3)));
        assert!(close_command(a.clone(), contract(), 0, Some(&q), rules()).is_none());
        let one = quote(Some(240), None);
        // Only its own side: that side, which the core takes with no mid.
        let bid_only = close_command(a.clone(), contract(), 2, Some(&one), rules()).unwrap();
        let Command::PlaceExit(s) = bid_only else {
            panic!("{bid_only:?}");
        };
        assert_eq!(s.kind, OrderKind::Limit { price: Price(240) });
        // Only the other side: nothing to close on.
        assert!(close_command(a.clone(), contract(), -2, Some(&one), rules()).is_none());
        assert!(close_command(a.clone(), contract(), 2, None, rules()).is_none());
        // A ticket's own order is reduce-only only in Close mode.
        let plain = command(a.clone(), contract(), Side::Sell, 1, Price(241), false);
        assert!(matches!(plain, Command::PlaceOrder(_)));
        let exit = command(a, contract(), Side::Sell, 1, Price(241), true);
        assert!(matches!(exit, Command::PlaceExit(_)));
    }

    /// Only an aimed single click sends: not the click that brings the pane forward, and not a
    /// double-click's second press.
    #[test]
    fn only_an_aimed_click_sends() {
        assert!(click_sends(true, true, 1));
        assert!(!click_sends(false, true, 1), "the switch is off");
        assert!(!click_sends(true, false, 1), "the pane wasn't focused");
        assert!(!click_sends(true, true, 2), "a double-click sends once");
    }

    /// One-click takes the clicked side's own price, and nothing on a missing side.
    #[test]
    fn one_click_needs_the_side_it_takes() {
        let q = quote(Some(240), Some(242));
        assert_eq!(click_limit(Cell::Ask, Some(&q)), Some(Price(242)));
        assert_eq!(click_limit(Cell::Bid, Some(&q)), Some(Price(240)));
        let one = quote(Some(240), None);
        assert_eq!(click_limit(Cell::Ask, Some(&one)), None);
        assert_eq!(click_limit(Cell::Bid, Some(&one)), Some(Price(240)));
        let crossed = quote(Some(250), Some(240));
        assert_eq!(click_limit(Cell::Bid, Some(&crossed)), None);
        assert_eq!(click_limit(Cell::Ask, None), None);
    }

    /// The reasons the ticket shows are the core's, before the press.
    #[test]
    fn refusals_read_in_the_cores_words() {
        let p = spy();
        let g = OptionGuard::default();
        let q = quote(Some(240), Some(242));
        let ok = check(&p, &g, Some(&q));
        assert_eq!(refusal(&ok, Some(Price(241)), None), None);
        // Too far from the mid: the same message the core gives.
        let far = refusal(&ok, Some(Price(1_000)), None).unwrap();
        assert!(far.contains("too far from the mid"), "{far}");
        // One-sided, missing, crossed: no two-sided quote.
        for q in [
            quote(Some(240), None),
            quote(None, Some(242)),
            quote(Some(250), Some(240)),
        ] {
            let r = refusal(&check(&p, &g, Some(&q)), Some(Price(241)), None).unwrap();
            assert!(r.contains("no two-sided quote"), "{r}");
            let r = refusal(&check(&p, &g, Some(&q)), None, None).unwrap();
            assert!(r.contains("no two-sided quote"), "no price either: {r}");
        }
        // Stale: past the guard's limit.
        let mut late = check(&p, &g, Some(&q));
        late.now = Some(lines_core::UtcMillis(q.time.0 + g.stale_after_ms + 1));
        let r = refusal(&late, Some(Price(241)), None).unwrap();
        assert!(r.contains("stale"), "{r}");
        // A prop-rule account can't open, in the core's own words, and closing isn't refused.
        let mut prop = check(&p, &g, Some(&q));
        prop.prop = true;
        assert_eq!(
            refusal(&prop, Some(Price(241)), None).as_deref(),
            Some(PROP_OPTIONS_REFUSAL)
        );
        assert_eq!(refusal(&prop, Some(Price(241)), Some(Side::Sell)), None);
        // Outside the session, in the broker's words: no order goes just to be rejected.
        let mut closed = check(&p, &g, Some(&q));
        closed.closed = true;
        assert_eq!(
            refusal(&closed, Some(Price(241)), Some(Side::Sell)).as_deref(),
            Some("SPY market is closed")
        );
        // A guardrail lockout, in its own line.
        let mut locked = check(&p, &g, Some(&q));
        locked.locked = Some("Locked · max 5 trades reached · until the next trade date");
        assert!(
            refusal(&locked, Some(Price(241)), None)
                .unwrap()
                .starts_with("Locked")
        );
        // A read-only account's reason comes first.
        let mut ro = check(&p, &g, Some(&q));
        ro.read_only = Some("observe mode");
        assert_eq!(
            refusal(&ro, Some(Price(241)), None).as_deref(),
            Some("observe mode")
        );
        // A close at its own bid passes however wide the quote: the core takes it too.
        let wide = quote(Some(5), Some(900));
        let c = check(&p, &g, Some(&wide));
        assert_eq!(refusal(&c, Some(Price(5)), Some(Side::Sell)), None);
        assert!(
            refusal(&c, Some(Price(5)), None).is_some(),
            "an opening one doesn't"
        );
        assert!(
            refusal(&c, Some(Price(6)), Some(Side::Sell)).is_some(),
            "only at its bid"
        );
        let no_ask = quote(Some(5), None);
        let c = check(&p, &g, Some(&no_ask));
        assert_eq!(refusal(&c, Some(Price(5)), Some(Side::Sell)), None);
        // A cleared field asks for a price; a quote problem says so first.
        assert_eq!(
            refusal(&ok, None, None).as_deref(),
            Some("type a limit price")
        );
    }

    #[test]
    fn the_ticket_reads_like_the_mock() {
        let p = spy();
        assert_eq!(side_word(Side::Buy), "▲ BUY");
        assert_eq!(side_word(Side::Sell), "▼ SELL");
        assert_eq!(contract_name(&p, key(), "Nov 9"), "SPY Nov 9 684 Call");
        assert_eq!(
            send_label(&p, Side::Buy, false, 1, Some(Price(241))),
            "Buy 1 at 2.41"
        );
        assert_eq!(
            send_label(&p, Side::Sell, false, 2, Some(Price(160))),
            "Sell 2 at 1.60"
        );
        assert_eq!(
            send_label(&p, Side::Sell, true, 2, Some(Price(310))),
            "Close 2 at 3.10"
        );
        assert_eq!(send_label(&p, Side::Buy, false, 1, None), "Buy 1");
        // The multiplier, exact: 2.41 × 100 × 3.
        assert_eq!(
            premium(&p, contract(), 3, Price(241)),
            Some(Money::whole(723))
        );
        let mut s = String::new();
        write_position(&mut s, &p, 2, Some(310.0)).unwrap();
        assert_eq!(s, "+2 @ 3.10");
        s.clear();
        write_position(&mut s, &p, -1, Some(88.0)).unwrap();
        assert_eq!(s, "-1 @ 0.88");
        s.clear();
        write_order(&mut s, &p, Side::Sell, 2, Some(Price(160))).unwrap();
        assert_eq!(s, "S 2 @ 1.60");
        s.clear();
        write_order(&mut s, &p, Side::Buy, 1, Some(Price(240))).unwrap();
        assert_eq!(s, "B 1 @ 2.40");
        assert_eq!(size_key("SPY"), "SPY options");
        // A position with orders, and the narrow row's mark.
        s.clear();
        write_position(&mut s, &p, 3, Some(118.0)).unwrap();
        assert_eq!(s, "+3 @ 1.18");
        s.clear();
        write_order_short(&mut s, Side::Sell, 2, 1).unwrap();
        assert_eq!(s, "S 2 +1");
        assert_eq!(
            paper_only("Sim venue"),
            "Sim venue takes no option orders here: paper only"
        );
        s.clear();
        write_compact(&mut s, 3, Some((Side::Sell, 2))).unwrap();
        assert_eq!(s, "+3");
        s.clear();
        write_compact(&mut s, -1, None).unwrap();
        assert_eq!(s, "-1");
        s.clear();
        write_compact(&mut s, 0, Some((Side::Sell, 2))).unwrap();
        assert_eq!(s, "S2");
        // Past the guard's limit, a quote is stale.
        let g = OptionGuard::default();
        let q = quote(Some(240), Some(242));
        assert!(!quote_stale(
            &q,
            &g,
            lines_core::UtcMillis(q.time.0 + g.stale_after_ms)
        ));
        assert!(quote_stale(
            &q,
            &g,
            lines_core::UtcMillis(q.time.0 + g.stale_after_ms + 1)
        ));
    }

    /// The real core, end to end: what the ticket says beforehand is what the core answers, and
    /// the row's Close reduces and can't flip or open, however often it's pressed.
    #[test]
    fn the_core_agrees_and_close_never_flips() {
        use lines_trading::rules::presets;
        use lines_trading::{
            AccountConfig, Event, MarketInput, PaperBroker, PaperConfig, Source, TradingCore,
        };
        use std::time::Instant;

        let plain = AccountId::new("paper-1");
        let prop = AccountId::new("paper-prop");
        let mut broker = PaperBroker::new(PaperConfig {
            latency_ms: 20,
            ..Default::default()
        });
        broker.open_account(plain.clone(), Money::whole(100_000));
        broker.open_account(prop.clone(), Money::whole(100_000));
        let mut core = TradingCore::new(Box::new(broker));
        core.add_account(AccountConfig::paper("paper-1", 100_000), None);
        let mut eval = AccountConfig::paper("paper-prop", 100_000);
        eval.rules = presets().into_iter().nth(1);
        core.add_account(eval, None);

        let p = spy();
        let g = OptionGuard::default();
        // Monday 2026-11-09, 10:00 ET.
        let mut t = lines_core::UtcMillis::parse_iso("2026-11-09T15:00:00Z")
            .unwrap()
            .0;
        let set_quote = |core: &mut TradingCore, t: i64, b: Option<i64>, a: Option<i64>| {
            core.market(
                lines_core::UtcMillis(t),
                contract(),
                &[MarketInput::OptionQuote {
                    bid: b.map(Price),
                    ask: a.map(Price),
                    last: None,
                    time: lines_core::UtcMillis(t),
                }],
            );
            OptionQuote {
                time: lines_core::UtcMillis(t),
                ..quote(b, a)
            }
        };
        let send = |core: &mut TradingCore, t: i64, c: Command| {
            let id = core.submit(lines_core::UtcMillis(t), Source::Test, Instant::now(), c);
            core.drain()
                .into_iter()
                .find_map(|e| match e {
                    Event::Command { id: i, result, .. } if i == id => Some(result),
                    _ => None,
                })
                .expect("every command has an answer")
        };
        let limit = |a: &AccountId, side, qty, cents, close| {
            command(a.clone(), contract(), side, qty, Price(cents), close)
        };
        let ask = |q: &OptionQuote, now: i64, prop: bool, cents: i64| {
            let c = Check {
                product: &p,
                contract: contract(),
                guard: &g,
                quote: Some(q),
                now: Some(lines_core::UtcMillis(now)),
                prop,
                closed: false,
                locked: None,
                read_only: None,
            };
            refusal(&c, Some(Price(cents)), None)
        };

        // Each refusal, in the same words as the core.
        let q = set_quote(&mut core, t, Some(240), Some(242));
        let cases = [(&plain, false, 1_000), (&prop, true, 241)];
        for (account, is_prop, cents) in cases {
            let said = ask(&q, t, is_prop, cents).expect("the ticket says no");
            let got = send(&mut core, t, limit(account, Side::Buy, 1, cents, false));
            assert_eq!(got.unwrap_err().message, said);
        }
        t += 1;
        let one = set_quote(&mut core, t, Some(240), None);
        let said = ask(&one, t, false, 241).unwrap();
        let got = send(&mut core, t, limit(&plain, Side::Buy, 1, 241, false));
        assert_eq!(got.unwrap_err().message, said, "one-sided");
        t += 1;
        let q = set_quote(&mut core, t, Some(240), Some(242));
        let late = t + g.stale_after_ms + 1;
        let said = ask(&q, late, false, 241).unwrap();
        let got = send(&mut core, late, limit(&plain, Side::Buy, 1, 241, false));
        assert_eq!(got.unwrap_err().message, said, "stale");

        // What the ticket lets through, the core takes: buy 2 at the ask, filled there.
        t = late + 1;
        let q = set_quote(&mut core, t, Some(240), Some(242));
        assert_eq!(ask(&q, t, false, 242), None);
        send(&mut core, t, limit(&plain, Side::Buy, 2, 242, false)).expect("accepted");
        t += 21;
        core.tick(lines_core::UtcMillis(t));
        core.drain();
        assert_eq!(core.position(&plain, contract()), 2);

        // Close, at the mid: it rests. A second one the core takes too, but both only ever
        // reduce (the pane itself says a close is working and sends none).
        let close = close_command(plain.clone(), contract(), 2, Some(&q), rules()).unwrap();
        send(&mut core, t, close.clone()).expect("the first close");
        t += 21;
        core.tick(lines_core::UtcMillis(t));
        core.drain();
        assert_eq!(core.position(&plain, contract()), 2, "resting at the mid");
        send(&mut core, t, close).expect("reduce-only either way");
        // A quote through it fills the close, and the position ends flat, never short.
        t += 1;
        set_quote(&mut core, t, Some(250), Some(252));
        t += 21;
        core.tick(lines_core::UtcMillis(t));
        core.drain();
        assert_eq!(core.position(&plain, contract()), 0);
        // A stale press of Close on the flat position sends nothing.
        assert!(close_command(plain.clone(), contract(), 0, Some(&q), rules()).is_none());
        // A close sized from an old position is held to what's there: nothing to reduce.
        let old = close_command(plain.clone(), contract(), 2, Some(&q), rules()).unwrap();
        assert!(send(&mut core, t, old).is_err());
        assert_eq!(core.position(&plain, contract()), 0);
    }

    #[test]
    fn the_screenshot_fixture_parses() {
        let t = parse_shot("sell 690 put at 9.00 close one-click").unwrap();
        assert_eq!(t.open, Some((Cell::Bid, "690".to_owned(), Right::Put)));
        assert_eq!(t.at.as_deref(), Some("9.00"));
        assert!(t.close && t.one_click);
        let c = parse_shot("one-click").unwrap();
        assert!(c.open.is_none() && c.one_click);
        assert!(parse_shot("buy").is_err());
        assert!(parse_shot("buy 684 call sideways").is_err());
    }
}
