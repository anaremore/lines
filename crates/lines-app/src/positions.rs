//! The Positions pane's content, without GPUI: every open position on every account, what it
//! is worth now, its exits, which columns fit, and what its Flatten sends. The core decides
//! everything; the money here is the core's own (`PositionView::unrealised`), so nothing is
//! computed twice. The last price is read back from that P&L, for display.

use std::fmt::Write;

use lines_core::{ContractKey, Price, Product, UtcMillis};
use lines_trading::trigger::{TriggerKind, TriggerView};
use lines_trading::types::ContractSpec;
use lines_trading::{AccountId, Command, Money, Order, OrderId, OrderState, PositionView, Role};

/// One open position, as the pane shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub account: AccountId,
    /// The account's name, as the trading header shows it.
    pub name: String,
    /// PAPER, SIM or LIVE, as the journal and the trading header say it.
    pub badge: &'static str,
    pub contract: ContractKey,
    pub qty: i64,
    pub avg: Option<f64>,
    pub last: Option<f64>,
    pub pnl: Money,
    /// When the price the P&L was taken at was seen.
    pub marked_at: Option<UtcMillis>,
    pub stop: Exit,
    pub target: Exit,
}

/// A working exit on the position: an order's price, or a trigger's level on the underlying.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub enum Exit {
    #[default]
    None,
    /// A working order at this price, and how many more there are.
    Order(Price, usize),
    /// An option's trigger at this price of its underlying.
    Trigger(Price),
}

/// An account the pane may list: its id, badge and name.
pub type Listed = (AccountId, &'static str, String);

/// What one price unit of one contract is worth, in whole currency.
fn per_unit(product: &Product, contract: ContractKey) -> Option<f64> {
    let spec = ContractSpec::new(product.clone(), contract, Money::ZERO)?;
    let m = spec.money(1, 1);
    let per = m.0 as f64 / Money::UNITS_PER_WHOLE as f64;
    (per != 0.0).then_some(per)
}

/// The price the core's open P&L was taken at: average plus P&L per contract per unit.
pub fn last_price(
    product: &Product,
    contract: ContractKey,
    qty: i64,
    avg: Option<f64>,
    pnl: Money,
) -> Option<f64> {
    let avg = avg?;
    if qty == 0 {
        return None;
    }
    let per = per_unit(product, contract)?;
    let pnl = pnl.0 as f64 / Money::UNITS_PER_WHOLE as f64;
    Some(avg + pnl / (qty as f64 * per))
}

/// A position's stop and target: working orders on the side that closes it (a bracket's legs, an
/// exit, a DOM's plain stop), or, for an option, its triggers. A bracket's legs wait for their
/// entry: while it's open they aren't this position's.
pub fn exits<'a>(
    orders: impl Iterator<Item = &'a Order>,
    triggers: impl Iterator<Item = &'a TriggerView>,
    position: i64,
    entry_open: impl Fn(OrderId) -> bool,
) -> (Exit, Exit) {
    let (mut stop, mut target) = (Exit::None, Exit::None);
    let add = |e: &mut Exit, p: Price| {
        *e = match *e {
            Exit::Order(first, more) => Exit::Order(first, more + 1),
            _ => Exit::Order(p, 0),
        }
    };
    for o in orders {
        // A leg still held for its entry (not working yet) isn't this position's; once the
        // entry has filled at all, its legs work for what filled.
        let held = matches!(o.state, OrderState::Submitted | OrderState::Acked)
            && o.parent.is_some_and(&entry_open);
        if o.side.sign() != -position.signum() || held {
            continue;
        }
        let Some(p) = o.kind.prices().next() else {
            continue;
        };
        let stop_kind = matches!(
            o.kind,
            lines_trading::OrderKind::Stop { .. } | lines_trading::OrderKind::StopLimit { .. }
        );
        match o.role {
            Role::StopLoss => add(&mut stop, p),
            Role::TakeProfit => add(&mut target, p),
            Role::Flatten | Role::Entry => {}
            _ if stop_kind => add(&mut stop, p),
            _ if o.reduce_only => add(&mut target, p),
            _ => {}
        }
    }
    for t in triggers {
        match t.kind {
            TriggerKind::Stop if stop == Exit::None => stop = Exit::Trigger(t.level),
            TriggerKind::Target if target == Exit::None => target = Exit::Trigger(t.level),
            _ => {}
        }
    }
    (stop, target)
}

