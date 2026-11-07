//! A broker that observes a venue (decision 0020 extensions in observe mode): accounts it
//! discovers after connecting join a running core, its snapshot is the truth for positions and
//! orders, and executions made outside Lines (`BrokerEvent::ExternalFill`) reach the fill
//! stream and the journal without moving positions or being counted twice.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerAccount, BrokerPosition};
use lines_trading::core::EXTERNAL_ORDER;
use lines_trading::order::Role;
use lines_trading::position::{Lot, Position};
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot,
    Event, Journal, MarketInput, Money, Order, OrderId, OrderKind, OrderState, Side, TradingCore,
};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn t() -> UtcMillis {
    UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
}

/// The venue's state, shared with the test.
#[derive(Default)]
struct Venue {
    snapshot: BrokerSnapshot,
    pending: VecDeque<BrokerEvent>,
}

struct Observer(Arc<Mutex<Venue>>);

impl Broker for Observer {
    fn name(&self) -> &str {
        "observer"
    }
    fn capabilities(&self) -> BrokerCaps {
        BrokerCaps {
            market_orders: false,
            stop_orders: false,
            modify: false,
            server_brackets: false,
            oco: false,
            fractional_qty: false,
            external_fills: true,
        }
    }
    fn request(&mut self, _now: UtcMillis, _req: BrokerRequest) {}
    fn market(&mut self, _now: UtcMillis, _c: ContractKey, _i: &MarketInput) {}
    fn poll(&mut self, _now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.extend(self.0.lock().unwrap().pending.drain(..));
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        self.0.lock().unwrap().snapshot.clone()
    }
}

fn venue_order() -> Order {
    Order {
        id: OrderId(900_001),
        account: AccountId::new("venue-sim"),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(601000),
        },
        state: OrderState::Working,
        filled: Qty(0),
        fill_value: 0,
        role: Role::Single,
        parent: None,
        oco: None,
        triggered: false,
        reduce_only: false,
        overfill: Qty(0),
        created: t(),
        updated: t(),
    }
}

fn long_two() -> Position {
    let mut p = Position::default();
    p.lots.push_back(Lot {
        price: 600000,
        qty: 2,
    });
    p
}

fn external_fill(account: &str, id: &str) -> BrokerEvent {
    BrokerEvent::ExternalFill {
        account: AccountId::new(account),
        contract: es(),
        side: Side::Buy,
        price: Price(600000),
        qty: Qty(2),
        fee: Money::ZERO,
        at: t(),
        trade_id: id.into(),
    }
}

fn setup() -> (Arc<Mutex<Venue>>, TradingCore) {
    let v = Arc::new(Mutex::new(Venue::default()));
    v.lock().unwrap().snapshot = BrokerSnapshot {
        accounts: vec![BrokerAccount {
            id: AccountId::new("venue-sim"),
            balance: Money::whole(50_000),
            positions: vec![BrokerPosition {
                contract: es(),
                position: long_two(),
            }],
            orders: vec![venue_order()],
        }],
    };
    let core = TradingCore::new(Box::new(Observer(v.clone())));
    (v, core)
}

#[test]
fn accounts_discovered_later_join_and_show_the_venue_state() {
    let (_v, mut core) = setup();
    let acct = AccountId::new("venue-sim");
    assert_eq!(
        core.position(&acct, es()),
        0,
        "unknown account: nothing yet"
    );
    let mut cfg = AccountConfig::paper("venue-sim", 0);
    cfg.name = "Venue Sim".into();
    core.add_accounts(t(), vec![cfg.clone()]);
    assert_eq!(core.position(&acct, es()), 2);
    assert_eq!(core.open_orders(&acct).count(), 1);
    let events = core.drain();
    assert!(events.iter().any(|e| matches!(e, Event::Rebuilt { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Order(o) if o.id == OrderId(900_001)))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Position(p) if p.qty == 2))
    );
    // Adding a known account again changes nothing (no second rebuild).
    core.add_accounts(t(), vec![cfg]);
    assert!(core.drain().is_empty());
}

#[test]
fn external_fills_are_recorded_once_without_moving_positions() {
    let (v, mut core) = setup();
    let acct = AccountId::new("venue-sim");
    core.add_accounts(t(), vec![AccountConfig::paper("venue-sim", 0)]);
    let _ = core.drain();
    {
        let mut v = v.lock().unwrap();
        v.pending.push_back(external_fill("venue-sim", "X1"));
        v.pending.push_back(external_fill("venue-sim", "X1")); // repeat
        v.pending.push_back(external_fill("someone-else", "X2")); // unknown account
    }
    core.tick(t());
    let events = core.drain();
    let fills: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Fill(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].order, EXTERNAL_ORDER);
    assert_eq!(fills[0].trade_id, "X1");
    assert_eq!(fills[0].realised, Money::ZERO);
    assert_eq!(
        core.position(&acct, es()),
        2,
        "the snapshot holds the position"
    );

    // The journal sees the round trip from external fills alone.
    let mut journal = Journal::new();
    for e in &events {
        journal.on_event(e);
    }
    v.lock()
        .unwrap()
        .pending
        .push_back(BrokerEvent::ExternalFill {
            account: acct.clone(),
            contract: es(),
            side: Side::Sell,
            price: Price(600100),
            qty: Qty(2),
            fee: Money::ZERO,
            at: UtcMillis(t().0 + 60_000),
            trade_id: "X3".into(),
        });
    core.tick(UtcMillis(t().0 + 60_000));
    for e in &core.drain() {
        journal.on_event(e);
    }
    assert_eq!(journal.trades().len(), 1, "one closed round trip");
}

