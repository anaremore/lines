//! Flatten pressed again while its order is on its way, on a venue that doesn't honor reduce-only
//! (`BrokerCaps::oco` false): a second market order would turn a closed position into an open one
//! the other way. The paper broker fills; this wrapper removes what would otherwise protect it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, Command, Event, FillModel, MarketInput, Money,
    OrderId, OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, Side, Source, TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn nq() -> ContractKey {
    ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap())
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

/// Ignores reduce-only, and its market orders can't be cancelled once placed: the cancel of a
/// Flatten that has already filled is too late. Can refuse the next market order.
struct Venue {
    inner: PaperBroker,
    /// Reduce-only market orders that reached it.
    flattens: Arc<AtomicUsize>,
    reject_next_market: Arc<AtomicBool>,
    markets: BTreeSet<OrderId>,
    /// Let cancels of market orders through, for the case of one that hasn't filled yet.
    cancels_work: Arc<AtomicBool>,
    /// Hold the next market order unfilled: it sits at the venue until it is cancelled.
    hold_next_market: Arc<AtomicBool>,
    held: Option<OrderId>,
    /// The venue says nothing about a held order, even to a cancel: it lost it.
    silent: Arc<AtomicBool>,
    rejected: Vec<BrokerEvent>,
}

impl Broker for Venue {
    fn name(&self) -> &str {
        "venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        let mut c = self.inner.capabilities();
        c.oco = false;
        c
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        let req = match req {
            BrokerRequest::Place(mut o) if matches!(o.kind, OrderKind::Market) => {
                if o.reduce_only {
                    self.flattens.fetch_add(1, Ordering::Relaxed);
                }
                if self.reject_next_market.swap(false, Ordering::Relaxed) {
                    self.rejected.push(BrokerEvent::Rejected {
                        id: o.id,
                        reason: "refused by the venue".into(),
                        at: now,
                    });
                    return;
                }
                if self.hold_next_market.swap(false, Ordering::Relaxed) {
                    self.held = Some(o.id);
                    return;
                }
                self.markets.insert(o.id);
                o.reduce_only = false;
                BrokerRequest::Place(o)
            }
            BrokerRequest::Cancel { id } if self.held == Some(id) => {
                if self.silent.load(Ordering::Relaxed) {
                    return;
                }
                self.held = None;
                self.rejected.push(BrokerEvent::Acked { id, at: now });
                self.rejected.push(BrokerEvent::Cancelled {
                    id,
                    reason: "cancelled".into(),
                    at: now,
                });
                return;
            }
            BrokerRequest::Cancel { id }
                if self.markets.contains(&id) && !self.cancels_work.load(Ordering::Relaxed) =>
            {
                return;
            }
            other => other,
        };
        self.inner.request(now, req)
    }
    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
        self.inner.market(now, contract, input)
    }
    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.append(&mut self.rejected);
        self.inner.poll(now, out)
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        self.inner.next_wake()
    }
    fn snapshot(&self) -> BrokerSnapshot {
        self.inner.snapshot()
    }
    fn simulated(&self) -> bool {
        true
    }
    fn saved_state(&self) -> Option<serde_json::Value> {
        self.inner.saved_state()
    }
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
    flattens: Arc<AtomicUsize>,
    reject_next_market: Arc<AtomicBool>,
    cancels_work: Arc<AtomicBool>,
    hold_next_market: Arc<AtomicBool>,
    silent: Arc<AtomicBool>,
}

impl Rig {
    fn new() -> Rig {
        let mut inner = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            fill_model: FillModel::TradeThrough,
            ..Default::default()
        });
        inner.open_account(acct(), Money::whole(50_000));
        let flattens = Arc::new(AtomicUsize::new(0));
        let reject_next_market = Arc::new(AtomicBool::new(false));
        let cancels_work = Arc::new(AtomicBool::new(false));
        let hold_next_market = Arc::new(AtomicBool::new(false));
        let silent = Arc::new(AtomicBool::new(false));
        let mut core = TradingCore::new(Box::new(Venue {
            inner,
            flattens: flattens.clone(),
            reject_next_market: reject_next_market.clone(),
            markets: BTreeSet::new(),
            cancels_work: cancels_work.clone(),
            hold_next_market: hold_next_market.clone(),
            held: None,
            silent: silent.clone(),
            rejected: Vec::new(),
        }));
        core.add_account(AccountConfig::paper("paper-1", 50_000), None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
            flattens,
            reject_next_market,
            cancels_work,
            hold_next_market,
            silent,
        };
        r.book(es(), 600_000);
        r.book(nq(), 2_000_000);
        r
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

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.events.extend(self.core.drain());
    }

    fn book(&mut self, contract: ContractKey, bid: i64) {
        self.t += 1;
        let inputs = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: Price(bid),
                qty: Qty(500),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: Price(bid + 25),
                qty: Qty(500),
            },
        ];
        self.core.market(self.now(), contract, &inputs);
        self.events.extend(self.core.drain());
    }

    /// Long `qty`, filled.
    fn long(&mut self, contract: ContractKey, qty: i64) {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract,
            side: Side::Buy,
            qty: Qty(qty),
            kind: OrderKind::Market,
        }))
        .expect("accepted");
        self.wait(LAT + 1);
        assert_eq!(self.pos(contract), qty);
    }

    fn working_limit(&mut self) -> OrderId {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(590_000),
            },
        }))
        .expect("accepted")[0]
    }

    fn flatten(&mut self, contract: Option<ContractKey>) -> Vec<OrderId> {
        self.cmd(Command::Flatten {
            account: acct(),
            contract,
        })
        .expect("Flatten is always accepted")
    }

    fn pos(&self, contract: ContractKey) -> i64 {
        self.core.position(&acct(), contract)
    }

    fn flattens(&self) -> usize {
        self.flattens.load(Ordering::Relaxed)
    }
}