/// The pane's rows from the trading state: the listed accounts (one, with a filter), positions
/// that aren't flat, the most real accounts first (LIVE before SIM before PAPER), then by contract.
pub fn rows(
    st: &crate::trading::TradeState,
    accounts: &[Listed],
    filter: Option<&AccountId>,
) -> Vec<Row> {
    let mut out = Vec::new();
    for (account, badge, name) in accounts {
        if filter.is_some_and(|f| f != account) {
            continue;
        }
        for p in st.positions_of(account).filter(|p| p.qty != 0) {
            out.push(row(st, account, badge, name, p));
        }
    }
    out.sort_by(|a, b| {
        rank(a.badge)
            .cmp(&rank(b.badge))
            .then(a.account.0.cmp(&b.account.0))
            .then(a.contract.cmp(&b.contract))
    });
    out
}

fn rank(badge: &str) -> u8 {
    match badge {
        "LIVE" => 0,
        "SIM" => 1,
        _ => 2,
    }
}

/// The header's tag: the most real account listed (LIVE, then SIM, then PAPER); `None` with
/// nothing listed.
pub fn header_badge(accounts: &[Listed], filter: Option<&AccountId>) -> Option<&'static str> {
    accounts
        .iter()
        .filter(|(a, _, _)| filter.is_none_or(|f| f == a))
        .map(|(_, b, _)| *b)
        .min_by_key(|b| rank(b))
}

fn row(
    st: &crate::trading::TradeState,
    account: &AccountId,
    badge: &'static str,
    name: &str,
    p: &PositionView,
) -> Row {
    let product = lines_feed::catalog::get(p.contract.product);
    // No price yet: no Last to read back (the P&L is a placeholder zero).
    let last = product
        .as_ref()
        .filter(|_| p.marked_at.is_some())
        .and_then(|pr| last_price(pr, p.contract, p.qty, p.avg_price, p.unrealised));
    let (stop, target) = exits(
        st.open_orders(account, p.contract),
        st.triggers()
            .filter(|t| &t.account == account && t.contract == p.contract),
        p.qty,
        |id| st.order(id).is_some_and(|o| o.is_open()),
    );
    Row {
        account: account.clone(),
        name: name.to_owned(),
        badge,
        contract: p.contract,
        qty: p.qty,
        avg: p.avg_price,
        last,
        pnl: p.unrealised,
        marked_at: p.marked_at,
        stop,
        target,
    }
}

/// What became of a Flatten's market order sent from the pane: `Some(word)` when it ended
/// without closing (said in the header, the broker's own reason when it gave one), `None` when
/// it closed or is still working. `open`: the position is still open; `replaced`: another
/// Flatten works on it now.
pub fn flatten_outcome(state: &OrderState, open: bool, replaced: bool) -> Option<String> {
    match state {
        OrderState::Rejected { reason } => Some(format!("couldn't close: {reason}")),
        OrderState::Cancelled if open && !replaced => {
            Some("Flatten canceled: the position is still open".to_owned())
        }
        _ => None,
    }
}

/// A press on a row's Flatten counts only once the pointer has moved since the rows last
/// changed: a row that closes or comes in slides the others, and a still pointer would be over
/// another position than the one it was aimed at.
#[derive(Clone, Debug, Default)]
pub struct PressGuard {
    rows: Vec<(AccountId, ContractKey)>,
    changed: bool,
}

impl PressGuard {
    /// The rows as they stand, each render.
    pub fn see(&mut self, rows: &[Row]) {
        let now: Vec<_> = rows
            .iter()
            .map(|r| (r.account.clone(), r.contract))
            .collect();
        if now != self.rows {
            self.rows = now;
            self.changed = true;
        }
    }

    pub fn pointer_moved(&mut self) {
        self.changed = false;
    }

    /// Whether a press now means what's under it.
    pub fn may_press(&self) -> bool {
        !self.changed
    }
}

/// Why an option's Flatten can't close it on this account: options trade on paper only here
/// (decision 0028), and the core's Flatten would cancel its orders and close nothing.
pub fn option_flatten_off(contract: ContractKey, paper: bool) -> Option<&'static str> {
    (contract.option_key().is_some() && !paper)
        .then_some("options close on paper only here: close it at the broker")
}

/// How many are open, and their P&L together.
pub fn totals(rows: &[Row]) -> (usize, Money) {
    let pnl = rows.iter().fold(Money::ZERO, |a, r| Money(a.0 + r.pnl.0));
    (rows.len(), pnl)
}

