//! Triggers on an option's underlying (decision 0029), one rule per test, on scripted time and
//! quotes. The broker is the paper broker, except where a test needs a partial fill, which paper
//! quotes (unlimited size) never give.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{CivilDate, ContractKey, Price, Qty, Right, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::guardrails::Guardrails;
use lines_trading::persist::{self, TradingFile};
use lines_trading::rules::presets;
use lines_trading::trigger::{Direction, TriggerId, TriggerKind, TriggerSpec, TriggerState};
use lines_trading::{
    AccountConfig, AccountId, Command, Event, MarketInput, Order, OrderId, OrderKind, OrderSpec,
    PaperBroker, PaperConfig, RejectCode, Side, Source, TradingCore,
};

const LAT: i64 = 20;
const STALE: i64 = 10_000;
const CHASE: i64 = 3_000;
/// Monday 2026-11-09, 10:00 ET: the regular session.
const T0: &str = "2026-11-09T15:00:00Z";

fn series(strike: i64, right: Right) -> ContractKey {
    ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 9).unwrap(),
        Price(strike * 100),
        right,
    )
}

fn call() -> ContractKey {
    series(684, Right::Call)
}

fn put() -> ContractKey {
    series(684, Right::Put)
}

fn spy() -> ContractKey {
    ContractKey::equity(catalog::SPY)
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
    fn with(account: AccountConfig) -> Rig {
        let cfg = PaperConfig {
            latency_ms: LAT,
            ..Default::default()
        };
        let mut b = PaperBroker::new(cfg);
        b.open_account(account.id.clone(), lines_trading::Money::whole(100_000));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(account, None);
        Rig::on(core)
    }

    fn on(core: TradingCore) -> Rig {
        Rig {
            core,
            t: UtcMillis::parse_iso(T0).unwrap().0,
            events: Vec::new(),
        }
    }

    fn new() -> Rig {
        Rig::with(AccountConfig::paper("paper-1", 100_000))
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let id = self.core.submit(self.now(), Source::Dom, Instant::now(), c);
        self.collect();
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("every command has a result")
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn market(&mut self, contract: ContractKey, input: MarketInput) {
        self.t += 1;
        self.core.market(self.now(), contract, &[input]);
        self.collect();
    }

    fn quote_on(&mut self, c: ContractKey, bid: Option<i64>, ask: Option<i64>) {
        let time = UtcMillis(self.t + 1);
        self.market(
            c,
            MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: None,
                time,
            },
        );
    }

    fn quote(&mut self, bid: Option<i64>, ask: Option<i64>) {
        self.quote_on(call(), bid, ask);
    }

    /// A trade of SPY, in dollars and cents as a whole number of cents.
    fn print(&mut self, cents: i64) {
        let time = UtcMillis(self.t + 1);
        self.market(
            spy(),
            MarketInput::Trade {
                price: Price(cents),
                qty: Qty(1),
                time,
            },
        );
    }

    fn arm(&mut self, contract: ContractKey, kind: TriggerKind, level: i64) -> TriggerId {
        self.try_arm(contract, kind, level).expect("armed");
        self.core
            .triggers()
            .filter(|t| t.contract == contract)
            .map(|t| t.id)
            .max()
            .expect("it's there")
    }

    fn try_arm(
        &mut self,
        contract: ContractKey,
        kind: TriggerKind,
        level: i64,
    ) -> Result<(), lines_trading::Reject> {
        self.cmd(Command::ArmTrigger(TriggerSpec {
            account: acct(),
            contract,
            kind,
            level: Price(level),
        }))
        .map(|_| ())
    }

    fn open(&mut self, contract: ContractKey, side: Side, qty: i64, cents: i64) {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract,
            side,
            qty: Qty(qty),
            kind: OrderKind::Limit {
                price: Price(cents),
            },
        }))
        .expect("accepted");
        self.wait(LAT + 1);
    }

    /// A position of `qty` on the call, opened at 2.42, with the underlying at 683.00.
    fn long_call(qty: i64) -> Rig {
        let mut r = Rig::new();
        r.print(68_300);
        r.quote(Some(240), Some(242));
        r.open(call(), Side::Buy, qty, 242);
        assert_eq!(r.position(call()), qty);
        r
    }

    fn position(&self, c: ContractKey) -> i64 {
        self.core.position(&acct(), c)
    }

    fn view(&self, id: TriggerId) -> Option<lines_trading::trigger::TriggerView> {
        self.events.iter().rev().find_map(|e| match e {
            Event::Trigger(t) if t.id == id => Some(t.clone()),
            _ => None,
        })
    }

    fn state(&self, id: TriggerId) -> TriggerState {
        self.view(id).expect("reported").state
    }

    fn reason(&self, id: TriggerId) -> String {
        self.view(id).expect("reported").reason.unwrap_or_default()
    }

    fn note(&self, id: TriggerId) -> Option<String> {
        self.view(id).expect("reported").note
    }

    fn orders(&self, contract: ContractKey) -> Vec<Order> {
        let mut last = std::collections::BTreeMap::new();
        for e in &self.events {
            if let Event::Order(o) = e
                && o.contract == contract
            {
                last.insert(o.id, o.clone());
            }
        }
        last.into_values().collect()
    }

    fn fills(&self, contract: ContractKey) -> Vec<(i64, i64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Fill(f) if f.contract == contract => Some((f.price.0, f.qty.0)),
                _ => None,
            })
            .collect()
    }
}

fn entry(qty: i64, max: i64) -> TriggerKind {
    TriggerKind::Entry {
        qty: Qty(qty),
        max_price: Price(max),
    }
}

/// An entry fires as the underlying reaches its level, from the side it was armed on, with a
/// limit at the ask.
#[test]
fn an_entry_fires_with_a_limit_at_the_ask() {
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let id = r.arm(call(), entry(2, 300), 68_350);
    assert_eq!(r.view(id).unwrap().direction, Direction::AtOrAbove);
    r.print(68_349);
    assert_eq!(r.state(id), TriggerState::Armed);
    assert!(r.orders(call()).is_empty(), "not yet");
    r.print(68_350);
    assert_eq!(r.state(id), TriggerState::Ended);
    assert!(r.reason(id).starts_with("fired"), "{}", r.reason(id));
    let o = r.orders(call());
    assert_eq!(o.len(), 1);
    assert_eq!((o[0].side, o[0].qty), (Side::Buy, Qty(2)));
    assert_eq!(o[0].kind, OrderKind::Limit { price: Price(242) });
    assert!(!o[0].reduce_only);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 2);
    assert_eq!(r.fills(call()), [(242, 2)]);

    // From above it fires going down, on a put as on a call: an entry follows the level.
    let id = r.arm(put(), entry(1, 300), 68_300);
    assert_eq!(r.view(id).unwrap().direction, Direction::AtOrBelow);
    r.quote_on(put(), Some(100), Some(102));
    r.print(68_301);
    assert_eq!(r.state(id), TriggerState::Armed);
    r.print(68_299);
    assert!(r.reason(id).starts_with("fired"));
}

