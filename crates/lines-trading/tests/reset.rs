//! Reset paper account (`core/reset.rs`): it flattens and cancels through the core first, resets
//! only once the account is flat, and refuses a broker account.

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot,
    Command, Event, FillModel, MarketInput, Money, OrderId, OrderKind, OrderSpec, OrderState,
    PaperBroker, PaperConfig, RejectCode, Side, Source, TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    fn new() -> Rig {
        let cfg = PaperConfig {
            latency_ms: LAT,
            fill_model: FillModel::TradeThrough,
            ..Default::default()
        };
        let account = AccountConfig::paper("paper-1", 50_000);
        let mut b = PaperBroker::new(cfg);
        b.open_account(account.id.clone(), Money::whole(account.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(account, None);
        Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
        }
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let id = self
            .core
            .submit(self.now(), Source::Test, Instant::now(), c);
        self.events.extend(self.core.drain());
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("every command has a result")
    }

    fn place(&mut self, side: Side, qty: i64, kind: OrderKind) -> OrderId {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        }))
        .expect("accepted")[0]
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.events.extend(self.core.drain());
    }

    /// A two-sided book around `mid` (a quarter point each side) and a trade at it.
    fn market_at(&mut self, mid: f64) {
        self.t += 1;
        let v = vec![
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: px(mid - 0.25),
                qty: Qty(50),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(mid + 0.25),
                qty: Qty(50),
            },
            MarketInput::Trade {
                time: UtcMillis(self.t),
                price: px(mid),
                qty: Qty(1),
            },
        ];
        self.core.market(self.now(), es(), &v);
        self.events.extend(self.core.drain());
    }

    fn reset(&mut self) -> Result<Vec<OrderId>, lines_trading::Reject> {
        self.cmd(Command::ResetPaper { account: acct() })
    }

    fn was_reset(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, Event::AccountReset { account, .. } if *account == acct()))
    }

    fn last_view(&self) -> lines_trading::AccountView {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Account(v) if v.account == acct() => Some(v.clone()),
                _ => None,
            })
            .expect("an account view")
    }
}

#[test]
fn a_reset_flattens_and_cancels_first_then_restores_the_starting_balance() {
    let mut r = Rig::new();
    r.market_at(6000.00);
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 2);
    let resting = r.place(Side::Sell, 1, OrderKind::Limit { price: px(6010.00) });
    r.wait(LAT);
    // Down 5 points: a loss today.
    r.market_at(5995.00);
    assert!(r.last_view().day_pnl < Money::ZERO);

    r.reset().expect("a paper account resets");
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::ResetWaiting { account } if *account == acct())),
        "it says the reset waits for the flatten"
    );
    assert!(!r.was_reset(), "not before the flatten has filled");
    r.wait(LAT);

    assert_eq!(r.core.order(resting).unwrap().state, OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert!(r.was_reset());
    assert_eq!(r.core.balance(&acct()), Some(Money::whole(50_000)));
    let v = r.last_view();
    assert_eq!(v.balance, Money::whole(50_000));
    assert_eq!(v.day_pnl, Money::ZERO, "today's P&L starts over");
    assert_eq!(r.core.resets_waiting().count(), 0);
}