/// A fill the venue queued before its snapshot changed is applied before the rebuilt
/// position, so the journal opens the trade once (from the fill), not twice (a seeded
/// position plus the fill).
#[test]
fn a_rebuild_applies_queued_fills_before_the_position() {
    let (v, mut core) = setup();
    {
        let mut v = v.lock().unwrap();
        v.snapshot.accounts[0].positions.clear();
        v.snapshot.accounts[0].orders.clear();
    }
    core.add_accounts(t(), vec![AccountConfig::paper("venue-sim", 0)]);
    let mut journal = Journal::new();
    for e in &core.drain() {
        journal.on_event(e);
    }
    {
        // The venue buys 2: the fill is queued and its snapshot already long 2.
        let mut v = v.lock().unwrap();
        v.pending.push_back(external_fill("venue-sim", "X1"));
        v.snapshot.accounts[0].positions = vec![BrokerPosition {
            contract: es(),
            position: long_two(),
        }];
    }
    core.rebuild(t()); // before any tick
    let events = core.drain();
    let fill = events.iter().position(|e| matches!(e, Event::Fill(_)));
    let pos = events
        .iter()
        .position(|e| matches!(e, Event::Position(p) if p.qty == 2));
    assert!(fill.unwrap() < pos.unwrap(), "{events:?}");
    for e in &events {
        journal.on_event(e);
    }
    let open: Vec<_> = journal.open_trades().collect();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].max_qty, 2, "opened once, not seeded and filled");
}

/// A venue order Lines didn't place, partly filled at the venue: its execution arrives as an
/// external fill, and the next rebuild (larger `filled`) adds no reconciled fill for it.
#[test]
fn adopted_orders_get_no_reconciled_fill_beside_their_external_fill() {
    let (v, mut core) = setup();
    core.add_accounts(t(), vec![AccountConfig::paper("venue-sim", 0)]);
    let _ = core.drain();
    {
        let mut v = v.lock().unwrap();
        let o = &mut v.snapshot.accounts[0].orders[0];
        o.filled = Qty(1);
        o.fill_value = 601000;
        o.state = OrderState::PartiallyFilled;
        v.pending.push_back(BrokerEvent::ExternalFill {
            account: AccountId::new("venue-sim"),
            contract: es(),
            side: Side::Sell,
            price: Price(601000),
            qty: Qty(1),
            fee: Money::ZERO,
            at: t(),
            trade_id: "X9".into(),
        });
    }
    core.rebuild(t());
    core.rebuild(t()); // a reconnect: still one fill
    let fills: Vec<_> = core
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            Event::Fill(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 1, "{fills:?}");
    assert_eq!(fills[0].trade_id, "X9");
}

/// A position closed at the venue (its snapshot no longer lists the contract) is reported
/// flat, never left showing.
#[test]
fn a_position_gone_from_the_snapshot_is_reported_flat() {
    let (v, mut core) = setup();
    let acct = AccountId::new("venue-sim");
    core.add_accounts(t(), vec![AccountConfig::paper("venue-sim", 0)]);
    let _ = core.drain();
    v.lock().unwrap().snapshot.accounts[0].positions.clear();
    core.rebuild(t());
    assert_eq!(core.position(&acct, es()), 0);
    assert!(
        core.drain()
            .iter()
            .any(|e| matches!(e, Event::Position(p) if p.contract == es() && p.qty == 0)),
        "a flat position event"
    );
}

/// Executions reported before their account joined are held and applied when it does.
#[test]
fn fills_before_their_account_joins_are_kept() {
    let (v, mut core) = setup();
    v.lock()
        .unwrap()
        .pending
        .push_back(external_fill("venue-sim", "X1"));
    core.tick(t()); // polled before the account is added
    assert!(core.drain().is_empty());
    core.add_accounts(t(), vec![AccountConfig::paper("venue-sim", 0)]);
    let fills = core
        .drain()
        .into_iter()
        .filter(|e| matches!(e, Event::Fill(f) if f.trade_id == "X1"))
        .count();
    assert_eq!(fills, 1);
}
