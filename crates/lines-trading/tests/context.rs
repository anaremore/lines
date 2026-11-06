//! Per-request context (issuing pane, its contract choice, the front month, manual vs
//! automated) reaches the broker with every request; the paper broker ignores it.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_trading::{
    AccountConfig, AccountId, BracketSpec, Broker, BrokerCaps, BrokerEvent, BrokerRequest,
    BrokerSnapshot, Command, ContractSelection, Event, MarketInput, Money, OrderKind, OrderSpec,
    Origin, PaneContext, PaperBroker, PaperConfig, RequestContext, Side, Source, TradingCore,
};

fn es(m: u8) -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(m).unwrap())
}

fn t0() -> UtcMillis {
    UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
}

/// Records each request's kind and context; acks nothing.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<(&'static str, RequestContext)>>>);

impl Broker for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn capabilities(&self) -> BrokerCaps {
        BrokerCaps {
            market_orders: true,
            stop_orders: true,
            modify: true,
            server_brackets: true,
            oco: true,
            fractional_qty: false,
        }
    }
    fn request(&mut self, _now: UtcMillis, _req: BrokerRequest) {
        panic!("the core must call request_in_context");
    }
    fn request_in_context(&mut self, _now: UtcMillis, req: BrokerRequest, ctx: &RequestContext) {
        let kind = match req {
            BrokerRequest::Place(_) => "place",
            BrokerRequest::Bracket { .. } => "bracket",
            BrokerRequest::Modify { .. } => "modify",
            BrokerRequest::Cancel { .. } => "cancel",
        };
        self.0.lock().unwrap().push((kind, *ctx));
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

fn pane(selection: ContractSelection, front: Option<ContractKey>) -> PaneContext {
    PaneContext {
        contract: es(12),
        selection,
        front_month: front,
    }
}

fn limit_spec() -> OrderSpec {
    OrderSpec {
        account: AccountId::new("paper-1"),
        contract: es(12),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(600000),
        },
    }
}

fn core_with(b: Box<dyn Broker>) -> TradingCore {
    let mut core = TradingCore::new(b);
    core.add_account(AccountConfig::paper("paper-1", 50_000), None);
    core
}

#[test]
fn every_request_carries_the_issuing_pane_and_origin() {
    let rec = Recorder::default();
    let mut core = core_with(Box::new(rec.clone()));
    let followed = pane(ContractSelection::FollowsProduct, Some(es(12)));
    let pinned = pane(ContractSelection::Pinned, Some(es(3)));

    core.submit_from_pane(
        t0(),
        Source::Dom,
        Instant::now(),
        Some(followed),
        Command::PlaceOrder(limit_spec()),
    );
    core.submit_from_pane(
        t0(),
        Source::Chart,
        Instant::now(),
        Some(pinned),
        Command::PlaceBracket(BracketSpec {
            entry: limit_spec(),
            take_profit: Some(Price(601000)),
            stop_loss: Some(Price(599000)),
        }),
    );
    // No pane: the API, and the plain `submit`.
    core.submit(
        t0(),
        Source::Api,
        Instant::now(),
        Command::PlaceOrder(limit_spec()),
    );
    let first = core.drain().iter().find_map(|e| match e {
        Event::Command {
            result: Ok(ids), ..
        } => Some(ids[0]),
        _ => None,
    });
    core.submit_from_pane(
        t0(),
        Source::Hotkey,
        Instant::now(),
        Some(followed),
        Command::CancelOrder {
            account: AccountId::new("paper-1"),
            order: first.unwrap(),
        },
    );

    let got = rec.0.lock().unwrap().clone();
    let kinds: Vec<&str> = got.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, ["place", "bracket", "place", "cancel"]);
    assert_eq!(got[0].1, RequestContext::new(Source::Dom, Some(followed)));
    assert_eq!(got[0].1.origin, Origin::Manual);
    assert_eq!(got[1].1.pane, Some(pinned));
    assert_eq!(got[1].1.pane.unwrap().front_month, Some(es(3)));
    assert_eq!(got[2].1, RequestContext::new(Source::Api, None));
    assert_eq!(got[2].1.origin, Origin::Automated);
    assert_eq!(got[3].1.source, Source::Hotkey);
}

#[test]
fn origin_follows_the_source() {
    for (s, o) in [
        (Source::Dom, Origin::Manual),
        (Source::Chart, Origin::Manual),
        (Source::Hotkey, Origin::Manual),
        (Source::Palette, Origin::Manual),
        (Source::Test, Origin::Manual),
        (Source::Rule, Origin::Automated),
        (Source::Api, Origin::Automated),
    ] {
        assert_eq!(Origin::of(s), o, "{s:?}");
    }
}

#[test]
fn the_paper_broker_ignores_the_context() {
    let run = |p: Option<PaneContext>| {
        let mut b = PaperBroker::new(PaperConfig::default());
        b.open_account(AccountId::new("paper-1"), Money::whole(50_000));
        let mut core = core_with(Box::new(b));
        core.submit_from_pane(
            t0(),
            Source::Dom,
            Instant::now(),
            p,
            Command::PlaceOrder(limit_spec()),
        );
        core.tick(UtcMillis(t0().0 + 1_000));
        format!("{:?}", core.drain())
    };
    assert_eq!(run(None), run(Some(pane(ContractSelection::Pinned, None))));
}

#[test]
fn pane_context_round_trips_through_json() {
    let p = pane(ContractSelection::Pinned, Some(es(3)));
    let ctx = RequestContext::new(Source::Chart, Some(p));
    let v = serde_json::to_string(&ctx).unwrap();
    assert_eq!(serde_json::from_str::<RequestContext>(&v).unwrap(), ctx);
    let none = PaneContext {
        front_month: None,
        ..p
    };
    let v = serde_json::to_string(&none).unwrap();
    assert_eq!(serde_json::from_str::<PaneContext>(&v).unwrap(), none);
}