#[test]
fn a_flat_account_resets_at_once() {
    let mut r = Rig::new();
    r.market_at(6000.00);
    r.place(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    r.market_at(6010.00);
    r.place(Side::Sell, 1, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_ne!(r.core.balance(&acct()), Some(Money::whole(50_000)));

    r.reset().unwrap();
    assert!(r.was_reset());
    assert_eq!(r.core.balance(&acct()), Some(Money::whole(50_000)));
}

#[test]
fn a_reset_waits_while_its_flatten_is_open() {
    let mut r = Rig::new();
    r.market_at(6000.00);
    r.place(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);

    r.reset().unwrap();
    // The flatten's market order is on its way (the paper broker's latency): nothing resets
    // while it is open, however long that takes.
    assert_eq!(r.core.position(&acct(), es()), 1);
    assert!(!r.was_reset());
    assert_eq!(r.core.resets_waiting().count(), 1);
    // Asked again meanwhile: still one reset, still waiting.
    r.reset().unwrap();
    assert_eq!(r.core.resets_waiting().count(), 1);
    assert!(!r.was_reset());

    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    let resets = r
        .events
        .iter()
        .filter(|e| matches!(e, Event::AccountReset { .. }))
        .count();
    assert_eq!(resets, 1, "one reset");
    assert_eq!(r.core.balance(&acct()), Some(Money::whole(50_000)));
}

impl Rig {
    fn dropped(&self) -> Option<String> {
        self.events.iter().find_map(|e| match e {
            Event::ResetDropped { account, reason } if *account == acct() => Some(reason.clone()),
            _ => None,
        })
    }
}

/// A new order while the reset waits drops it: it never resets the trading done since.
#[test]
fn a_new_order_drops_a_waiting_reset() {
    let mut r = Rig::new();
    r.market_at(6000.00);
    r.place(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    r.reset().unwrap();
    assert_eq!(r.core.resets_waiting().count(), 1);
    r.place(Side::Buy, 1, OrderKind::Limit { price: px(5990.00) });
    assert_eq!(
        r.dropped().as_deref(),
        Some("Reset not done: a new order was placed on the account")
    );
    assert_eq!(r.core.resets_waiting().count(), 0);
    // The flatten fills and the account is flat: no reset.
    r.wait(LAT);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: None,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert!(!r.was_reset());
}

/// The flatten is refused (the market closed): the reset is dropped, never left to go off
/// once the account is flat on another day.
#[test]
fn a_flatten_that_ends_without_closing_drops_the_reset() {
    let mut r = Rig::new();
    r.market_at(6000.00);
    r.place(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    // Saturday: CME is closed; the flatten's market order is refused.
    r.t = UtcMillis::parse_iso("2026-11-14T15:00:00Z").unwrap().0;
    r.reset().unwrap();
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 1);
    assert_eq!(
        r.dropped().as_deref(),
        Some("Reset not done: its flatten ended with the position still open")
    );
    assert_eq!(r.core.resets_waiting().count(), 0);
    // Monday: the trader flattens and trades on; nothing resets.
    r.t = UtcMillis::parse_iso("2026-11-16T15:00:00Z").unwrap().0;
    r.market_at(6010.00);
    r.cmd(Command::Flatten {
        account: acct(),
        contract: None,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert!(!r.was_reset());
    assert_ne!(r.core.balance(&acct()), Some(Money::whole(50_000)));
}

#[test]
fn a_strategy_cannot_reset_an_account() {
    let mut r = Rig::new();
    let id = r.core.submit(
        r.now(),
        Source::Strategy(lines_trading::StrategyId(1)),
        Instant::now(),
        Command::ResetPaper { account: acct() },
    );
    r.events.extend(r.core.drain());
    let result = r
        .events
        .iter()
        .find_map(|e| match e {
            Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
            _ => None,
        })
        .unwrap();
    assert!(result.is_err());
    assert!(!r.was_reset());
}

/// A broker that isn't simulated: a venue's account.
struct Venue;

impl Broker for Venue {
    fn name(&self) -> &str {
        "venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        BrokerCaps {
            market_orders: true,
            stop_orders: true,
            modify: true,
            server_brackets: true,
            oco: true,
            fractional_qty: false,
            external_fills: false,
        }
    }
    fn request(&mut self, _now: UtcMillis, _req: BrokerRequest) {}
    fn market(&mut self, _now: UtcMillis, _c: ContractKey, _i: &MarketInput) {}
    fn poll(&mut self, _now: UtcMillis, _out: &mut Vec<BrokerEvent>) {}
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot::default()
    }
}

#[test]
fn a_broker_account_is_refused_and_says_why() {
    let mut core = TradingCore::new(Box::new(Venue));
    let mut cfg = AccountConfig::paper("venue-1", 50_000);
    cfg.name = "Evaluation".to_owned();
    core.add_account(cfg, None);
    let now = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap();
    let id = core.submit(
        now,
        Source::Test,
        Instant::now(),
        Command::ResetPaper {
            account: AccountId::new("venue-1"),
        },
    );
    let events = core.drain();
    let result = events
        .iter()
        .find_map(|e| match e {
            Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
            _ => None,
        })
        .unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.code, RejectCode::Broker);
    assert_eq!(
        err.message,
        "Evaluation is a broker account: only a paper account can be reset"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AccountReset { .. } | Event::ResetWaiting { .. }))
    );
}