/// Above the cap it's refused with its reason and ends: it doesn't wait or chase.
#[test]
fn an_entry_above_its_cap_ends_with_its_reason() {
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(300), Some(305));
    let id = r.arm(call(), entry(1, 301), 68_350);
    r.print(68_350);
    assert_eq!(r.state(id), TriggerState::Ended);
    assert_eq!(r.reason(id), "the ask 3.05 is above your max 3.01");
    assert!(r.orders(call()).is_empty());
    // The ask comes back under the cap: nothing, it's over.
    r.quote(Some(240), Some(242));
    r.print(68_351);
    r.wait(LAT + 1);
    assert!(r.orders(call()).is_empty());
    assert_eq!(r.position(call()), 0);
    // At the cap exactly it goes.
    let id = r.arm(call(), entry(1, 242), 68_400);
    r.print(68_400);
    assert!(r.reason(id).starts_with("fired"));
    // No ask at all.
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), None);
    let id = r.arm(call(), entry(1, 300), 68_350);
    r.print(68_350);
    assert_eq!(r.reason(id), "no ask to buy at");
}

/// All new-order checks run when it fires, not only when armed.
#[test]
fn an_entrys_checks_run_when_it_fires() {
    // The far-from-mid guard: the ask is too far from a wide quote's mid.
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let id = r.arm(call(), entry(1, 2_000), 68_350);
    r.quote(Some(100), Some(1_000));
    r.print(68_350);
    assert_eq!(r.state(id), TriggerState::Ended);
    assert!(
        r.reason(id).contains("too far from the mid"),
        "{}",
        r.reason(id)
    );
    assert!(r.orders(call()).is_empty());

    // A stale option quote has no mid.
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let id = r.arm(call(), entry(1, 300), 68_350);
    for _ in 0..20 {
        r.wait(60_000);
        r.print(68_300);
    }
    r.print(68_350);
    assert!(r.reason(id).contains("stale"), "{}", r.reason(id));

    // A lockout that began after it was armed.
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let id = r.arm(call(), entry(1, 300), 68_350);
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_position: Some(0),
            ..Default::default()
        },
    })
    .unwrap();
    r.print(68_350);
    assert_eq!(r.state(id), TriggerState::Ended);
    assert!(!r.reason(id).starts_with("fired"), "{}", r.reason(id));
    assert!(r.orders(call()).is_empty());
}

/// A premarket or after-hours print never fires one; the market opening past a level is a gap.
#[test]
fn only_regular_hours_fire() {
    let mut r = Rig::new();
    // Monday 09:00 ET.
    r.t = UtcMillis::parse_iso("2026-11-09T14:00:00Z").unwrap().0;
    r.print(68_300);
    r.quote(Some(240), Some(242));
    r.open(call(), Side::Buy, 1, 242);
    assert_eq!(r.position(call()), 0, "the option's market is closed");
    let entry_id = r.arm(call(), entry(1, 300), 68_350);
    r.print(68_400);
    assert_eq!(r.state(entry_id), TriggerState::Armed, "premarket: no");
    // The first print of the session, already past it: the entry ends, it doesn't chase.
    r.t = UtcMillis::parse_iso("2026-11-09T14:30:00Z").unwrap().0;
    r.quote(Some(240), Some(242));
    r.print(68_400);
    assert_eq!(r.state(entry_id), TriggerState::Ended);
    assert!(r.reason(entry_id).contains("already past"));
    assert!(r.orders(call()).iter().all(|o| o.qty == Qty(1)));

    // A position's Stop, on a later expiry: after hours nothing; next morning's first print fires
    // it at once.
    let friday = ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 13).unwrap(),
        Price(68_400),
        Right::Call,
    );
    let mut r = Rig::new();
    r.print(68_300);
    r.quote_on(friday, Some(240), Some(242));
    r.open(friday, Side::Buy, 1, 242);
    assert_eq!(r.position(friday), 1);
    let id = r.arm(friday, TriggerKind::Stop, 68_000);
    // Monday 17:00 ET.
    r.t = UtcMillis::parse_iso("2026-11-09T22:00:00Z").unwrap().0;
    r.print(67_000);
    assert_eq!(r.state(id), TriggerState::Armed, "after hours: no");
    // Tuesday's open.
    r.t = UtcMillis::parse_iso("2026-11-10T14:30:00Z").unwrap().0;
    r.quote_on(friday, Some(100), Some(102));
    r.print(67_000);
    assert_eq!(r.state(id), TriggerState::Working);
    r.wait(LAT + 1);
    assert_eq!(r.position(friday), 0);
    assert_eq!(r.fills(friday).last(), Some(&(100, 1)));
}

/// A long put's direction is flipped: its Stop fires as the underlying rises.
#[test]
fn a_long_puts_exits_are_flipped() {
    let mut r = Rig::new();
    r.print(68_300);
    r.quote_on(put(), Some(100), Some(102));
    r.open(put(), Side::Buy, 1, 102);
    let stop = r.arm(put(), TriggerKind::Stop, 68_500);
    assert_eq!(r.view(stop).unwrap().direction, Direction::AtOrAbove);
    let target = r.arm(put(), TriggerKind::Target, 68_000);
    assert_eq!(r.view(target).unwrap().direction, Direction::AtOrBelow);
    r.print(68_100);
    assert_eq!(r.state(stop), TriggerState::Armed, "falling is its gain");
    r.print(68_500);
    assert_eq!(r.state(stop), TriggerState::Working);
    r.wait(LAT + 1);
    assert_eq!(r.position(put()), 0);
    assert_eq!(
        r.fills(put()).last(),
        Some(&(100, 1)),
        "at the option's own bid"
    );
}

