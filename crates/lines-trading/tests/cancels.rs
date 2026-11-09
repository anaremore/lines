//! Cancels and Flatten on a broker that loses requests, ignores reduce-only and answers late (a
//! venue with `oco: false`), with the paper broker underneath and time passed in.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{
    BrokerEvent, BrokerRequest, BrokerSnapshot, ContractSelection, PaneContext, RequestContext,
};
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
    /// Events to report on the next poll.
    inject: Arc<Mutex<Vec<BrokerEvent>>>,
    /// Holds a stop or target as reduce-only itself? (`false`: only the core's sweep ends one.)
    honors_exits: Arc<AtomicBool>,
    up: Arc<AtomicBool>,
    /// The context of each cancel that reached it.
    contexts: Arc<Mutex<Vec<RequestContext>>>,
}

impl Broker for Venue {
    fn name(&self) -> &str {
        "venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        let mut c = self.inner.capabilities();
        c.oco = self.honors_exits.load(Ordering::Relaxed);
        c
    }
    fn request_in_context(&mut self, now: UtcMillis, req: BrokerRequest, ctx: &RequestContext) {
        if matches!(req, BrokerRequest::Cancel { .. }) {
            self.contexts.lock().unwrap().push(*ctx);
        }
        self.request(now, req);
    }
    fn connected(&self) -> bool {
        self.up.load(Ordering::Relaxed)
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
            // An exit the broker won't end by itself.
            BrokerRequest::Place(mut o) => {
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
        out.append(&mut self.inject.lock().unwrap());
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
    honors_exits: Arc<AtomicBool>,
    up: Arc<AtomicBool>,
    contexts: Arc<Mutex<Vec<RequestContext>>>,
    inject: Arc<Mutex<Vec<BrokerEvent>>>,
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
        let honors_exits = Arc::new(AtomicBool::new(false));
        let up = Arc::new(AtomicBool::new(true));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let inject = Arc::new(Mutex::new(Vec::new()));
        let mut core = TradingCore::new(Box::new(Venue {
            inner,
            flattens: flattens.clone(),
            cancels: cancels.clone(),
            lose_cancels: lose_cancels.clone(),
            lose_market: lose_market.clone(),
            refuse_cancels: refuse_cancels.clone(),
            refused: Vec::new(),
            honors_exits: honors_exits.clone(),
            up: up.clone(),
            contexts: contexts.clone(),
            inject: inject.clone(),
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
            honors_exits,
            up,
            contexts,
            inject,
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
    let first = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Order(o) if o.state == OrderState::Unresolved => Some(o.id),
            _ => None,
        })
        .expect("the rebuild left it unresolved");
    assert_eq!(r.state(first), OrderState::Unresolved);
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
    assert_eq!(r.failures(), ["still working: no answer to 3 cancels"]);
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
    assert_eq!(
        r.failures(),
        ["still working: cancel refused: too late to cancel"]
    );
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

/// A stranded exit whose cancel the broker refuses: the sweep doesn't ask forever (a rule adds
/// no sends), and a person's command still goes at once.
#[test]
fn a_refused_cancel_on_a_stranded_exit_isnt_asked_forever() {
    let mut r = Rig::new();
    r.honors_exits.store(true, Ordering::Relaxed);
    r.long(1);
    let stop = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Stop {
                trigger: Price(590_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    assert_eq!(r.state(stop), OrderState::Working);
    r.refuse_cancels.store(true, Ordering::Relaxed);
    r.flatten();
    for _ in 0..20 {
        r.wait(LAT + 1);
    }
    assert_eq!(r.pos(), 0);
    assert_eq!(r.sent_cancels(), 1, "one refusal is the answer");
    assert_eq!(r.failures().len(), 1);
    r.wait(10_000);
    assert_eq!(r.sent_cancels(), 1);
    r.refuse_cancels.store(false, Ordering::Relaxed);
    r.cancel(stop);
    r.wait(LAT + 1);
    assert_eq!(r.state(stop), OrderState::Cancelled);
}

/// A cancel for an order a rebuild left unresolved stays on its way: it may still be at the
/// broker.
#[test]
fn a_cancel_survives_its_order_becoming_unresolved() {
    let mut r = Rig::new();
    r.lose_cancels.store(100, Ordering::Relaxed);
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
    r.cancel(id);
    r.rebuild();
    assert_eq!(r.state(id), OrderState::Unresolved);
    r.wait(2_000);
    assert_eq!(r.sent_cancels(), 2, "the rebuild didn't drop it");
}

/// The replacement's cancel for a lost Flatten goes on until answered, then says so: never
/// fire-and-forget, and the order isn't called working.
#[test]
fn the_cancel_of_an_unresolved_flatten_is_sent_again_and_reported() {
    let mut r = Rig::new();
    r.long(5);
    r.lose_market.store(true, Ordering::Relaxed);
    r.flatten();
    r.rebuild();
    r.wait(5_100);
    r.lose_cancels.store(100, Ordering::Relaxed);
    r.flatten();
    let first = r.sent_cancels();
    for _ in 0..3 {
        r.wait(2_000);
    }
    assert_eq!(r.sent_cancels(), first + 2);
    assert_eq!(r.failures(), ["outcome unknown: no answer to 3 cancels"]);
}

/// Even when the position reads flat, a Flatten gets the cancel to an unresolved one: if it
/// fills on a broker ignoring reduce-only, the account would end up short.
#[test]
fn flatten_cancels_an_unresolved_one_when_the_position_reads_flat() {
    let mut r = Rig::new();
    r.long(1);
    r.lose_market.store(true, Ordering::Relaxed);
    r.flatten();
    r.rebuild();
    // Closed elsewhere: another market order the broker does take.
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Market,
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.pos(), 0);
    let before = r.sent_cancels();
    r.flatten();
    assert_eq!(r.sent_cancels(), before + 1);
}

/// While the broker is disconnected nothing is sent and no send is spent, without a wake loop;
/// back up, the cancel goes.
#[test]
fn a_disconnect_spends_no_sends() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(id);
    r.up.store(false, Ordering::Relaxed);
    for _ in 0..6 {
        r.wait(2_000);
        let wake = r.core.next_wake().expect("it still looks");
        assert!(wake.0 > r.t, "no wake loop: {} vs {}", wake.0, r.t);
    }
    assert_eq!(r.sent_cancels(), 1);
    assert!(r.failures().is_empty(), "{:?}", r.failures());
    r.up.store(true, Ordering::Relaxed);
    r.wait(2_000);
    assert_eq!(r.sent_cancels(), 2);
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
}

/// A resend reaches the broker as the first send did: same source, same pane (a venue routes a
/// cancel by the pane's contract).
#[test]
fn a_resend_keeps_the_source_and_the_pane() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(1, Ordering::Relaxed);
    let pane = PaneContext {
        contract: es(),
        selection: ContractSelection::Pinned,
        front_month: Some(es()),
    };
    r.core.submit_from_pane(
        r.now(),
        Source::Dom,
        Instant::now(),
        Some(pane),
        Command::CancelOrder {
            account: acct(),
            order: id,
        },
    );
    r.wait(2_000);
    let sent = r.contexts.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    assert_eq!(sent[1].source, Source::Dom);
    assert_eq!(sent[1].pane, Some(pane));
}

/// A Flatten order lost for good isn't asked about again on every Flatten after: one failure,
/// no more sends, however many Flattens follow.
#[test]
fn a_dead_unresolved_flatten_is_reported_once() {
    let mut r = Rig::new();
    r.long(5);
    r.lose_market.store(true, Ordering::Relaxed);
    r.flatten();
    r.rebuild();
    r.wait(5_100);
    r.lose_cancels.store(100, Ordering::Relaxed);
    r.flatten(); // replaced: flat
    for _ in 0..3 {
        r.wait(2_000);
    }
    assert_eq!(r.pos(), 0);
    assert_eq!(r.failures().len(), 1);
    let sent = r.sent_cancels();
    for _ in 0..3 {
        r.flatten();
        r.wait(2_000);
    }
    assert_eq!(r.sent_cancels(), sent, "no more cancels for it");
    assert_eq!(r.failures().len(), 1, "no more warnings");
}

/// The paper broker answers a cancel for an order it never got, so the unresolved order ends.
#[test]
fn paper_ends_an_order_it_never_got() {
    let mut r = Rig::new();
    r.long(1);
    r.lose_market.store(true, Ordering::Relaxed);
    r.flatten();
    r.rebuild();
    let lost = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Order(o) if o.state == OrderState::Unresolved => Some(o.id),
            _ => None,
        })
        .unwrap();
    r.wait(5_100);
    r.flatten();
    r.wait(LAT + 1);
    assert_eq!(r.state(lost), OrderState::Cancelled);
    assert_eq!(r.pos(), 0);
    assert!(r.failures().is_empty());
}

/// A cancel asked for while disconnected is remembered, sent when the broker is back, and
/// spends nothing; a rule's too, and a rule's send doesn't change who asked.
#[test]
fn commands_while_disconnected_spend_nothing_and_rules_keep_who_asked() {
    let mut r = Rig::new();
    let id = r.resting();
    r.up.store(false, Ordering::Relaxed);
    r.cancel(id);
    assert_eq!(r.sent_cancels(), 0);
    r.wait(2_000);
    r.cmd_from(
        Source::Rule,
        Command::CancelOrder {
            account: acct(),
            order: id,
        },
    )
    .unwrap();
    r.wait(6_000);
    assert_eq!(r.sent_cancels(), 0);
    assert!(r.failures().is_empty(), "{:?}", r.failures());
    r.up.store(true, Ordering::Relaxed);
    r.wait(2_000);
    assert_eq!(r.sent_cancels(), 1);
    assert_eq!(
        r.contexts.lock().unwrap()[0].source,
        Source::Dom,
        "as the person asked"
    );
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
}

/// Rebuilds don't let a rule burn the sends: it waits the 2 s like any resend.
#[test]
fn rebuilds_dont_let_a_rule_burn_the_sends() {
    let mut r = Rig::new();
    let id = r.resting();
    r.lose_cancels.store(100, Ordering::Relaxed);
    r.cancel(id);
    for _ in 0..3 {
        r.rebuild();
        r.cmd_from(
            Source::Rule,
            Command::CancelOrder {
                account: acct(),
                order: id,
            },
        )
        .unwrap();
    }
    assert_eq!(r.sent_cancels(), 1);
}

/// A Flatten while the broker is disconnected is refused whole: the stop stays, now and after
/// reconnecting, whatever became of the market order it would have sent.
#[test]
fn a_flatten_while_disconnected_never_strips_the_stop() {
    let mut r = Rig::new();
    r.honors_exits.store(true, Ordering::Relaxed);
    r.long(5);
    let stop = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(5),
            kind: OrderKind::Stop {
                trigger: Price(590_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.up.store(false, Ordering::Relaxed);
    let no = r
        .cmd(Command::Flatten {
            account: acct(),
            contract: Some(es()),
        })
        .unwrap_err();
    assert!(no.message.contains("disconnected"), "{}", no.message);
    assert_eq!(r.flattens.load(Ordering::Relaxed), 0);
    r.wait(2_000);
    r.up.store(true, Ordering::Relaxed);
    r.wait(10_000);
    assert_eq!(r.sent_cancels(), 0, "nothing remembered to send");
    assert_eq!(r.state(stop), OrderState::Working, "the stop stands");
    assert_eq!(r.pos(), 5);
}

/// Two refusals of an unresolved order's cancels leave it unresolved, and say so once.
#[test]
fn a_second_refusal_doesnt_end_an_unresolved_order() {
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
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(id);
    r.rebuild();
    assert_eq!(r.state(id), OrderState::Unresolved);
    // Unresolved: the broker refuses the cancel, twice over (a resend was outstanding).
    let refusal = BrokerEvent::Rejected {
        id,
        reason: "too late to cancel".into(),
        at: UtcMillis(r.t),
    };
    r.inject.lock().unwrap().extend([refusal.clone(), refusal]);
    r.wait(1);
    assert_eq!(r.state(id), OrderState::Unresolved);
    assert_eq!(r.failures().len(), 1);
}

/// A `Rejected` for an order whose cancel was only remembered isn't a refused cancel: nothing
/// was sent to refuse. One sent before a disconnect stays sent, whatever a person asks meanwhile.
#[test]
fn only_a_cancel_that_was_sent_can_be_refused() {
    let mut r = Rig::new();
    let place = |r: &mut Rig| {
        r.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(590_000),
            },
        }))
        .unwrap()[0]
    };
    let (remembered, sent) = (place(&mut r), place(&mut r));
    let at = UtcMillis(r.t);
    r.inject.lock().unwrap().extend([
        BrokerEvent::Acked { id: remembered, at },
        BrokerEvent::Acked { id: sent, at },
    ]);
    r.wait(1);
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(sent);
    r.up.store(false, Ordering::Relaxed);
    r.cancel(remembered);
    r.wait(2_000);
    r.cancel(sent); // asked again while it is down
    let refused = |id| BrokerEvent::Rejected {
        id,
        reason: "no".into(),
        at,
    };
    r.inject
        .lock()
        .unwrap()
        .extend([refused(remembered), refused(sent)]);
    r.wait(1);
    assert_eq!(
        r.failures().len(),
        1,
        "only the one that was sent: {:?}",
        r.failures()
    );
    assert_eq!(r.state(sent), OrderState::Working);
    // The remembered one wasn't dropped: it goes when the broker is back.
    r.up.store(true, Ordering::Relaxed);
    r.wait(2_000);
    r.wait(LAT + 1);
    assert_eq!(r.state(remembered), OrderState::Cancelled);
}

/// A refused cancel on an acked order leaves it alive: a held bracket exit, or an order between
/// ack and working, is still at the broker. Without a cancel sent, `Rejected` is the order's own.
#[test]
fn a_refused_cancel_on_an_acked_order_leaves_it_alive() {
    let mut r = Rig::new();
    let place = |r: &mut Rig| {
        r.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(590_000),
            },
        }))
        .unwrap()[0]
    };
    let (kept, ended) = (place(&mut r), place(&mut r));
    let at = UtcMillis(r.t);
    r.inject.lock().unwrap().extend([
        BrokerEvent::Acked { id: kept, at },
        BrokerEvent::Acked { id: ended, at },
    ]);
    r.wait(1);
    assert_eq!(r.state(kept), OrderState::Acked);
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.cancel(kept);
    let refused = |id| BrokerEvent::Rejected {
        id,
        reason: "too late to cancel".into(),
        at,
    };
    r.inject
        .lock()
        .unwrap()
        .extend([refused(kept), refused(ended)]);
    r.wait(1);
    assert_eq!(r.state(kept), OrderState::Acked, "the cancel was refused");
    assert!(matches!(r.state(ended), OrderState::Rejected { .. }));
    assert_eq!(r.failures().len(), 1);
}