/// The review's scenario: long 5, Flatten, and Flatten again before the fill report.
#[test]
fn a_second_flatten_never_reverses_the_position() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.flatten(Some(es()));
    r.wait(1);
    assert_eq!(r.pos(es()), 5, "no fill report yet");
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.pos(es()), 0, "closed, not reversed");
    assert_eq!(r.flattens(), 1, "one market order to close it");
}

/// The first Flatten's order stays: cancelling it could leave the position open.
#[test]
fn a_second_flatten_leaves_the_first_order_working() {
    let mut r = Rig::new();
    r.cancels_work.store(true, Ordering::Relaxed);
    r.long(es(), 5);
    r.flatten(Some(es()));
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.flattens(), 1);
    assert_eq!(r.pos(es()), 0, "the first order closed it");
}

/// It still cancels what else is working: a stop left behind would open a new position.
#[test]
fn a_second_flatten_still_cancels_other_orders() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.flatten(Some(es()));
    let other = r.working_limit();
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.core.order(other).unwrap().state, OrderState::Cancelled);
    assert_eq!(r.flattens(), 1);
    assert_eq!(r.pos(es()), 0);
}

/// A Flatten the venue refused is over: the next one places an order, so an exit is never dead.
#[test]
fn a_rejected_flatten_allows_a_new_one() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.reject_next_market.store(true, Ordering::Relaxed);
    r.flatten(Some(es()));
    r.wait(1);
    assert_eq!(r.pos(es()), 5);
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.flattens(), 2);
    assert_eq!(r.pos(es()), 0);
}

/// A trader's way out of a Flatten that isn't filling: Cancel all, then Flatten again.
#[test]
fn a_cancelled_flatten_allows_a_new_one() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.hold_next_market.store(true, Ordering::Relaxed);
    let first = r.flatten(Some(es()))[0];
    // Held: nothing is sent again while it is open.
    r.flatten(Some(es()));
    r.wait(LAT + 1);
    assert_eq!(r.flattens(), 1);
    assert_eq!(r.pos(es()), 5);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .expect("accepted");
    r.wait(LAT + 1);
    assert_eq!(r.core.order(first).unwrap().state, OrderState::Cancelled);
    assert_eq!(r.pos(es()), 5);
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.flattens(), 2);
    assert_eq!(r.pos(es()), 0);
}

/// A venue that lost the order and answers nothing, not even a cancel: an exit is never dead. Soon
/// after, pressing again sends a new order.
#[test]
fn a_flatten_nobody_answers_stops_holding_back_the_next() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.hold_next_market.store(true, Ordering::Relaxed);
    r.silent.store(true, Ordering::Relaxed);
    r.flatten(Some(es()));
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .expect("accepted");
    r.wait(4_000);
    r.flatten(Some(es()));
    assert_eq!(r.flattens(), 1, "still within the wait for an answer");
    r.wait(1_100);
    r.flatten(Some(es()));
    assert_eq!(r.flattens(), 2, "the exit works again");
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.pos(es()), 0);
}

/// While a cancel of the first is on its way it may yet fill: no second order until that's known.
#[test]
fn no_second_order_while_the_first_is_being_cancelled() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.flatten(Some(es()));
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .expect("accepted");
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(
        r.pos(es()),
        0,
        "the cancel came too late: closed, not reversed"
    );
    assert_eq!(r.flattens(), 1);
}

/// Account-wide: only the contract whose Flatten is on its way waits; the other still closes.
#[test]
fn an_account_wide_flatten_skips_only_the_contract_already_closing() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.long(nq(), 2);
    r.flatten(Some(es()));
    r.flatten(None);
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.flattens(), 2, "one each");
    assert_eq!(r.pos(es()), 0);
    assert_eq!(r.pos(nq()), 0);
}

/// After it filled, the position is flat and a further Flatten sends nothing.
#[test]
fn a_flatten_after_the_fill_sends_nothing() {
    let mut r = Rig::new();
    r.long(es(), 5);
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    r.flatten(Some(es()));
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.flattens(), 1);
    assert_eq!(r.pos(es()), 0);
}