/// A fired Stop closes at once with a reduce-only market order at the option's bid, past the
/// far-from-mid guard and with no floor, though market orders stay refused otherwise.
#[test]
fn a_stop_sends_a_reduce_only_market_order() {
    let mut r = Rig::long_call(2);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    // A person's market order on an option stays refused.
    let m = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Market,
    }));
    assert_eq!(m.unwrap_err().code, RejectCode::Risk);
    // A very wide quote, and a bid far under what was paid.
    r.quote(Some(5), Some(900));
    r.print(68_001);
    assert_eq!(r.state(id), TriggerState::Armed);
    r.print(68_000);
    assert_eq!(r.state(id), TriggerState::Working);
    let sell = r
        .orders(call())
        .into_iter()
        .rfind(|o| o.side == Side::Sell)
        .expect("sent at once");
    assert_eq!(sell.kind, OrderKind::Market);
    assert!(sell.reduce_only);
    assert_eq!(sell.qty, Qty(2));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(5, 2)), "no floor");
    assert_eq!(r.state(id), TriggerState::Ended);
    assert_eq!(r.reason(id), "closed");
}

/// With no usable bid a fired Stop keeps waiting, says so, and goes when one comes. A market
/// order is never sent on nothing.
#[test]
fn a_stop_with_no_bid_waits_loudly() {
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    r.quote(None, Some(242));
    r.print(68_000);
    assert_eq!(r.state(id), TriggerState::Working);
    assert_eq!(
        r.note(id).as_deref(),
        Some("no usable bid: waiting to exit")
    );
    assert!(
        r.orders(call()).iter().all(|o| o.side == Side::Buy),
        "nothing sent"
    );
    r.wait(5_000);
    assert_eq!(r.position(call()), 1);
    r.quote(Some(150), Some(242));
    assert_eq!(r.note(id), None, "it says so when it stops waiting");
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(150, 1)));

    // A stale quote is no bid either.
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    for _ in 0..16 {
        r.wait(60_000);
        r.print(68_300);
    }
    r.print(68_000);
    assert_eq!(
        r.note(id).as_deref(),
        Some("no usable bid: waiting to exit")
    );
    assert_eq!(r.position(call()), 1);
}

/// A fired Target limits at the bid and re-prices about every 3 s until filled, however wide the
/// quote; it's reduce-only, and never chases past the position.
#[test]
fn a_target_chases_the_bid() {
    let mut r = Rig::long_call(2);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(300), Some(900));
    r.print(68_500);
    assert_eq!(r.state(id), TriggerState::Working);
    let o = r
        .orders(call())
        .into_iter()
        .rfind(|o| o.side == Side::Sell)
        .unwrap();
    assert_eq!(o.kind, OrderKind::Limit { price: Price(300) });
    assert!(o.reduce_only);
    assert_eq!(o.qty, Qty(2));
    // The bid drops before it arrives: it rests above it.
    r.quote(Some(280), Some(900));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 2);
    // Not re-priced before its time.
    r.wait(CHASE - 2 * LAT - 10);
    let price = |r: &Rig| match r
        .orders(call())
        .iter()
        .rfind(|o| o.side == Side::Sell)
        .unwrap()
        .kind
    {
        OrderKind::Limit { price } => price.0,
        k => panic!("{k:?}"),
    };
    assert_eq!(price(&r), 300);
    // At its time: to the bid, past the guard, arriving after the broker's latency, and it fills
    // there.
    r.wait(LAT + 30);
    r.wait(LAT + 1);
    assert_eq!(price(&r), 280);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(280, 2)));
    assert_eq!(r.reason(id), "closed");
}

/// A Target with no usable bid keeps waiting and says so; its order stays.
#[test]
fn a_target_with_no_bid_waits_loudly() {
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(None, Some(900));
    r.print(68_500);
    assert_eq!(
        r.note(id).as_deref(),
        Some("no usable bid: waiting to exit")
    );
    r.wait(CHASE * 2);
    assert_eq!(r.position(call()), 1);
    r.quote(Some(250), Some(900));
    r.wait(1);
    assert_eq!(r.note(id), None);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(250, 1)));
}

/// The trader cancelling a chasing Target's order, or the trigger, stops the chase; the position
/// going stops it too.
#[test]
fn the_chase_stops_when_told_or_when_the_position_goes() {
    for by_trigger in [false, true] {
        let mut r = Rig::long_call(1);
        let id = r.arm(call(), TriggerKind::Target, 68_500);
        r.quote(Some(300), Some(900));
        r.print(68_500);
        r.quote(Some(280), Some(900));
        r.wait(LAT + 1);
        let order = r
            .orders(call())
            .into_iter()
            .rfind(|o| o.side == Side::Sell)
            .unwrap()
            .id;
        let c = if by_trigger {
            Command::CancelTrigger {
                account: acct(),
                id,
            }
        } else {
            Command::CancelOrder {
                account: acct(),
                order,
            }
        };
        r.cmd(c).unwrap();
        assert_eq!(r.state(id), TriggerState::Ended);
        r.wait(LAT + 1);
        let sells = r
            .orders(call())
            .into_iter()
            .filter(|o| o.side == Side::Sell)
            .count();
        r.wait(CHASE * 3);
        assert_eq!(
            r.orders(call())
                .into_iter()
                .filter(|o| o.side == Side::Sell)
                .count(),
            sells,
            "nothing new"
        );
        assert_eq!(r.position(call()), 1, "the position is the trader's now");
        assert!(
            !r.orders(call())
                .iter()
                .any(|o| o.side == Side::Sell && o.is_open())
        );
    }
    // A person's Flatten closes it and ends everything on the series.
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(300), Some(900));
    r.print(68_500);
    r.quote(Some(280), Some(900));
    r.cmd(Command::Flatten {
        account: acct(),
        contract: Some(call()),
    })
    .unwrap();
    assert_eq!(r.state(id), TriggerState::Ended);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
}

/// Stop and Target are one-cancels-other, and one print can't fire both: levels on the same or
/// the wrong side are refused, and in a batch reaching both the first print decides.
#[test]
fn stop_and_target_cancel_each_other() {
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    // Refused: at the Stop, or past it.
    for level in [68_000, 67_900] {
        let e = r.try_arm(call(), TriggerKind::Target, level).unwrap_err();
        assert_eq!(e.code, RejectCode::Invalid, "{level}");
    }
    let target = r.arm(call(), TriggerKind::Target, 68_500);
    // A second Stop replaces the first: one of each per position.
    let e = r.try_arm(call(), TriggerKind::Stop, 68_600).unwrap_err();
    assert_eq!(e.code, RejectCode::Invalid, "past the Target");
    let stop2 = r.arm(call(), TriggerKind::Stop, 68_100);
    assert_eq!(r.reason(stop), "replaced");

    // One batch through both levels: the first print fires, the other is over.
    r.t += 1;
    let at = r.now();
    let prints = [68_100, 68_500].map(|p| MarketInput::Trade {
        price: Price(p),
        qty: Qty(1),
        time: at,
    });
    r.core.market(at, spy(), &prints);
    r.collect();
    assert_eq!(r.state(stop2), TriggerState::Working);
    assert_eq!(r.state(target), TriggerState::Ended);
    assert_eq!(r.reason(target), "the Stop fired");
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    let sold: i64 = r.fills(call()).iter().skip(1).map(|f| f.1).sum();
    assert_eq!(sold, 1, "closed once");
}