/// `3 positions`, `1 position`.
pub fn count_words(n: usize) -> String {
    if n == 1 {
        "1 position".to_owned()
    } else {
        format!("{n} positions")
    }
}

/// Flatten all's question: `Flatten 3 positions?`.
pub fn flatten_prompt(n: usize) -> String {
    format!("Flatten {}?", count_words(n))
}

/// A row's Flatten: the DOM's Flatten for that account and contract (cancel everything, then
/// close; the owner's rule).
pub fn flatten(row: &Row) -> Command {
    Command::Flatten {
        account: row.account.clone(),
        contract: Some(row.contract),
    }
}

/// Flatten all: each listed position's own Flatten, nothing else.
pub fn flatten_all(rows: &[Row]) -> Vec<Command> {
    rows.iter().map(flatten).collect()
}

/// Whether a row's P&L is older than its market allows: an option's quote age limit by expiry,
/// anything else a few seconds without a price (a feed that stopped). No price yet is stale too.
pub fn stale(row: &Row, now: UtcMillis) -> bool {
    let limit = if row.contract.option_key().is_some() {
        lines_trading::risk::option_quote_stale_ms(row.contract, now)
    } else {
        STALE_MS
    };
    row.marked_at.is_none_or(|t| now.0 - t.0 > limit)
}

/// A future's or crypto's P&L with no price for this long reads stale (twice the feed's own
/// stale limit, as a trigger's underlying).
pub const STALE_MS: i64 = lines_trading::risk::DEFAULT_TRIGGER_STALE_MS;

/// Where a click on a row takes the link group: a future's own contract, and an option's
/// underlying (charts and DOMs show the underlying, and its chain the option).
pub fn target_of(contract: ContractKey) -> lines_feed::catalog::Target {
    use lines_feed::catalog::Target;
    if contract.option_key().is_some() {
        Target::Product(contract.product)
    } else {
        Target::Contract(contract)
    }
}

/// `▲ Long`, `▼ Short`: the side in a shape and a word, never color alone.
pub fn side_word(qty: i64) -> &'static str {
    if qty > 0 { "▲ Long" } else { "▼ Short" }
}

/// An exit as text: `5809.50`, `5809.50 +1`, `SPY 682.50`; nothing when there's none.
pub fn write_exit(
    out: &mut impl Write,
    exit: Exit,
    product: Option<&Product>,
    underlying: Option<&Product>,
) -> std::fmt::Result {
    match exit {
        Exit::None => Ok(()),
        Exit::Order(p, more) => {
            match product {
                Some(pr) => pr.price.write(p, out)?,
                None => write!(out, "{}", p.0)?,
            }
            if more > 0 {
                write!(out, " +{more}")?;
            }
            Ok(())
        }
        Exit::Trigger(level) => match underlying {
            Some(u) => {
                write!(out, "{} ", u.symbol)?;
                u.price.write(level, out)
            }
            None => write!(out, "{}", level.0),
        },
    }
}

/// The header's state, beside every tab: `3 open`. Nothing when nothing is.
pub fn open_label(count: usize) -> Option<String> {
    (count > 0).then(|| format!("{count} open"))
}

/// The pane's columns, left to right.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Col {
    Symbol,
    Account,
    Side,
    Qty,
    Avg,
    Last,
    Exits,
    Pnl,
    Flatten,
}

impl Col {
    pub const ALL: [Col; 9] = [
        Col::Symbol,
        Col::Account,
        Col::Side,
        Col::Qty,
        Col::Avg,
        Col::Last,
        Col::Exits,
        Col::Pnl,
        Col::Flatten,
    ];

    /// Width, logical px.
    pub fn width(self) -> f32 {
        match self {
            Col::Symbol => 132.0,
            Col::Account => 96.0,
            Col::Side => 70.0,
            Col::Qty => 44.0,
            Col::Avg => 80.0,
            Col::Last => 80.0,
            Col::Exits => 150.0,
            Col::Pnl => 96.0,
            Col::Flatten => 64.0,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Col::Symbol => "Symbol",
            Col::Account => "Account",
            Col::Side => "Side",
            Col::Qty => "Qty",
            Col::Avg => "Avg",
            Col::Last => "Last",
            Col::Exits => "Stop / Target",
            Col::Pnl => "Open P&L",
            Col::Flatten => "",
        }
    }

