//! The Positions pane's content, without GPUI: every open position on every account, what it
//! is worth now, and its exits. The core decides everything; the money here is the core's own
//! (`PositionView::unrealised`), so nothing is computed twice. The last price is read back from
//! that P&L, for display.

use std::fmt::Write;

use lines_core::{ContractKey, Price, Product};
use lines_trading::trigger::{TriggerKind, TriggerView};
use lines_trading::types::ContractSpec;
use lines_trading::{AccountId, Command, Money, Order, PositionView, Role};

/// One open position, as the pane shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub account: AccountId,
    /// PAPER, SIM or LIVE, as the journal and the trading header say it.
    pub badge: &'static str,
    pub contract: ContractKey,
    pub qty: i64,
    pub avg: Option<f64>,
    pub last: Option<f64>,
    pub pnl: Money,
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

/// A position's stop and target: working exit orders, or, for an option, its triggers.
pub fn exits<'a>(
    orders: impl Iterator<Item = &'a Order>,
    triggers: impl Iterator<Item = &'a TriggerView>,
) -> (Exit, Exit) {
    let (mut stop, mut target) = (Exit::None, Exit::None);
    let add = |e: &mut Exit, p: Price| {
        *e = match *e {
            Exit::Order(first, more) => Exit::Order(first, more + 1),
            _ => Exit::Order(p, 0),
        }
    };
    for o in orders {
        let Some(p) = o.kind.prices().next() else {
            continue;
        };
        match o.role {
            Role::StopLoss => add(&mut stop, p),
            Role::TakeProfit => add(&mut target, p),
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

/// The pane's rows from the trading state: every account the app lists, positions that aren't
/// flat, the most real accounts first (LIVE before SIM before PAPER), then by contract.
pub fn rows(st: &crate::trading::TradeState, accounts: &[(AccountId, &'static str)]) -> Vec<Row> {
    let mut out = Vec::new();
    for (account, badge) in accounts {
        for p in st.positions_of(account).filter(|p| p.qty != 0) {
            out.push(row(st, account, badge, p));
        }
    }
    let rank = |b: &str| match b {
        "LIVE" => 0,
        "SIM" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| {
        rank(a.badge)
            .cmp(&rank(b.badge))
            .then(a.account.0.cmp(&b.account.0))
            .then(a.contract.product.cmp(&b.contract.product))
            .then(format!("{:?}", a.contract).cmp(&format!("{:?}", b.contract)))
    });
    out
}

fn row(
    st: &crate::trading::TradeState,
    account: &AccountId,
    badge: &'static str,
    p: &PositionView,
) -> Row {
    let product = lines_feed::catalog::get(p.contract.product);
    let last = product
        .as_ref()
        .and_then(|pr| last_price(pr, p.contract, p.qty, p.avg_price, p.unrealised));
    let (stop, target) = exits(
        st.open_orders(account, p.contract),
        st.triggers()
            .filter(|t| &t.account == account && t.contract == p.contract),
    );
    Row {
        account: account.clone(),
        badge,
        contract: p.contract,
        qty: p.qty,
        avg: p.avg_price,
        last,
        pnl: p.unrealised,
        stop,
        target,
    }
}

/// How many are open, and their P&L together.
pub fn totals(rows: &[Row]) -> (usize, Money) {
    let pnl = rows.iter().fold(Money::ZERO, |a, r| Money(a.0 + r.pnl.0));
    (rows.len(), pnl)
}

/// A row's Close: the DOM's Flatten for that contract. Reduce-only, so it can't open, grow or
/// flip a position, and it cancels that contract's orders on the account.
pub fn close(row: &Row) -> Command {
    Command::Flatten {
        account: row.account.clone(),
        contract: Some(row.contract),
    }
}

/// The header's Flatten all or Cancel all: each listed position's contract, nothing else.
pub fn exit_all(rows: &[Row], flatten: bool) -> Vec<Command> {
    rows.iter()
        .map(|r| {
            let (account, contract) = (r.account.clone(), Some(r.contract));
            if flatten {
                Command::Flatten { account, contract }
            } else {
                Command::CancelAll { account, contract }
            }
        })
        .collect()
}

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

/// `▲ Long 2`, `▼ Short 1`: the side in a shape and a word, never color alone.
pub fn write_side(out: &mut impl Write, qty: i64) -> std::fmt::Result {
    if qty > 0 {
        write!(out, "▲ Long {qty}")
    } else {
        write!(out, "▼ Short {}", -qty)
    }
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
        let (stop, target) = exits(orders.iter(), [].iter());
        assert_eq!(stop, Exit::Order(Price(580_000), 0));
        assert_eq!(target, Exit::Order(Price(582_000), 1), "and one more");
        let triggers = [
            trigger(TriggerKind::Stop, 68_250),
            trigger(TriggerKind::Target, 68_600),
        ];
        let (stop, target) = exits([].iter(), triggers.iter());
        assert_eq!(stop, Exit::Trigger(Price(68_250)));
        assert_eq!(target, Exit::Trigger(Price(68_600)));
        assert_eq!(exits([].iter(), [].iter()), (Exit::None, Exit::None));
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
        let side = |q| {
            let mut s = String::new();
            write_side(&mut s, q).unwrap();
            s
        };
        assert_eq!(side(2), "▲ Long 2");
        assert_eq!(side(-1), "▼ Short 1");
    }

    fn a_row(account: &str, contract: ContractKey, pnl: i64) -> Row {
        Row {
            account: AccountId::new(account),
            badge: "PAPER",
            contract,
            qty: 1,
            avg: None,
            last: None,
            pnl: Money::whole(pnl),
            stop: Exit::None,
            target: Exit::None,
        }
    }

    /// The header covers exactly what the pane lists: a Close and the header's exits are the
    /// DOM's Flatten and Cancel All for each listed contract, and the total is theirs.
    #[test]
    fn the_exits_and_totals_cover_what_is_listed() {
        let rows = [a_row("paper", es(), 250), a_row("paper", call(), -72)];
        assert_eq!(totals(&rows), (2, Money::whole(178)));
        assert_eq!(totals(&[]), (0, Money::ZERO));
        assert_eq!(
            close(&rows[0]),
            Command::Flatten {
                account: AccountId::new("paper"),
                contract: Some(es()),
            }
        );
        let flat = exit_all(&rows, true);
        assert_eq!(flat.len(), 2);
        assert!(flat.iter().all(|c| matches!(
            c,
            Command::Flatten {
                contract: Some(_),
                ..
            }
        )));
        let cancel = exit_all(&rows, false);
        assert!(cancel.iter().all(|c| matches!(
            c,
            Command::CancelAll {
                contract: Some(_),
                ..
            }
        )));
        assert!(
            exit_all(&[], true).is_empty(),
            "nothing listed, nothing sent"
        );
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
}