/// Exits close what the position is when they fire, and can never open, grow or flip it.
#[test]
fn exits_shrink_with_the_position_and_never_flip() {
    // It shrank before the Stop fired: only what's left is sold.
    let mut r = Rig::long_call(3);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    r.cmd(Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(2),
        kind: OrderKind::Limit { price: Price(240) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 1);
    r.print(68_000);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0, "flat, not short");
    assert_eq!(r.fills(call()).last(), Some(&(240, 1)));
    assert_eq!(r.state(id), TriggerState::Ended);

    // A working plain sell doesn't take the exit's place, nor outlive the position to open one.
    let mut r = Rig::long_call(2);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(2),
        kind: OrderKind::Limit { price: Price(250) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    r.print(68_500);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    r.quote(Some(300), Some(302));
    r.wait(CHASE);
    assert_eq!(r.position(call()), 0, "nothing left to open a short");
    assert_eq!(r.state(id), TriggerState::Ended);

    // The position goes by other means: its exits end, and a later print does nothing.
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    let target = r.arm(call(), TriggerKind::Target, 68_500);
    r.cmd(Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(240) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.reason(stop), "the position closed");
    assert_eq!(r.reason(target), "the position closed");
    let n = r.orders(call()).len();
    r.print(68_000);
    r.wait(LAT + 1);
    assert_eq!(r.orders(call()).len(), n);
    assert_eq!(r.position(call()), 0);

    // It flips: an exit armed for the long side must not fire for the short one.
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    r.open(call(), Side::Sell, 2, 240);
    assert_eq!(r.position(call()), -1);
    assert_eq!(r.reason(stop), "the position flipped");

    // No position: nothing to arm.
    let mut r = Rig::new();
    r.print(68_300);
    let e = r.try_arm(call(), TriggerKind::Stop, 68_000).unwrap_err();
    assert_eq!(e.code, RejectCode::Invalid);
}

/// A fired exit ends if the position flips: it never buys back a short it wasn't armed for.
#[test]
fn a_fired_exit_ends_when_the_position_flips() {
    let mut r = Rig::long_call(2);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(300), Some(900));
    r.print(68_500);
    r.quote(Some(280), Some(900));
    r.wait(LAT + 1);
    assert_eq!(r.state(id), TriggerState::Working);
    // The trader sells through flat at the bid, on a narrower quote the guard allows.
    r.quote(Some(280), Some(285));
    r.open(call(), Side::Sell, 5, 280);
    assert_eq!(r.position(call()), -3);
    assert_eq!(r.state(id), TriggerState::Ended);
    assert_eq!(r.reason(id), "the position flipped");
    r.wait(CHASE * 3);
    r.quote(Some(280), Some(285));
    r.wait(CHASE);
    assert_eq!(r.position(call()), -3, "the short is the trader's");
    assert!(
        !r.orders(call())
            .iter()
            .any(|o| o.side == Side::Buy && o.is_open()),
        "nothing buys it back"
    );
}

/// Risk limits block entries, never exits: a daily loss, a size limit or a price band can't trap
/// a fired exit, and a real lockout (trades used up) stops an armed entry when it fires.
#[test]
fn risk_limits_never_trap_a_fired_exit() {
    use lines_trading::RiskLimits;
    let build = |limits: RiskLimits| {
        let mut a = AccountConfig::paper("paper-1", 100_000);
        a.limits = limits;
        let mut r = Rig::with(a);
        r.print(68_300);
        r.quote(Some(240), Some(242));
        r
    };
    // Bought in two lots of 2 under a limit of 2: a Target for all 4 is over it.
    let mut r = build(RiskLimits {
        max_order_qty: Some(2),
        ..Default::default()
    });
    r.open(call(), Side::Buy, 2, 242);
    r.open(call(), Side::Buy, 2, 242);
    assert_eq!(r.position(call()), 4);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(300), Some(302));
    r.print(68_500);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);

    // A price band tighter than a wide quote: the bid and its re-price stay allowed.
    let mut r = build(RiskLimits {
        price_band_ticks: Some(2),
        ..Default::default()
    });
    r.open(call(), Side::Buy, 1, 242);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(100), Some(900));
    r.print(68_500);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    r.quote(Some(90), Some(900));
    r.wait(LAT + 1);
    r.wait(CHASE);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(90, 1)));

    // A daily loss already over its limit: a person's new order is refused, the Target isn't.
    let mut r = build(RiskLimits {
        daily_loss: Some(10),
        ..Default::default()
    });
    r.open(call(), Side::Buy, 1, 242);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(100), Some(102));
    let refused = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(102) },
    }));
    assert_eq!(refused.unwrap_err().code, RejectCode::Risk);
    r.print(68_500);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);

    // The rate limits: a person's modify is refused, the chase's re-price isn't, and a submit a
    // moment after a person's still goes.
    let mut r = build(RiskLimits {
        max_modifies_per_sec: Some(0),
        max_submits_per_sec: Some(1),
        ..Default::default()
    });
    r.open(call(), Side::Buy, 1, 242);
    r.wait(2_000);
    let id = r.arm(call(), TriggerKind::Target, 68_500);
    let plain = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: call(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit { price: Price(245) },
        }))
        .unwrap()[0];
    let refused = r.cmd(Command::ModifyOrder {
        account: acct(),
        order: plain,
        qty: None,
        price: Some(Price(246)),
    });
    assert_eq!(refused.unwrap_err().code, RejectCode::Risk);
    r.quote(Some(240), Some(242));
    r.print(68_500);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    r.quote(Some(230), Some(232));
    r.wait(LAT + 1);
    r.wait(CHASE);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);

    // A real lockout: the day's trades used up by a round trip on another series. The entry
    // armed before it ends when it fires, with the lockout's reason; the exit goes.
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    r.quote_on(put(), Some(100), Some(102));
    let later = r.arm(put(), entry(1, 300), 68_400);
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_trades_per_day: Some(1),
            ..Default::default()
        },
    })
    .unwrap();
    let other = series(690, Right::Call);
    r.quote_on(other, Some(50), Some(52));
    r.open(other, Side::Buy, 1, 52);
    r.open(other, Side::Sell, 1, 50);
    assert_eq!(r.position(other), 0);
    r.print(68_400);
    assert_eq!(r.state(later), TriggerState::Ended);
    assert!(!r.reason(later).starts_with("fired"), "{}", r.reason(later));
    assert!(r.orders(put()).is_empty());
    r.print(68_000);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.state(stop), TriggerState::Ended);
}

