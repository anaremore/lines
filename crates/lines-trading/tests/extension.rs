//! The broker extension point (decisions 0004, 0018, 0020): a broker registered from outside
//! the public crates is driven by the trading core exactly like a built-in one. The dummy
//! extensions here are made up; nothing names a vendor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_trading::extension::{
    AccountKind, AccountSource, BrokerExtension, ExtensionAccount, summarize, validate,
};
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot,
    Command, Event, MarketInput, Money, OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig,
    Side, Source, TradingCore,
};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

struct Accounts {
    rev: AtomicU64,
}

impl AccountSource for Accounts {
    fn accounts(&self) -> Vec<ExtensionAccount> {
        vec![
            ExtensionAccount {
                id: AccountId::new("demo-sim"),
                label: "Demo Sim".into(),
                kind: AccountKind::Simulated,
                tradable: Ok(()),
            },
            ExtensionAccount {
                id: AccountId::new("demo-live"),
                label: "Demo ••42".into(),
                kind: AccountKind::Live,
                tradable: Err("live entry not enabled".into()),
            },
            ExtensionAccount {
                id: AccountId::new("demo-other"),
                label: "Demo other".into(),
                kind: AccountKind::Unknown,
                tradable: Err("account mode unknown".into()),
            },
        ]
    }
    fn revision(&self) -> u64 {
        self.rev.load(Ordering::Relaxed)
    }
}

/// An extension whose broker is a paper broker under another name.
struct DummyVenue {
    name: &'static str,
    refuse: bool,
}

/// A broker that refuses every request, like a connector in observe mode.
struct Refusing;

impl Broker for Refusing {
    fn name(&self) -> &str {
        "refusing"
    }
    fn capabilities(&self) -> BrokerCaps {
        caps(false)
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

/// Wraps `Refusing` so requests are answered with a typed rejection.
struct RefusingWithReasons {
    pending: Vec<BrokerEvent>,
}

impl Broker for RefusingWithReasons {
    fn name(&self) -> &str {
        "refusing"
    }
    fn capabilities(&self) -> BrokerCaps {
        Refusing.capabilities()
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        if let BrokerRequest::Place(o) = req {
            self.pending.push(BrokerEvent::Rejected {
                id: o.id,
                reason: "observe mode: order entry disabled".into(),
                at: now,
            });
        }
    }
    fn market(&mut self, _now: UtcMillis, _c: ContractKey, _i: &MarketInput) {}
    fn poll(&mut self, _now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.append(&mut self.pending);
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        self.pending.first().map(|_| UtcMillis(0))
    }
    fn snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot::default()
    }
}

fn caps(on: bool) -> BrokerCaps {
    BrokerCaps {
        market_orders: on,
        stop_orders: on,
        modify: on,
        server_brackets: on,
        oco: on,
        fractional_qty: false,
    }
}

impl BrokerExtension for DummyVenue {
    fn name(&self) -> &str {
        self.name
    }
    fn label(&self) -> &str {
        "Dummy venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        caps(!self.refuse)
    }
    fn accounts(&self) -> Arc<dyn AccountSource> {
        Arc::new(Accounts {
            rev: AtomicU64::new(1),
        })
    }
    fn open(&self) -> Result<Box<dyn Broker>> {
        if self.refuse {
            return Ok(Box::new(RefusingWithReasons {
                pending: Vec::new(),
            }));
        }
        let mut b = PaperBroker::new(PaperConfig::default());
        b.open_account(AccountId::new("demo-sim"), Money::whole(50_000));
        Ok(Box::new(b))
    }
}

fn venue(name: &'static str) -> Arc<dyn BrokerExtension> {
    Arc::new(DummyVenue {
        name,
        refuse: false,
    })
}

#[test]
fn names_are_validated() {
    assert!(validate(&[venue("dummy-venue"), venue("other-1")]).is_ok());
    assert!(validate(&[]).is_ok());
    for bad in ["paper", "replay", "", "Dummy", "a b", "x_y"] {
        assert!(validate(&[venue(bad)]).is_err(), "{bad:?}");
    }
    let err = validate(&[venue("dup"), venue("dup")]).unwrap_err();
    assert!(err.to_string().contains("two broker extensions"), "{err}");
}

#[test]
fn accounts_summarize_by_kind_without_ids() {
    let ext = venue("dummy-venue");
    let s = summarize(&ext.accounts().accounts());
    assert_eq!((s.simulated, s.live, s.unknown, s.tradable), (1, 1, 1, 1));
    assert_eq!(ext.accounts().revision(), 1);
}

fn place(core: &mut TradingCore, now: UtcMillis) -> lines_trading::OrderId {
    let id = core.submit(
        now,
        Source::Test,
        Instant::now(),
        Command::PlaceOrder(OrderSpec {
            account: AccountId::new("demo-sim"),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(600000),
            },
        }),
    );
    let events = core.drain();
    events
        .iter()
        .find_map(|e| match e {
            Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
            _ => None,
        })
        .expect("command result")
        .expect("accepted by the core")[0]
}

#[test]
fn the_core_drives_an_extension_broker_like_a_built_in_one() {
    let ext = venue("dummy-venue");
    let mut core = TradingCore::new(ext.open().unwrap());
    core.add_account(AccountConfig::paper("demo-sim", 50_000), None);
    // Tuesday, CME open.
    let t = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap();
    let id = place(&mut core, t);
    core.tick(UtcMillis(t.0 + 1_000));
    assert_eq!(core.order(id).unwrap().state, OrderState::Working);
}

#[test]
fn an_extension_broker_rejection_reaches_the_order() {
    let ext: Arc<dyn BrokerExtension> = Arc::new(DummyVenue {
        name: "observing",
        refuse: true,
    });
    assert_eq!(ext.capabilities(), caps(false));
    let mut core = TradingCore::new(ext.open().unwrap());
    core.add_account(AccountConfig::paper("demo-sim", 50_000), None);
    let t = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap();
    let id = place(&mut core, t);
    core.tick(UtcMillis(t.0 + 1));
    let OrderState::Rejected { reason } = &core.order(id).unwrap().state else {
        panic!("{:?}", core.order(id).unwrap().state)
    };
    assert_eq!(reason, "observe mode: order entry disabled");
}