    /// Numbers sit right.
    pub fn right(self) -> bool {
        matches!(self, Col::Qty | Col::Avg | Col::Last | Col::Pnl)
    }
}

/// Which columns go first when the pane is narrow: right to left, Symbol, Qty, P&L and
/// Flatten last of all.
const DROP: [Col; 5] = [Col::Exits, Col::Last, Col::Avg, Col::Side, Col::Account];

/// The columns that fit `width` px, in order.
pub fn shown(width: f32) -> Vec<Col> {
    let mut cols = Col::ALL.to_vec();
    let total = |cols: &[Col]| cols.iter().map(|c| c.width()).sum::<f32>();
    for d in DROP {
        if total(&cols) <= width {
            break;
        }
        cols.retain(|c| *c != d);
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{CivilDate, Qty, Right};
    use lines_feed::catalog;
    use lines_trading::trigger::{Direction, TriggerId, TriggerState};
    use lines_trading::{OrderId, OrderKind, OrderState, Side};

    fn es() -> ContractKey {
        lines_feed::front_month(&catalog::get(catalog::ES).unwrap(), None).unwrap()
    }

    fn call() -> ContractKey {
        ContractKey::option(
            catalog::SPY,
            CivilDate::new(2026, 11, 9).unwrap(),
            Price(68_400),
            Right::Call,
        )
    }

    /// The last price read back from the core's P&L, with the multiplier: ES is $50 a point,
    /// an option 100 shares.
    #[test]
    fn the_last_price_is_the_one_the_pnl_was_taken_at() {
        let es_p = catalog::get(catalog::ES).unwrap();
        // Long 2 from 5800.00, +$250: 5802.50.
        let scale = es_p.price.scale;
        let units = |whole: f64| whole * scale.den() as f64 / scale.num() as f64;
        let last = last_price(&es_p, es(), 2, Some(units(5800.0)), Money::whole(250)).unwrap();
        assert!((last - units(5802.5)).abs() < 1e-6, "{last}");
        // Short: the same P&L means a lower price.
        let last = last_price(&es_p, es(), -2, Some(units(5800.0)), Money::whole(250)).unwrap();
        assert!((last - units(5797.5)).abs() < 1e-6, "{last}");
        let spy = catalog::get(catalog::SPY).unwrap();
        // Long 2 calls from 0.65, +$72: 1.01.
        let last = last_price(&spy, call(), 2, Some(65.0), Money::whole(72)).unwrap();
        assert!((last - 101.0).abs() < 1e-6, "{last}");
        assert_eq!(last_price(&spy, call(), 0, Some(65.0), Money::ZERO), None);
        assert_eq!(last_price(&spy, call(), 2, None, Money::ZERO), None);
    }

    fn order(role: Role, price: i64) -> Order {
        Order {
            id: OrderId(price as u64),
            account: AccountId::new("paper"),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            filled: Qty(0),
            kind: OrderKind::Limit {
                price: Price(price),
            },
            role,
            state: OrderState::Working,
            fill_value: 0,
            parent: None,
            oco: None,
            triggered: false,
            created: lines_core::UtcMillis(0),
            updated: lines_core::UtcMillis(0),
            reduce_only: false,
            overfill: Qty(0),
        }
    }

    fn trigger(kind: TriggerKind, level: i64) -> TriggerView {
        TriggerView {
            id: TriggerId(1),
            account: AccountId::new("paper"),
            contract: call(),
            kind,
            level: Price(level),
            direction: Direction::AtOrBelow,
            state: TriggerState::Armed,
            armed_by: None,
            stale: false,
            restored: false,
            note: None,
            reason: None,
        }
    }

    /// Working exit orders by role; an option's triggers when there's no order.
    #[test]
    fn exits_come_from_orders_or_triggers() {
        let orders = [
            order(Role::StopLoss, 580_000),
            order(Role::TakeProfit, 582_000),
            order(Role::TakeProfit, 583_000),
            order(Role::Entry, 579_000),
        ];
        let none = |_| false;
        let (stop, target) = exits(orders.iter(), [].iter(), 1, none);
        assert_eq!(stop, Exit::Order(Price(580_000), 0));
        assert_eq!(target, Exit::Order(Price(582_000), 1), "and one more");
        let triggers = [
            trigger(TriggerKind::Stop, 68_250),
            trigger(TriggerKind::Target, 68_600),
        ];
        let (stop, target) = exits([].iter(), triggers.iter(), 1, none);
        assert_eq!(stop, Exit::Trigger(Price(68_250)));
        assert_eq!(target, Exit::Trigger(Price(68_600)));
        assert_eq!(
            exits([].iter(), [].iter(), 1, none),
            (Exit::None, Exit::None)
        );
        // A DOM's plain stop on the closing side is the stop; one on the other side isn't.
        let mut plain = order(Role::Single, 579_500);
        plain.kind = OrderKind::Stop {
            trigger: Price(579_500),
        };
        let (stop, _) = exits(std::iter::once(&plain), [].iter(), 1, none);
        assert_eq!(stop, Exit::Order(Price(579_500), 0));
        let (stop, _) = exits(std::iter::once(&plain), [].iter(), -1, none);
        assert_eq!(stop, Exit::None, "a sell stop doesn't close a short");
        // A bracket's leg whose entry is still open isn't this position's.
        let mut leg = order(Role::StopLoss, 578_000);
        leg.parent = Some(OrderId(1));
        leg.state = OrderState::Acked;
        let (stop, _) = exits(std::iter::once(&leg), [].iter(), 1, |_| true);
        assert_eq!(stop, Exit::None, "held for its entry");
        // Its entry partly filled: the leg works for what filled.
        leg.state = OrderState::Working;
        let (stop, _) = exits(std::iter::once(&leg), [].iter(), 1, |_| true);
        assert_eq!(stop, Exit::Order(Price(578_000), 0));
        let (stop, _) = exits(std::iter::once(&leg), [].iter(), 1, none);
        assert_eq!(stop, Exit::Order(Price(578_000), 0));
    }

    /// A Flatten from the pane that ends without closing is said, in the broker's words; one
    /// that closed, still works, or was replaced by another Flatten isn't.
    #[test]
    fn a_flatten_that_cant_close_is_said() {
        let rejected = OrderState::Rejected {
            reason: "market closed".into(),
        };
        assert_eq!(
            flatten_outcome(&rejected, true, false).as_deref(),
            Some("couldn't close: market closed")
        );
        assert_eq!(
            flatten_outcome(&OrderState::Cancelled, true, false).as_deref(),
            Some("Flatten canceled: the position is still open")
        );
        assert_eq!(
            flatten_outcome(&OrderState::Cancelled, false, false),
            None,
            "flat"
        );
        assert_eq!(
            flatten_outcome(&OrderState::Cancelled, true, true),
            None,
            "replaced"
        );
        assert_eq!(flatten_outcome(&OrderState::Filled, false, false), None);
        assert_eq!(flatten_outcome(&OrderState::Working, true, false), None);
    }

    #[test]
    fn exits_read_as_prices_and_underlying_levels() {
        let es_p = catalog::get(catalog::ES).unwrap();
        let spy = catalog::get(catalog::SPY).unwrap();
        let text = |e, p: Option<&Product>, u: Option<&Product>| {
            let mut s = String::new();
            write_exit(&mut s, e, p, u).unwrap();
            s
        };
        assert_eq!(
            text(Exit::Order(Price(580_950), 0), Some(&es_p), None),
            "5809.50"
        );
        assert_eq!(
            text(Exit::Order(Price(580_950), 2), Some(&es_p), None),
            "5809.50 +2"
        );
        assert_eq!(
            text(Exit::Trigger(Price(68_250)), None, Some(&spy)),
            "SPY 682.50"
        );
        assert_eq!(text(Exit::None, Some(&es_p), None), "");
    }

    #[test]
    fn the_side_is_a_shape_and_a_word() {
        assert_eq!(side_word(2), "▲ Long");
        assert_eq!(side_word(-1), "▼ Short");
    }

    fn a_row(account: &str, contract: ContractKey, pnl: i64) -> Row {
        Row {
            account: AccountId::new(account),
            name: account.to_owned(),
            badge: "PAPER",
            contract,
            qty: 1,
            avg: None,
            last: None,
            pnl: Money::whole(pnl),
            marked_at: Some(UtcMillis(1_000)),
            stop: Exit::None,
            target: Exit::None,
        }
    }

    /// A row's Flatten and Flatten all are the DOM's Flatten for each listed account and
    /// contract, and the total is theirs.
    #[test]
    fn flatten_and_totals_cover_what_is_listed() {
        let rows = [a_row("paper", es(), 250), a_row("sim", call(), -72)];
        assert_eq!(totals(&rows), (2, Money::whole(178)));
        assert_eq!(totals(&[]), (0, Money::ZERO));
        assert_eq!(
            flatten(&rows[0]),
            Command::Flatten {
                account: AccountId::new("paper"),
                contract: Some(es()),
            }
        );
        assert_eq!(
            flatten_all(&rows),
            vec![
                Command::Flatten {
                    account: AccountId::new("paper"),
                    contract: Some(es()),
                },
                Command::Flatten {
                    account: AccountId::new("sim"),
                    contract: Some(call()),
                },
            ]
        );
        assert!(flatten_all(&[]).is_empty(), "nothing listed, nothing sent");
        assert_eq!(flatten_prompt(3), "Flatten 3 positions?");
        assert_eq!(flatten_prompt(1), "Flatten 1 position?");
        assert_eq!(count_words(0), "0 positions");
    }

    /// A future switches the group to its contract; an option to its underlying.
    #[test]
    fn a_click_switches_to_what_charts_show() {
        use lines_feed::catalog::Target;
        assert_eq!(target_of(es()), Target::Contract(es()));
        assert_eq!(target_of(call()), Target::Product(catalog::SPY));
    }

    #[test]
    fn the_open_label_says_only_when_something_is() {
        assert_eq!(open_label(0), None);
        assert_eq!(open_label(3), Some("3 open".to_owned()));
    }

    /// A narrow pane gives up columns right to left; Symbol, Qty, P&L and Flatten last of all.
    #[test]
    fn narrow_panes_drop_columns_right_to_left() {
        let full: f32 = Col::ALL.iter().map(|c| c.width()).sum();
        assert_eq!(shown(full), Col::ALL.to_vec());
        let without_exits = shown(full - 1.0);
        assert!(!without_exits.contains(&Col::Exits));
        assert!(without_exits.contains(&Col::Last));
        let narrow = shown(200.0);
        assert_eq!(narrow, vec![Col::Symbol, Col::Qty, Col::Pnl, Col::Flatten]);
        // Narrower still: the four stay (the pane clips rather than lose the Flatten).
        assert_eq!(shown(50.0), narrow);
    }

    /// A P&L with no price for longer than its market allows reads stale, never silently frozen.
    #[test]
    fn a_pnl_with_no_recent_price_is_stale() {
        let t = UtcMillis::parse_iso("2026-11-09T15:00:00Z").unwrap();
        let mut r = a_row("paper", es(), 10);
        r.marked_at = Some(t);
        assert!(!stale(&r, UtcMillis(t.0 + STALE_MS)));
        assert!(stale(&r, UtcMillis(t.0 + STALE_MS + 1)));
        r.marked_at = None;
        assert!(stale(&r, t), "no price yet");
        // An option by its expiry's limit: 10 s on the day it expires.
        let mut o = a_row("paper", call(), 10);
        o.marked_at = Some(t);
        assert!(!stale(&o, UtcMillis(t.0 + 10_000)));
        assert!(stale(&o, UtcMillis(t.0 + 10_001)));
    }

    /// An option on an account that isn't paper can't be flattened here; anything else can.
    #[test]
    fn an_option_off_paper_says_why_its_flatten_is_off() {
        assert!(option_flatten_off(call(), false).is_some());
        assert_eq!(option_flatten_off(call(), true), None);
        assert_eq!(option_flatten_off(es(), false), None);
    }

    /// Rows that move under a still pointer hold a press until it moves.
    #[test]
    fn a_press_waits_for_the_pointer_after_the_rows_move() {
        let mut g = PressGuard::default();
        let rows = [a_row("paper", es(), 0), a_row("paper", call(), 0)];
        g.see(&rows);
        g.pointer_moved();
        assert!(g.may_press());
        g.see(&rows);
        assert!(g.may_press(), "the same rows");
        g.see(&rows[1..]);
        assert!(!g.may_press(), "one closed: the other slid up");
        g.pointer_moved();
        assert!(g.may_press());
    }

    /// The header's tag is the most real account listed.
    #[test]
    fn the_header_badge_is_the_most_real_listed() {
        let accounts: Vec<Listed> = vec![
            (AccountId::new("paper"), "PAPER", "Paper".into()),
            (AccountId::new("sim"), "SIM", "Sim".into()),
        ];
        assert_eq!(header_badge(&accounts, None), Some("SIM"));
        assert_eq!(
            header_badge(&accounts, Some(&AccountId::new("paper"))),
            Some("PAPER")
        );
        assert_eq!(header_badge(&[], None), None);
    }
}