/// A backlog of old prints isn't the feed coming back: the next print past a level is still a
/// gap. And which way an entry fires comes from the newest print, not a late one.
#[test]
fn old_and_late_prints_prove_nothing() {
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let id = r.arm(call(), entry(1, 300), 68_350);
    let start = r.t;
    r.wait(STALE * 6);
    // Stamped 11 s ago: skipped, and it mustn't count as data.
    let old = UtcMillis(r.t - STALE - 1);
    r.market(
        spy(),
        MarketInput::Trade {
            price: Price(68_300),
            qty: Qty(1),
            time: old,
        },
    );
    // 5 s old and past the level: 6 s after the skipped print, but the feed never came back.
    let recent = UtcMillis(r.t - 5_000);
    r.market(
        spy(),
        MarketInput::Trade {
            price: Price(68_350),
            qty: Qty(1),
            time: recent,
        },
    );
    assert_eq!(r.state(id), TriggerState::Ended);
    assert!(r.reason(id).contains("already past"), "{}", r.reason(id));
    assert!(r.orders(call()).is_empty());
    assert!(r.t > start);

    let mut r = Rig::new();
    r.print(68_400);
    r.quote(Some(240), Some(242));
    let late = UtcMillis(r.t - 500);
    r.market(
        spy(),
        MarketInput::Trade {
            price: Price(68_200),
            qty: Qty(1),
            time: late,
        },
    );
    let id = r.arm(call(), entry(1, 300), 68_300);
    assert_eq!(
        r.view(id).unwrap().direction,
        Direction::AtOrBelow,
        "from 684.00, not the late 682.00"
    );
    r.print(68_400);
    assert_eq!(r.state(id), TriggerState::Armed, "it didn't fire at once");
}

/// Once an exit is closing the position nothing more can be armed on it, and a person cancelling
/// a fired Stop's market order, or the Stop, stops it like a Target.
#[test]
fn a_fired_exit_stays_in_charge_until_told() {
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    r.quote(None, Some(900));
    r.print(68_000);
    assert_eq!(r.state(id), TriggerState::Working);
    for kind in [TriggerKind::Stop, TriggerKind::Target, entry(1, 300)] {
        let e = r.try_arm(call(), kind, 69_000).unwrap_err();
        assert!(e.message.contains("already closing"), "{}", e.message);
    }
    // Cancelling the waiting Stop: nothing goes when a bid comes.
    r.cmd(Command::CancelTrigger {
        account: acct(),
        id,
    })
    .unwrap();
    r.quote(Some(240), Some(242));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 1);

    // Its market order on its way, cancelled by the person: it isn't sent again.
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    r.print(68_000);
    let order = r
        .orders(call())
        .into_iter()
        .rfind(|o| o.side == Side::Sell)
        .unwrap()
        .id;
    r.cmd(Command::CancelOrder {
        account: acct(),
        order,
    })
    .unwrap();
    assert_eq!(r.state(id), TriggerState::Ended);
    assert_eq!(r.reason(id), "you canceled its order");
}

/// A fired exit ends the series' armed entry: it mustn't reopen what's being closed.
#[test]
fn a_fired_exit_ends_the_entry_on_its_series() {
    let mut r = Rig::long_call(2);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    let again = r.arm(call(), entry(1, 300), 68_000);
    let other = r.arm(put(), entry(1, 300), 68_000);
    r.quote_on(put(), Some(100), Some(102));
    r.print(68_000);
    assert_eq!(r.state(stop), TriggerState::Working);
    assert_eq!(r.state(again), TriggerState::Ended);
    assert_eq!(r.reason(again), "the Stop fired");
    assert!(
        r.reason(other).starts_with("fired"),
        "another series is its own"
    );
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0, "flat, not long 1");
}

/// A strategy's contract takes no trigger, and a strategy starting on one ends it unfired: a fill
/// under the strategy would be missing from its book.
#[test]
fn no_triggers_under_a_strategy() {
    use lines_trading::strategy::{
        HoursWindow, StrategyConfig, StrategyId, StrategyLimits, TradingHours,
    };
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    let id = StrategyId(7);
    r.cmd(Command::AddStrategy(StrategyConfig {
        id,
        label: "example".into(),
        account: acct(),
        contract: call(),
        limits: StrategyLimits {
            max_position: 1,
            max_daily_loss: 1_000,
            max_orders_per_min: 5,
            hours: TradingHours {
                tz: "America/New_York".into(),
                windows: vec![HoursWindow::new("09:30", "16:00")],
            },
        },
    }))
    .unwrap();
    let e = r.try_arm(call(), TriggerKind::Target, 68_500).unwrap_err();
    assert_eq!(e.code, RejectCode::Strategy);
    // At once, not when it would have fired: the trader knows it's unprotected.
    assert_eq!(r.state(stop), TriggerState::Ended);
    assert_eq!(r.reason(stop), "a strategy trades this option now");
    r.print(68_000);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 1, "nothing sold under it");
}

/// A Stop the broker couldn't fill on a quote says so, and tries on the next quote.
#[test]
fn a_stop_the_broker_cannot_fill_says_so() {
    let (mut r, s) = scripted(true);
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let buy = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: call(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit { price: Price(242) },
        }))
        .unwrap()[0];
    r.wait(1);
    fill(&mut r, &s, buy, 242, 1);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    r.print(68_000);
    r.wait(1);
    let first = s.lock().unwrap().placed.last().unwrap().id;
    s.lock().unwrap().out.push(BrokerEvent::Cancelled {
        id: first,
        reason: "no liquidity".into(),
        at: UtcMillis(r.t),
    });
    r.wait(1);
    assert_eq!(
        r.note(id).as_deref(),
        Some("the bid couldn't fill it: waiting for a new quote")
    );
    r.quote(Some(230), Some(242));
    r.wait(1);
    assert_eq!(r.note(id), None);
}

