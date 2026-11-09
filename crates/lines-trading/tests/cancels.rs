//! Cancels and Flatten on a broker that loses requests, ignores reduce-only and answers late (a
//! venue with `oco: false`), with the paper broker underneath and time passed in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, Command, Event, MarketInput, Money, OrderId,
    OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, Side, Source, TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

/// `oco: false`, ignores reduce-only, can lose or refuse what it is sent. Counts what reaches it.
struct Venue {
    inner: PaperBroker,
    flattens: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
    /// How many cancels to lose from now on.
    lose_cancels: Arc<AtomicUsize>,
    /// Lose the next market order.
    lose_market: Arc<AtomicBool>,
    /// Refuse cancels, as a broker that says "too late to cancel" would.
    refuse_cancels: Arc<AtomicBool>,
    refused: Vec<BrokerEvent>,
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
                if self.lose_market.swap(false, Ordering::Relaxed) {
                    return;
                }
                o.reduce_only = false;
                BrokerRequest::Place(o)
            }
            BrokerRequest::Cancel { id } => {
                self.cancels.fetch_add(1, Ordering::Relaxed);
                if self.refuse_cancels.load(Ordering::Relaxed) {
                    self.refused.push(BrokerEvent::Rejected {
                        id,
                        reason: "too late to cancel".into(),
                        at: now,
                    });
                    return;
                }
                let lost = self
                    .lose_cancels
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_ok();
                if lost {
                    return;
                }
                BrokerRequest::Cancel { id }
            }
            other => other,
        };
        self.inner.request(now, req)
    }
    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
        self.inner.market(now, contract, input)
    }
    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.append(&mut self.refused);
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
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
    flattens: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
    lose_cancels: Arc<AtomicUsize>,
    lose_market: Arc<AtomicBool>,
    refuse_cancels: Arc<AtomicBool>,
}

impl Rig {
    fn new() -> Rig {
        let mut inner = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            ..Default::default()
        });
        inner.open_account(acct(), Money::whole(100_000));
        let flattens = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::new(AtomicUsize::new(0));
        let lose_cancels = Arc::new(AtomicUsize::new(0));
        let lose_market = Arc::new(AtomicBool::new(false));
        let refuse_cancels = Arc::new(AtomicBool::new(false));
        let mut core = TradingCore::new(Box::new(Venue {
            inner,
            flattens: flattens.clone(),
            cancels: cancels.clone(),
            lose_cancels: lose_cancels.clone(),
            lose_market: lose_market.clone(),
            refuse_cancels: refuse_cancels.clone(),
            refused: Vec::new(),
        }));
        core.add_account(AccountConfig::paper("paper-1", 100_000), None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
            flattens,
            cancels,
            lose_cancels,
            lose_market,
            refuse_cancels,
        };
        r.t += 1;
        let book = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: Price(600_000),
                qty: Qty(500),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: Price(600_025),
                qty: Qty(500),
            },
        ];
        r.core.market(r.now(), es(), &book);
        r
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
        self.cmd_from(Source::Dom, c)
    }

    fn cmd_from(
        &mut self,
        source: Source,
        c: Command,
    ) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let id = self.core.submit(self.now(), source, Instant::now(), c);
        self.collect();
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("answered")
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn rebuild(&mut self) {
        self.t += 1;
        self.core.rebuild(self.now());
        self.collect();
    }

    fn long(&mut self, q: i64) {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(q),
            kind: OrderKind::Market,
        }))
        .unwrap();
        self.wait(LAT + 1);
        assert_eq!(self.pos(), q);
    }

    /// A resting limit far from the market.
    fn resting(&mut self) -> OrderId {
        let id = self
            .cmd(Command::PlaceOrder(OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(1),
                kind: OrderKind::Limit {
                    price: Price(590_000),
                },
            }))
            .unwrap()[0];
        self.wait(LAT + 1);
        id
    }

    fn flatten(&mut self) {
        self.cmd(Command::Flatten {
            account: acct(),
            contract: Some(es()),
        })
        .unwrap();
    }

    fn cancel(&mut self, order: OrderId) {
        self.cmd(Command::CancelOrder {
            account: acct(),
            order,
        })
        .unwrap();
    }

    fn pos(&self) -> i64 {
        self.core.position(&acct(), es())
    }

    fn state(&self, id: OrderId) -> OrderState {
        self.core.order(id).unwrap().state.clone()
    }

    fn sent_cancels(&self) -> usize {
        self.cancels.load(Ordering::Relaxed)
    }

    fn failures(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::CancelFailed { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }
}

/// A rebuild before the broker answers makes the Flatten unresolved. It may still fill, so a
/// second Flatten sends nothing: long 5 must not end short 5.
#[test]
fn a_rebuild_during_a_flatten_never_lets_a_second_one_reverse() {
    let mut r = Rig::new();
    r.long(5);
    r.flatten();
    r.rebuild();
    r.flatten();
    for _ in 0..5 {
        r.wait(LAT);
    }
    assert_eq!(r.flattens.load(Ordering::Relaxed), 1);
    assert_eq!(r.pos(), 0, "flat, not reversed");
}

