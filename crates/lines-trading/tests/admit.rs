//! A broker's pre-check (`Broker::admit`): what it won't send is refused as the **command**
//! (DENIED, never REJECTED), before the core makes any order; nothing reaches the broker. What it
//! admits goes out as before.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_trading::{
    AccountConfig, AccountId, Admit, BracketSpec, Broker, BrokerCaps, BrokerEvent, BrokerRequest,
    BrokerSnapshot, Command, Event, MarketInput, OrderKind, OrderSpec, RejectCode, RequestContext,
    Side, Source, TradingCore,
};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn t0() -> UtcMillis {
    UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
}

/// Admits only limit entries for brackets; records what it was asked and what was sent.
#[derive(Clone, Default)]
struct Picky {
    asked: Arc<Mutex<Vec<String>>>,
    sent: Arc<Mutex<Vec<&'static str>>>,
}

impl Broker for Picky {
    fn name(&self) -> &str {
        "picky"
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
    fn request(&mut self, _now: UtcMillis, req: BrokerRequest) {
        self.sent.lock().unwrap().push(match req {
            BrokerRequest::Place(_) => "place",
            BrokerRequest::Bracket { .. } => "bracket",
            BrokerRequest::Modify { .. } => "modify",
            BrokerRequest::Cancel { .. } => "cancel",
        });
    }
    fn admit(
        &mut self,
        _now: UtcMillis,
        new: Admit<'_>,
        _ctx: &RequestContext,
    ) -> Result<(), String> {
        match new {
            Admit::Place(spec, _) => {
                self.asked
                    .lock()
                    .unwrap()
                    .push(format!("place {:?}", spec.kind));
                Ok(())
            }
            Admit::Bracket(b) => {
                self.asked
                    .lock()
                    .unwrap()
                    .push(format!("bracket {:?}", b.entry.kind));
                match b.entry.kind {
                    OrderKind::Limit { .. } => Ok(()),
                    _ => Err("picky: brackets need a limit entry".into()),
                }
            }
        }
    }
    fn market(&mut self, _now: UtcMillis, _c: ContractKey, _i: &MarketInput) {}
    fn poll(&mut self, _now: UtcMillis, _out: &mut Vec<BrokerEvent>) {}
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot::default()
    }
}

fn spec(kind: OrderKind) -> OrderSpec {
    OrderSpec {
        account: AccountId::new("acct-1"),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind,
    }
}

#[test]
fn a_broker_refusal_before_sending_is_a_command_reject() {
    let b = Picky::default();
    let mut core = TradingCore::new(Box::new(b.clone()));
    core.add_account(AccountConfig::paper("acct-1", 50_000), None);
    let _ = core.drain();

    // A market-entry bracket the broker won't send: the command is refused (DENIED), no order
    // exists, nothing was sent.
    core.submit(
        t0(),
        Source::Dom,
        Instant::now(),
        Command::PlaceBracket(BracketSpec {
            entry: spec(OrderKind::Market),
            take_profit: Some(Price(601000)),
            stop_loss: Some(Price(599000)),
        }),
    );
    let events = core.drain();
    let refused = events.iter().find_map(|e| match e {
        Event::Command { result: Err(r), .. } => Some(r.clone()),
        _ => None,
    });
    let r = refused.expect("a command reject");
    assert_eq!(r.code, RejectCode::Broker);
    assert_eq!(r.message, "picky: brackets need a limit entry");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Order(_))),
        "no order made: {events:?}"
    );
    assert!(b.sent.lock().unwrap().is_empty(), "nothing sent");
    assert_eq!(core.open_orders(&AccountId::new("acct-1")).count(), 0);

    // A limit-entry bracket and a plain order are admitted and sent.
    core.submit(
        t0(),
        Source::Dom,
        Instant::now(),
        Command::PlaceBracket(BracketSpec {
            entry: spec(OrderKind::Limit {
                price: Price(600000),
            }),
            take_profit: Some(Price(601000)),
            stop_loss: Some(Price(599000)),
        }),
    );
    core.submit(
        t0(),
        Source::Dom,
        Instant::now(),
        Command::PlaceOrder(spec(OrderKind::Market)),
    );
    let events = core.drain();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, Event::Command { result: Err(_), .. })),
        "{events:?}"
    );
    assert_eq!(*b.sent.lock().unwrap(), ["bracket", "place"]);
    assert_eq!(b.asked.lock().unwrap().len(), 3, "asked before each");
}