/// A short position's Target chases the ask: at its own side, past the guard, however wide.
#[test]
fn a_short_positions_target_chases_the_ask() {
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    r.open(call(), Side::Sell, 2, 240);
    assert_eq!(r.position(call()), -2);
    let id = r.arm(call(), TriggerKind::Target, 68_100);
    assert_eq!(r.view(id).unwrap().direction, Direction::AtOrBelow);
    r.quote(Some(10), Some(200));
    r.print(68_100);
    assert_eq!(r.state(id), TriggerState::Working);
    assert_eq!(r.note(id), None, "{:?}", r.note(id));
    let o = r
        .orders(call())
        .into_iter()
        .rfind(|o| o.side == Side::Buy)
        .unwrap();
    assert_eq!(
        (o.kind, o.qty),
        (OrderKind::Limit { price: Price(200) }, Qty(2))
    );
    assert!(o.reduce_only);
    r.quote(Some(10), Some(210));
    r.wait(LAT + 1);
    r.wait(CHASE);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(210, 2)));
}

/// Prop rules set after an entry was armed refuse it when it fires.
#[test]
fn an_entry_armed_before_prop_rules_is_refused_when_it_fires() {
    let mut r = Rig::with(AccountConfig::paper("paper-1", 50_000));
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let id = r.arm(call(), entry(1, 300), 68_350);
    r.cmd(Command::SetRules {
        account: acct(),
        rules: presets().into_iter().nth(1),
    })
    .unwrap();
    r.print(68_350);
    assert_eq!(r.reason(id), lines_trading::risk::PROP_OPTIONS_REFUSAL);
    assert!(r.orders(call()).is_empty());
}

/// A short position's exits buy back: its Stop lifts the ask.
#[test]
fn a_short_positions_exits_buy_back() {
    let mut r = Rig::new();
    r.print(68_300);
    r.quote(Some(240), Some(242));
    r.open(call(), Side::Sell, 1, 240);
    assert_eq!(r.position(call()), -1);
    let id = r.arm(call(), TriggerKind::Stop, 68_500);
    assert_eq!(r.view(id).unwrap().direction, Direction::AtOrAbove);
    r.quote(Some(260), None);
    r.print(68_500);
    assert_eq!(
        r.note(id).as_deref(),
        Some("no usable ask: waiting to exit")
    );
    r.quote(Some(260), Some(900));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(900, 1)), "no ceiling");
}

/// A stale underlying never fires one, and says so. When data returns past a level, an exit
/// fires at once and an entry ends with its reason. An old print says nothing about now.
#[test]
fn a_stale_feed_never_fires() {
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    let entry_id = r.arm(put(), entry(1, 300), 68_200);
    assert!(!r.view(stop).unwrap().stale);
    r.wait(STALE + 2);
    assert!(r.view(stop).unwrap().stale);
    assert!(r.view(entry_id).unwrap().stale);
    assert_eq!(r.state(stop), TriggerState::Armed);
    // An old print, even past both levels: nothing.
    let old = UtcMillis(r.t - STALE - 1);
    r.market(
        spy(),
        MarketInput::Trade {
            price: Price(67_000),
            qty: Qty(1),
            time: old,
        },
    );
    assert_eq!(r.state(stop), TriggerState::Armed);
    assert_eq!(r.state(entry_id), TriggerState::Armed);
    // The feed returns past both, the quote still good.
    r.quote(Some(150), Some(152));
    r.print(67_000);
    assert_eq!(
        r.state(stop),
        TriggerState::Working,
        "an exit fires at once"
    );
    assert_eq!(r.state(entry_id), TriggerState::Ended, "an entry ends");
    assert!(r.reason(entry_id).contains("already past"));
    assert!(r.orders(put()).is_empty());
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);

    // Returning short of the level: still armed, no longer stale.
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    r.wait(STALE + 2);
    assert!(r.view(stop).unwrap().stale);
    r.print(68_100);
    r.wait(1);
    assert!(!r.view(stop).unwrap().stale);
    assert_eq!(r.state(stop), TriggerState::Armed);

    // An entry can't be armed on a stale underlying: it couldn't tell which way it fires.
    let mut r = Rig::new();
    r.print(68_300);
    r.wait(STALE + 2);
    let e = r.try_arm(call(), entry(1, 300), 68_350).unwrap_err();
    assert_eq!(e.code, RejectCode::Risk);
    // A Stop can: it's the protection, and its direction comes from the position.
    let mut r = Rig::long_call(1);
    r.wait(STALE + 2);
    r.try_arm(call(), TriggerKind::Stop, 68_000).unwrap();
}

fn restart(r: &Rig, paper: &PaperConfig) -> Rig {
    let file = lines_trading::service::trading_file(&r.core, paper).expect("paper");
    let dir = std::env::temp_dir().join(format!(
        "lines-triggers-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let path = dir.join("paper.json");
    persist::save(&path, &file).unwrap();
    let f: TradingFile = persist::load(&path).unwrap().expect("saved");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(f.version, persist::VERSION);
    let mut core = TradingCore::new(Box::new(PaperBroker::from_state(paper.clone(), f.broker)));
    for a in f.accounts {
        core.add_account(a.config, Some(a.tracker));
    }
    core.set_next_order_id(f.next_order_id);
    core.restore_triggers(f.triggers);
    assert_eq!(
        core.triggers().count(),
        0,
        "not before the positions are back"
    );
    core.rebuild(UtcMillis(r.t));
    let mut n = Rig::on(core);
    n.t = r.t;
    n.collect();
    n
}

/// After a restart entries don't return. Exits on a still-open position return armed, with a
/// notice, and fire at once on a first print past their level.
#[test]
fn a_restart_keeps_exits_and_drops_entries() {
    let paper = PaperConfig {
        latency_ms: LAT,
        ..Default::default()
    };
    let mut r = Rig::long_call(2);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    let target = r.arm(call(), TriggerKind::Target, 68_500);
    r.arm(put(), entry(1, 300), 68_200);
    let mut n = restart(&r, &paper);
    assert_eq!(n.core.triggers().count(), 2, "the entry is gone");
    for id in [stop, target] {
        let v = n.view(id).expect("reported on restoring");
        assert!(v.restored, "with a notice");
        assert_eq!(v.state, TriggerState::Armed);
    }
    assert_eq!(n.position(call()), 2);
    // A new trigger doesn't take a restored id.
    n.print(68_300);
    n.quote(Some(240), Some(242));
    let again = n.arm(call(), TriggerKind::Stop, 68_100);
    assert!(again > target);
    assert_eq!(n.reason(stop), "replaced");
    // The first print is past the level: it fires at once.
    let mut n = restart(&r, &paper);
    n.quote(Some(150), Some(152));
    n.print(67_900);
    assert_eq!(n.state(stop), TriggerState::Working);
    assert_eq!(n.state(target), TriggerState::Ended);
    n.wait(LAT + 1);
    assert_eq!(n.position(call()), 0);
    assert_eq!(n.fills(call()).last(), Some(&(150, 2)));

    // The position is gone: nothing comes back.
    let mut r = Rig::long_call(1);
    r.arm(call(), TriggerKind::Stop, 68_000);
    let saved = r.core.saved_triggers();
    assert_eq!(saved.len(), 1);
    r.cmd(Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(240) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    r.core.restore_triggers(saved);
    r.core.rebuild(r.now());
    assert_eq!(r.core.triggers().count(), 0);
}

/// The saved format: v4, with a tested migration, and an older file reads with no triggers.
#[test]
fn the_trading_file_moves_to_v4() {
    assert_eq!(persist::VERSION, 4);
    let v3 = serde_json::json!({
        "version": 3,
        "paper": PaperConfig::default(),
        "broker": { "accounts": {}, "orders": [] },
        "accounts": [],
        "next_order_id": 1,
    });
    let v4 = persist::migrate(v3).unwrap();
    assert_eq!(v4["version"], 4);
    let f: TradingFile = serde_json::from_value(v4).unwrap();
    assert!(f.triggers.is_empty());
    // Damaged: no accounts.
    assert!(persist::migrate(serde_json::json!({"version": 3})).is_err());
    // Newer: refused.
    assert!(persist::migrate(serde_json::json!({"version": 5})).is_err());
}

/// Lockouts block entries, never exits.
#[test]
fn a_lockout_blocks_entries_and_never_exits() {
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_position: Some(0),
            ..Default::default()
        },
    })
    .unwrap();
    // New entries are refused where they'd be refused anyway.
    let e = r.try_arm(put(), entry(1, 300), 68_200).unwrap_err();
    assert_eq!(e.code, RejectCode::Guardrail);
    r.print(68_000);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0, "the exit went");
    assert_eq!(r.state(stop), TriggerState::Ended);

    // A Target too.
    let mut r = Rig::long_call(1);
    r.arm(call(), TriggerKind::Target, 68_500);
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_position: Some(0),
            ..Default::default()
        },
    })
    .unwrap();
    r.print(68_500);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
}