/// The 5 s rule holds for an unresolved Flatten too: lost for good, the next one past that goes,
/// after a cancel for the old one. Before then nothing goes.
#[test]
fn an_unresolved_flatten_that_was_lost_is_replaced_after_five_seconds() {
    let mut r = Rig::new();
    r.long(5);
    r.lose_market.store(true, Ordering::Relaxed);
    r.flatten();
    r.rebuild();
    r.wait(4_900);
    r.flatten();
    assert_eq!(r.flattens.load(Ordering::Relaxed), 1, "still counted");
    r.wait(200);
    let before = r.sent_cancels();
    r.flatten();
    assert_eq!(r.flattens.load(Ordering::Relaxed), 2, "replaced");
    assert_eq!(r.sent_cancels(), before + 1, "the old one cancelled first");
    r.wait(LAT + 1);
    assert_eq!(r.pos(), 0);
}

/// A lost cancel goes again by itself, about 2 s later, and the service is woken for it.
#[test]
fn a_lost_cancel_is_sent_again_after_two_seconds() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(id);
    let wake = r.core.next_wake().expect("a wake for the resend");
    assert!(wake.0 <= r.t + 2_000, "{} vs {}", wake.0, r.t);
    r.wait(1_900);
    assert_eq!(r.sent_cancels(), 1, "not before it may have been answered");
    r.wait(100);
    assert_eq!(r.sent_cancels(), 2);
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
    assert!(r.core.next_wake().is_none(), "nothing left to wake for");
}

/// Repeated clicks while a cancel is on its way send one.
#[test]
fn a_cancel_on_its_way_isnt_sent_twice() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(id);
    r.cancel(id);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.flatten();
    assert_eq!(r.sent_cancels(), 1);
}

/// After a rebuild the broker's list is new: every later command for an order it still lists
/// sends at once (the review's probe: only the lost one ever went, and it stayed working).
#[test]
fn after_a_rebuild_a_cancel_goes_at_once() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(LAT + 1);
    r.rebuild();
    assert_eq!(
        r.state(id),
        OrderState::Working,
        "the broker still lists it"
    );
    r.cancel(id);
    assert_eq!(r.sent_cancels(), 2);
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
}

/// A rebuild never stops the resend (a venue rebuilds on every revision).
#[test]
fn rebuilds_dont_stop_the_resend() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(id);
    for _ in 0..4 {
        r.wait(500);
        r.rebuild();
    }
    r.wait(100);
    assert_eq!(r.sent_cancels(), 2);
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
}

/// Three sends, then it says so once and stops; a person's next command starts again.
#[test]
fn a_cancel_nobody_answers_is_reported_once_and_left_to_the_person() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(100, Ordering::Relaxed);
    r.cancel(id);
    for _ in 0..10 {
        r.wait(2_000);
    }
    assert_eq!(r.sent_cancels(), 3, "no more on its own");
    assert_eq!(r.failures(), ["no answer to 3 cancels"]);
    assert_eq!(r.state(id), OrderState::Working, "never assumed gone");
    assert!(r.core.next_wake().is_none(), "no wake loop");
    // A venue's rebuilds neither send it again nor say it again.
    for _ in 0..3 {
        r.rebuild();
        r.wait(2_000);
    }
    assert_eq!(r.sent_cancels(), 3);
    assert_eq!(r.failures().len(), 1);
    r.lose_cancels.store(0, Ordering::Relaxed);
    r.cancel(id);
    assert_eq!(r.sent_cancels(), 4);
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
}

/// A rule's cancel doesn't get a new budget: only a person's does.
#[test]
fn a_rule_adds_no_sends_past_the_budget() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(100, Ordering::Relaxed);
    r.cancel(id);
    for _ in 0..3 {
        r.wait(2_000);
    }
    assert_eq!(r.sent_cancels(), 3);
    r.cmd_from(
        Source::Rule,
        Command::CancelOrder {
            account: acct(),
            order: id,
        },
    )
    .unwrap();
    assert_eq!(r.sent_cancels(), 3);
}

/// How a broker refuses a cancel: `Rejected` for a working order. The order is live and stays
/// so, nothing resends on its own, it is said, and the next command goes at once.
#[test]
fn a_refused_cancel_leaves_the_order_working_and_says_why() {
    let mut r = Rig::new();
    let id = r.resting();
    assert_eq!(r.state(id), OrderState::Working);
    r.refuse_cancels.store(true, Ordering::Relaxed);
    r.cancel(id);
    r.wait(1);
    assert_eq!(
        r.state(id),
        OrderState::Working,
        "never Rejected: it is live"
    );
    assert_eq!(r.failures(), ["cancel refused: too late to cancel"]);
    r.wait(10_000);
    assert_eq!(r.sent_cancels(), 1, "refused isn't resent on its own");
    r.refuse_cancels.store(false, Ordering::Relaxed);
    r.cancel(id);
    assert_eq!(r.sent_cancels(), 2);
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
}

/// A `Rejected` for an order not yet working is still the order's own: it ends.
#[test]
fn a_rejected_order_with_a_cancel_on_its_way_is_rejected() {
    let mut r = Rig::new();
    let id = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(590_000),
            },
        }))
        .unwrap()[0];
    r.refuse_cancels.store(true, Ordering::Relaxed);
    r.cancel(id);
    r.wait(1);
    assert!(
        matches!(r.state(id), OrderState::Rejected { .. }),
        "{:?}",
        r.state(id)
    );
    assert!(r.failures().is_empty());
}