/// Prop-rule accounts stay refused.
#[test]
fn prop_accounts_arm_nothing() {
    let mut a = AccountConfig::paper("paper-1", 50_000);
    a.rules = presets().into_iter().nth(1);
    let mut r = Rig::with(a);
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let e = r.try_arm(call(), entry(1, 300), 68_350).unwrap_err();
    assert_eq!(e.code, RejectCode::PropRule);
    let e = r.try_arm(call(), TriggerKind::Stop, 68_000).unwrap_err();
    assert_eq!(e.code, RejectCode::PropRule);
}

/// What a script does to a broker, for the tests paper can't give: a partial fill; a venue.
#[derive(Default)]
struct Script {
    out: Vec<BrokerEvent>,
    placed: Vec<Order>,
    modified: Vec<(OrderId, Option<Price>)>,
    simulated: bool,
    next_trade: u64,
}

struct ScriptBroker(Arc<Mutex<Script>>);

impl Broker for ScriptBroker {
    fn name(&self) -> &str {
        "script"
    }
    fn capabilities(&self) -> BrokerCaps {
        PaperBroker::new(PaperConfig::default()).capabilities()
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        let mut s = self.0.lock().unwrap();
        match req {
            BrokerRequest::Place(o) => {
                s.out.push(BrokerEvent::Acked { id: o.id, at: now });
                s.out.push(BrokerEvent::Working { id: o.id, at: now });
                s.placed.push(o);
            }
            BrokerRequest::Modify { id, price, .. } => {
                s.modified.push((id, price));
                let found = s.placed.iter().position(|o| o.id == id);
                if let (Some(i), Some(p)) = (found, price) {
                    s.placed[i].kind = OrderKind::Limit { price: p };
                    let order = s.placed[i].clone();
                    s.out.push(BrokerEvent::Modified { order });
                }
            }
            BrokerRequest::Cancel { id } => {
                s.out.push(BrokerEvent::Cancelled {
                    id,
                    reason: String::new(),
                    at: now,
                });
            }
            _ => {}
        }
    }
    fn market(&mut self, _: UtcMillis, _: ContractKey, _: &MarketInput) {}
    fn poll(&mut self, _: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.append(&mut self.0.lock().unwrap().out);
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot::default()
    }
    fn simulated(&self) -> bool {
        self.0.lock().unwrap().simulated
    }
}

fn scripted(simulated: bool) -> (Rig, Arc<Mutex<Script>>) {
    let s = Arc::new(Mutex::new(Script {
        simulated,
        ..Script::default()
    }));
    let mut core = TradingCore::new(Box::new(ScriptBroker(s.clone())));
    core.add_account(AccountConfig::paper("paper-1", 100_000), None);
    (Rig::on(core), s)
}

fn fill(r: &mut Rig, s: &Arc<Mutex<Script>>, id: OrderId, price: i64, qty: i64) {
    {
        let mut g = s.lock().unwrap();
        g.next_trade += 1;
        let trade_id = format!("t{}", g.next_trade);
        g.out.push(BrokerEvent::Fill {
            id,
            trade_id,
            price: Price(price),
            qty: Qty(qty),
            fee: lines_trading::Money::ZERO,
            at: UtcMillis(r.t),
        });
    }
    r.wait(1);
}

/// On a partial fill the chase goes on for what's left: the same order, re-priced, never re-sent
/// for what filled, and it ends when the position is flat.
#[test]
fn a_partial_fill_keeps_chasing_the_rest() {
    let (mut r, s) = scripted(true);
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let buy = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: call(),
            side: Side::Buy,
            qty: Qty(5),
            kind: OrderKind::Limit { price: Price(242) },
        }))
        .unwrap()[0];
    r.wait(1);
    fill(&mut r, &s, buy, 242, 5);
    assert_eq!(r.position(call()), 5);

    let id = r.arm(call(), TriggerKind::Target, 68_500);
    r.quote(Some(300), Some(900));
    r.print(68_500);
    r.wait(1);
    let sell = s.lock().unwrap().placed.last().unwrap().clone();
    assert_eq!((sell.side, sell.qty), (Side::Sell, Qty(5)));
    assert!(sell.reduce_only);

    fill(&mut r, &s, sell.id, 300, 2);
    assert_eq!(r.position(call()), 3);
    assert_eq!(r.state(id), TriggerState::Working, "the rest still chases");
    r.quote(Some(280), Some(900));
    r.wait(CHASE);
    {
        let g = s.lock().unwrap();
        assert_eq!(g.placed.len(), 2, "the same order, not a second one");
        assert_eq!(g.modified.last(), Some(&(sell.id, Some(Price(280)))));
    }
    fill(&mut r, &s, sell.id, 280, 3);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.reason(id), "closed");
    r.wait(CHASE * 2);
    assert_eq!(s.lock().unwrap().placed.len(), 2, "over");
}

/// A Stop's market order partly filled and then cancelled: the rest goes on the next quote, for
/// the rest only.
#[test]
fn a_partial_stop_sends_only_the_rest() {
    let (mut r, s) = scripted(true);
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let buy = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: call(),
            side: Side::Buy,
            qty: Qty(5),
            kind: OrderKind::Limit { price: Price(242) },
        }))
        .unwrap()[0];
    r.wait(1);
    fill(&mut r, &s, buy, 242, 5);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    r.print(68_000);
    r.wait(1);
    let first = s.lock().unwrap().placed.last().unwrap().clone();
    assert_eq!((first.kind, first.qty), (OrderKind::Market, Qty(5)));
    fill(&mut r, &s, first.id, 240, 2);
    s.lock().unwrap().out.push(BrokerEvent::Cancelled {
        id: first.id,
        reason: "no more size".into(),
        at: UtcMillis(r.t),
    });
    r.wait(1);
    assert_eq!(r.position(call()), 3);
    // Not again on the same quote.
    r.wait(1_000);
    assert_eq!(s.lock().unwrap().placed.len(), 2);
    r.quote(Some(230), Some(242));
    r.wait(1);
    let second = s.lock().unwrap().placed.last().unwrap().clone();
    assert_ne!(second.id, first.id);
    assert_eq!((second.kind, second.qty), (OrderKind::Market, Qty(3)));
    fill(&mut r, &s, second.id, 230, 3);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.state(id), TriggerState::Ended);
}

/// Venue accounts stay paper-only.
#[test]
fn a_venue_account_arms_nothing() {
    let (mut r, _) = scripted(false);
    r.print(68_300);
    r.quote(Some(240), Some(242));
    let e = r.try_arm(call(), entry(1, 300), 68_350).unwrap_err();
    assert_eq!(e.code, RejectCode::Broker);
    assert!(e.message.contains("paper only"), "{}", e.message);
    let e = r.try_arm(call(), TriggerKind::Stop, 68_000).unwrap_err();
    assert_eq!(e.code, RejectCode::Broker);
}

/// Closes the known gap: a reduce-only limit at its own side of the quote passes however wide,
/// with no other side needed. Nothing else gets past the guard.
#[test]
fn an_exit_at_its_own_bid_passes_the_guard() {
    let mut r = Rig::long_call(1);
    r.quote(Some(5), Some(900));
    let exit = |r: &mut Rig, price: i64| {
        r.cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: call(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(price),
            },
        }))
    };
    let e = exit(&mut r, 6).unwrap_err();
    assert_eq!(
        e.code,
        RejectCode::Risk,
        "a tick off the bid is still a limit like any"
    );
    // Moving a resting exit: to its bid it may; elsewhere it's the guard's.
    let id = exit(&mut r, 450).unwrap()[0];
    r.wait(LAT + 1);
    let moved = r.cmd(Command::ModifyOrder {
        account: acct(),
        order: id,
        qty: None,
        price: Some(Price(100)),
    });
    assert_eq!(moved.unwrap_err().code, RejectCode::Risk);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: id,
    })
    .unwrap();
    r.wait(LAT + 1);
    // No ask at all: a bid is enough.
    r.quote(Some(5), None);
    exit(&mut r, 5).expect("at its own bid");
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(5, 1)));
    // An opening order gets no exemption, at the bid or not.
    let opening = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(5) },
    }));
    assert_eq!(opening.unwrap_err().code, RejectCode::Risk);
    // A stale bid is no exemption either.
    let mut r = Rig::long_call(1);
    r.quote(Some(5), Some(900));
    r.wait(16 * 60_000);
    let e = exit(&mut r, 5).unwrap_err();
    assert_eq!(e.code, RejectCode::Risk);
}

/// An expired series' trigger ends.
#[test]
fn a_trigger_ends_with_its_expiry() {
    let mut r = Rig::long_call(1);
    let id = r.arm(call(), TriggerKind::Stop, 68_000);
    let close = UtcMillis::parse_iso("2026-11-09T21:00:00Z").unwrap();
    r.t = close.0;
    r.wait(1);
    assert_eq!(r.state(id), TriggerState::Ended);
    assert_eq!(r.reason(id), "the option expired");
}

/// A person's Cancel All on the series ends its triggers: entries and exits. Its own internal
/// use by a firing Target doesn't end that Target.
#[test]
fn cancel_all_ends_the_triggers_it_covers() {
    let mut r = Rig::long_call(1);
    let stop = r.arm(call(), TriggerKind::Stop, 68_000);
    let other = r.arm(put(), entry(1, 300), 68_200);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(call()),
    })
    .unwrap();
    assert_eq!(r.reason(stop), "you canceled all");
    assert_eq!(r.state(other), TriggerState::Armed, "another series");
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: None,
    })
    .unwrap();
    assert_eq!(r.state(other), TriggerState::Ended);
}

/// The core asks for a tick when a chase is due and when a feed goes stale, and never for one in
/// the past, which would spin its loop.
#[test]
fn the_wake_is_never_in_the_past() {
    let mut r = Rig::long_call(1);
    r.arm(call(), TriggerKind::Target, 68_500);
    let wake = r.core.next_wake().expect("a stale check");
    assert!(wake.0 > r.t && wake.0 <= r.t + STALE + 1);
    r.quote(Some(300), Some(900));
    r.print(68_500);
    r.quote(Some(280), Some(900));
    r.wait(LAT + 1);
    let wake = r.core.next_wake().expect("a re-price");
    assert!(wake.0 > r.t && wake.0 <= r.t + CHASE);
    // No bid: waiting on a quote, and nothing due in the past.
    r.quote(None, Some(900));
    r.wait(CHASE * 3);
    assert!(r.core.next_wake().is_none_or(|w| w.0 > r.t));
    // Out of hours: an armed trigger has nothing due.
    let mut r = Rig::long_call(1);
    r.arm(call(), TriggerKind::Stop, 68_000);
    r.t = UtcMillis::parse_iso("2026-11-09T21:30:00Z").unwrap().0;
    r.wait(1);
    assert!(r.core.next_wake().is_none_or(|w| w.0 > r.t));
}
