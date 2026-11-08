//! A broker extension in observe mode through the app's trading layer (decision 0020):
//! accounts discovered after start, the venue's positions, orders and fills in the cache and
//! journal, and every command on a read-only account refused here, never reaching the broker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::FeedConfig;
use lines_feed::Source as FeedSource;
use lines_feed::catalog;
use lines_feed::registry::FeedRegistry;
use lines_trading::broker::{BrokerAccount, BrokerPosition};
use lines_trading::extension::{
    AccountKind, AccountSource, BrokerExtension, ExtensionAccount, VenueStatus,
};
use lines_trading::order::Role;
use lines_trading::position::{Lot, Position};
use lines_trading::{
    AccountId, Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot,
    Command as TradeCommand, MarketInput, Money, Order, OrderId, OrderKind, OrderSpec, OrderState,
    Side, Source,
};

use super::{FlashKind, Trading, TradingSetup};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

#[derive(Default)]
struct Venue {
    accounts: Vec<ExtensionAccount>,
    snapshot: BrokerSnapshot,
    pending: VecDeque<BrokerEvent>,
    rev: u64,
}

struct Shared {
    v: Mutex<Venue>,
    requests: AtomicUsize,
}

struct Accounts(Arc<Shared>);

impl AccountSource for Accounts {
    fn accounts(&self) -> Vec<ExtensionAccount> {
        self.0.v.lock().unwrap().accounts.clone()
    }
    fn revision(&self) -> u64 {
        self.0.v.lock().unwrap().rev
    }
    fn status(&self) -> Option<VenueStatus> {
        Some(VenueStatus {
            connected: true,
            detail: "connected".into(),
            last_message: Some(lines_feed::now_utc()),
        })
    }
}

struct Observing(Arc<Shared>);

impl Broker for Observing {
    fn name(&self) -> &str {
        "observing"
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
    fn request(&mut self, _now: UtcMillis, _req: BrokerRequest) {
        self.0.requests.fetch_add(1, Ordering::SeqCst);
    }
    fn market(&mut self, _now: UtcMillis, _c: ContractKey, _i: &MarketInput) {}
    fn poll(&mut self, _now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.extend(self.0.v.lock().unwrap().pending.drain(..));
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        self.0.v.lock().unwrap().snapshot.clone()
    }
}

struct Ext(Arc<Shared>);

impl BrokerExtension for Ext {
    fn name(&self) -> &str {
        "observing"
    }
    fn label(&self) -> &str {
        "Observed venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        Observing(self.0.clone()).capabilities()
    }
    fn accounts(&self) -> Arc<dyn AccountSource> {
        Arc::new(Accounts(self.0.clone()))
    }
    fn open(&self) -> anyhow::Result<Box<dyn Broker>> {
        Ok(Box::new(Observing(self.0.clone())))
    }
}

fn sim_account() -> ExtensionAccount {
    ExtensionAccount {
        id: AccountId::new("venue-sim"),
        label: "Venue sim m1".into(),
        kind: AccountKind::Simulated,
        tradable: Err("Sim: order entry not enabled yet".into()),
    }
}

fn venue_order() -> Order {
    let t = lines_feed::now_utc();
    Order {
        id: OrderId(700_001),
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
        created: t,
        updated: t,
    }
}

fn start(shared: &Arc<Shared>) -> Trading {
    start_with(shared, true)
}

/// `marks`: the panes' (replay) feeds reach the venue's core (`venue_marks_always`).
fn start_with(shared: &Arc<Shared>, marks: bool) -> Trading {
    let mut setup = TradingSetup::manual();
    setup.brokers = vec![Arc::new(Ext(shared.clone()))];
    setup.venue_marks_always = marks;
    let base = FeedConfig::new(FeedSource::Replay {
        path: "unused.lfr".into(),
        speed: 0.0,
        stop_at: None,
    });
    Trading::start(setup, FeedRegistry::new(), base)
}

/// Wait (bounded) until `f` holds, settling between looks.
fn until(t: &Trading, f: impl Fn(&Trading) -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        t.settle();
        if f(t) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn a_read_only_venue_shows_its_state_and_refuses_every_command() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue::default()),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    // Paper accounts only until the venue reports its accounts.
    assert!(t.accounts().iter().all(|a| a.paper));
    {
        let mut v = shared.v.lock().unwrap();
        v.accounts = vec![
            sim_account(),
            ExtensionAccount {
                id: AccountId::new("venue-live"),
                label: "Venue 01".into(),
                kind: AccountKind::Live,
                tradable: Err("observe mode".into()),
            },
        ];
        let mut long2 = Position::default();
        long2.lots.push_back(Lot {
            price: 600000,
            qty: 2,
        });
        v.snapshot = BrokerSnapshot {
            accounts: vec![BrokerAccount {
                id: AccountId::new("venue-sim"),
                balance: Money::whole(50_000),
                positions: vec![BrokerPosition {
                    contract: es(),
                    position: long2,
                }],
                orders: vec![venue_order()],
            }],
        };
        v.rev = 1;
    }
    let sim = AccountId::new("venue-sim");
    assert!(
        until(&t, |t| t.account(&sim).is_some()
            && t.cache.read().position(&sim, es()).is_some()),
        "the venue's account and position arrive"
    );
    let info = t.account(&sim).unwrap();
    assert_eq!((info.badge, info.paper), ("SIM", false));
    assert_eq!(
        info.read_only.as_deref(),
        Some("Sim: order entry not enabled yet")
    );
    assert_eq!(
        t.account(&AccountId::new("venue-live")).unwrap().badge,
        "LIVE"
    );
    let (label, status) = t.venue_status(info.venue.unwrap()).unwrap();
    assert_eq!(label, "Observed venue");
    assert!(status.unwrap().connected);
    assert_eq!(t.cache.read().position(&sim, es()).unwrap().qty, 2);
    assert_eq!(t.cache.read().open_orders(&sim, es()).count(), 1);

    // Every command on it is DENIED here and never reaches the broker.
    let cmds = vec![
        TradeCommand::PlaceOrder(OrderSpec {
            account: sim.clone(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(600000),
            },
        }),
        TradeCommand::CancelOrder {
            account: sim.clone(),
            order: OrderId(700_001),
        },
        TradeCommand::ModifyOrder {
            account: sim.clone(),
            order: OrderId(700_001),
            qty: None,
            price: Some(Price(601_250)),
        },
        TradeCommand::Flatten {
            account: sim.clone(),
            contract: Some(es()),
        },
        TradeCommand::CancelAll {
            account: sim.clone(),
            contract: Some(es()),
        },
    ];
    for c in cmds {
        let id = t.submit(Source::Dom, Instant::now(), c);
        let s = t.cache.read();
        assert!(s.result(id).expect("answered at once").is_err());
    }
    t.settle();
    assert_eq!(
        shared.requests.load(Ordering::SeqCst),
        0,
        "nothing was sent"
    );
    let s = t.cache.read();
    let flashes: Vec<_> = s.flashes(&sim, Some(es()), Instant::now()).collect();
    assert!(flashes.len() >= 5, "{flashes:?}");
    assert!(flashes.iter().all(|f| f.kind == FlashKind::Denied));
    assert!(
        flashes
            .iter()
            .all(|f| f.message == "Sim: order entry not enabled yet")
    );
}

/// A venue's account changing kind with the count unchanged moves the account generation, so
/// the journal's labels and header are read again (real money never stays marked simulated).
#[test]
fn a_changed_account_moves_the_account_generation() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let id = AccountId::new("venue-sim");
    assert!(until(&t, |t| t
        .account(&id)
        .is_some_and(|a| a.badge == "SIM")));
    let count = t.accounts().len();
    let before = super::accounts_epoch();
    {
        let mut v = shared.v.lock().unwrap();
        v.accounts[0].kind = AccountKind::Live;
        v.rev += 1;
    }
    assert!(until(&t, |t| t
        .account(&id)
        .is_some_and(|a| a.badge == "LIVE")));
    assert_eq!(t.accounts().len(), count, "the same count");
    // `>`: the static is shared with tests running alongside.
    assert!(super::accounts_epoch() > before);
}

#[test]
fn venue_fills_reach_the_journal_without_moving_its_positions() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t.account(&sim).is_some()));
    let now = lines_feed::now_utc();
    {
        let mut v = shared.v.lock().unwrap();
        for (side, px, id, dt) in [
            (Side::Buy, 600000, "X1", 0),
            (Side::Sell, 600100, "X2", 60_000),
        ] {
            v.pending.push_back(BrokerEvent::ExternalFill {
                account: sim.clone(),
                contract: es(),
                side,
                price: Price(px),
                qty: Qty(1),
                fee: Money::ZERO,
                at: UtcMillis(now.0 + dt),
                trade_id: id.into(),
            });
        }
    }
    // The venue's core polls its broker on its clock: move it on.
    assert!(until(&t, |t| {
        t.venues[0].handle.advance_to(UtcMillis(now.0 + 120_000));
        t.venues[0].handle.sync();
        t.journal.sync();
        t.journal
            .snapshot()
            .is_some_and(|s| s.trades.iter().any(|tr| tr.account == sim))
    }));
    assert!(
        t.cache.read().position(&sim, es()).is_none(),
        "positions are the snapshot's (flat here), not the fills'"
    );
}

/// A venue's order ids may be any the broker likes (here 1, as paper's first order): the
/// shared cache sees them translated, so they never replace another core's orders.
#[test]
fn venue_order_ids_never_collide_with_paper_ids() {
    let mut order = venue_order();
    order.id = OrderId(1);
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            snapshot: BrokerSnapshot {
                accounts: vec![BrokerAccount {
                    id: AccountId::new("venue-sim"),
                    balance: Money::whole(50_000),
                    positions: Vec::new(),
                    orders: vec![order],
                }],
            },
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t
        .cache
        .read()
        .open_orders(&sim, es())
        .count()
        == 1));
    let s = t.cache.read();
    let id = s.open_orders(&sim, es()).next().unwrap().id;
    assert_ne!(id, OrderId(1), "translated");
    assert!(s.order(OrderId(1)).is_none(), "id 1 stays free for paper");
}

/// A reconnect lists the same accounts again: the core already has them, but the venue's
/// new snapshot is shown (the watcher rebuilds on every change).
#[test]
fn a_reconnect_rebuilds_from_the_new_snapshot() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    // `settle` has the watcher look now, so each step below sees exactly the venue's state
    // (no waiting on its poll or rebuild timers).
    t.settle();
    assert!(t.account(&sim).is_some());
    {
        // Disconnected: no accounts listed.
        let mut v = shared.v.lock().unwrap();
        v.accounts.clear();
        v.rev = 2;
    }
    t.settle();
    // Still listed, read-only, with its badge: never shown as another kind or tradable.
    assert_eq!(
        t.account(&sim).unwrap().read_only.as_deref(),
        Some(super::NOT_LISTED)
    );
    assert_eq!(t.account(&sim).unwrap().badge, "SIM");
    assert!(t.is_read_only(&sim));
    {
        // Reconnected, long 3 meanwhile: listed and snapshot under one revision.
        let mut v = shared.v.lock().unwrap();
        v.accounts = vec![sim_account()];
        let mut long3 = Position::default();
        long3.lots.push_back(Lot {
            price: 600000,
            qty: 3,
        });
        v.snapshot = BrokerSnapshot {
            accounts: vec![BrokerAccount {
                id: sim.clone(),
                balance: Money::whole(50_000),
                positions: vec![BrokerPosition {
                    contract: es(),
                    position: long3,
                }],
                orders: Vec::new(),
            }],
        };
        v.rev = 3;
    }
    t.settle();
    assert_eq!(t.cache.read().position(&sim, es()).map(|p| p.qty), Some(3));
    assert_eq!(
        t.account(&sim).unwrap().read_only.as_deref(),
        Some("Sim: order entry not enabled yet")
    );
}

/// The watcher's rebuild decision: the first change rebuilds at once, a change within
/// `VENUE_REBUILD` of the last rebuild waits for it (a quick reconnect is never dropped), and
/// `settle` asking skips the wait; with nothing changed nothing rebuilds.
#[test]
fn the_watcher_rebuilds_at_most_every_venue_rebuild_unless_asked() {
    use super::{VENUE_REBUILD, rebuild_due};
    let t0 = Instant::now();
    assert!(rebuild_due(true, false, None, t0));
    assert!(!rebuild_due(true, false, Some(t0), t0 + VENUE_REBUILD / 2));
    assert!(rebuild_due(true, false, Some(t0), t0 + VENUE_REBUILD));
    assert!(rebuild_due(true, true, Some(t0), t0 + VENUE_REBUILD / 2));
    assert!(!rebuild_due(false, false, None, t0));
    assert!(!rebuild_due(false, true, Some(t0), t0 + VENUE_REBUILD));
}

/// With a broker configured, a pane restored on a broker account not listed yet keeps it
/// (shown waiting, read-only); every command on it is denied and nothing is sent.
#[test]
fn a_saved_broker_account_waits_for_its_broker() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue::default()),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let later = AccountId::new("venue-sim");
    assert!(t.account(&later).is_none());
    assert!(t.keeps_saved_account(&later));
    assert!(t.is_read_only(&later));
    let id = t.submit(
        Source::Dom,
        Instant::now(),
        TradeCommand::Flatten {
            account: later.clone(),
            contract: Some(es()),
        },
    );
    assert!(t.cache.read().result(id).unwrap().is_err());
    t.settle();
    assert_eq!(shared.requests.load(Ordering::SeqCst), 0);
}

fn long(qty: i64) -> Position {
    let mut p = Position::default();
    p.lots.push_back(Lot { price: 600000, qty });
    p
}

fn set_position(shared: &Shared, qty: i64) {
    let mut v = shared.v.lock().unwrap();
    v.snapshot = BrokerSnapshot {
        accounts: vec![BrokerAccount {
            id: AccountId::new("venue-sim"),
            balance: Money::whole(50_000),
            positions: if qty == 0 {
                Vec::new()
            } else {
                vec![BrokerPosition {
                    contract: es(),
                    position: long(qty),
                }]
            },
            orders: Vec::new(),
        }],
    };
    v.rev += 1;
}

/// A venue's positions are marked from the pane's feed: open P&L, not zero.
#[test]
fn venue_positions_get_open_pnl_from_the_panes_feed() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    set_position(&shared, 2);
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    let product = catalog::get(catalog::ES).unwrap();
    let store = Arc::new(lines_feed::store::ChartStore::new(
        product,
        Some(es()),
        lines_core::Capabilities::default(),
        "test",
        60_000,
        100,
    ));
    store
        .read()
        .apply(&[lines_feed::provider::MarketEvent::Trade {
            time: lines_feed::now_utc(),
            price: Price(601000), // 10 points over the entry
            qty: Qty(1),
            aggressor: None,
        }]);
    t.follow(&store);
    assert!(
        until(&t, |t| t
            .cache
            .read()
            .position(&sim, es())
            .is_some_and(|p| p.qty == 2 && p.unrealised > Money::ZERO)),
        "marked"
    );
}

/// The journal follows a venue account from its fills: a position report that reaches the
/// core before the execution that made it is not counted on top of it.
#[test]
fn the_journal_counts_a_venue_execution_once_whatever_arrives_first() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t.account(&sim).is_some()));
    let now = lines_feed::now_utc();
    let fill = |side, px, id: &str, dt| BrokerEvent::ExternalFill {
        account: sim.clone(),
        contract: es(),
        side,
        price: Price(px),
        qty: Qty(2),
        fee: Money::ZERO,
        at: UtcMillis(now.0 + dt),
        trade_id: id.into(),
    };
    // The venue buys 2: its position report first, the execution later.
    set_position(&shared, 2);
    assert!(until(&t, |t| t
        .cache
        .read()
        .position(&sim, es())
        .is_some_and(|p| p.qty == 2)));
    shared
        .v
        .lock()
        .unwrap()
        .pending
        .push_back(fill(Side::Buy, 600000, "X1", 0));
    // Then sells 2: execution first this time.
    shared
        .v
        .lock()
        .unwrap()
        .pending
        .push_back(fill(Side::Sell, 600100, "X2", 60_000));
    set_position(&shared, 0);
    assert!(until(&t, |t| {
        t.journal.sync();
        t.journal
            .snapshot()
            .is_some_and(|s| s.trades.iter().any(|tr| tr.account == sim))
    }));
    let snap = t.journal.snapshot().unwrap();
    let trades: Vec<_> = snap.trades.iter().filter(|tr| tr.account == sim).collect();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].max_qty, 2, "counted once");
    assert_eq!(snap.open, 0, "nothing left open");
}

/// A store with one ES trade at `price`.
fn store_at(price: i64) -> Arc<lines_feed::store::ChartStore> {
    let store = Arc::new(lines_feed::store::ChartStore::new(
        catalog::get(catalog::ES).unwrap(),
        Some(es()),
        lines_core::Capabilities::default(),
        "test",
        60_000,
        100,
    ));
    store
        .read()
        .apply(&[lines_feed::provider::MarketEvent::Trade {
            time: lines_feed::now_utc(),
            price: Price(price),
            qty: Qty(1),
            aggressor: None,
        }]);
    store
}

/// A replay's (or the mock's) prices never mark a real venue's positions.
#[test]
fn replay_prices_never_mark_venue_positions() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    set_position(&shared, 2);
    let t = start_with(&shared, false);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t
        .cache
        .read()
        .position(&sim, es())
        .is_some_and(|p| p.qty == 2)));
    t.follow(&store_at(595_000));
    t.settle();
    assert_eq!(
        t.cache.read().position(&sim, es()).unwrap().unrealised,
        Money::ZERO,
        "not marked from the replay"
    );
}

/// A position open when the account joins seeds the journal once; closing it is one trade.
#[test]
fn a_position_open_at_join_seeds_the_journal_once() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    set_position(&shared, 2);
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t
        .cache
        .read()
        .position(&sim, es())
        .is_some_and(|p| p.qty == 2)));
    shared
        .v
        .lock()
        .unwrap()
        .pending
        .push_back(BrokerEvent::ExternalFill {
            account: sim.clone(),
            contract: es(),
            side: Side::Sell,
            price: Price(600_100),
            qty: Qty(2),
            fee: Money::ZERO,
            at: lines_feed::now_utc(),
            trade_id: "S1".into(),
        });
    set_position(&shared, 0);
    assert!(until(&t, |t| {
        t.journal.sync();
        t.journal
            .snapshot()
            .is_some_and(|s| s.trades.iter().any(|tr| tr.account == sim))
    }));
    let snap = t.journal.snapshot().unwrap();
    let trades: Vec<_> = snap.trades.iter().filter(|tr| tr.account == sim).collect();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].max_qty, 2);
    assert_eq!(snap.open, 0);
}

/// Executions made while the venue was disconnected arrive as external fills after the
/// reconnect (the `BrokerCaps::external_fills` contract): the journal stays right although it
/// ignores the reconnect's position report.
#[test]
fn executions_missed_while_disconnected_reach_the_journal_after_reconnect() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start(&shared);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t.account(&sim).is_some()));
    let now = lines_feed::now_utc();
    {
        // Disconnected; the trader buys 2 meanwhile.
        let mut v = shared.v.lock().unwrap();
        v.accounts.clear();
        v.rev += 1;
    }
    assert!(until(&t, |t| t.is_read_only(&sim)
        && t.account(&sim).unwrap().read_only.as_deref()
            == Some(super::NOT_LISTED)));
    {
        // Reconnected: the snapshot is long 2 and the missed execution is reported.
        let mut v = shared.v.lock().unwrap();
        v.accounts = vec![sim_account()];
        v.pending.push_back(BrokerEvent::ExternalFill {
            account: sim.clone(),
            contract: es(),
            side: Side::Buy,
            price: Price(600_000),
            qty: Qty(2),
            fee: Money::ZERO,
            at: now,
            trade_id: "M1".into(),
        });
    }
    set_position(&shared, 2);
    assert!(until(&t, |t| t
        .cache
        .read()
        .position(&sim, es())
        .is_some_and(|p| p.qty == 2)));
    // Then sells 2.
    shared
        .v
        .lock()
        .unwrap()
        .pending
        .push_back(BrokerEvent::ExternalFill {
            account: sim.clone(),
            contract: es(),
            side: Side::Sell,
            price: Price(600_100),
            qty: Qty(2),
            fee: Money::ZERO,
            at: UtcMillis(now.0 + 60_000),
            trade_id: "M2".into(),
        });
    set_position(&shared, 0);
    assert!(until(&t, |t| {
        t.journal.sync();
        t.journal
            .snapshot()
            .is_some_and(|s| s.trades.iter().any(|tr| tr.account == sim))
    }));
    let snap = t.journal.snapshot().unwrap();
    let trades: Vec<_> = snap.trades.iter().filter(|tr| tr.account == sim).collect();
    assert_eq!(trades.len(), 1);
    assert_eq!(
        trades[0].side,
        Side::Buy,
        "a long round trip, no phantom short"
    );
    assert_eq!(snap.open, 0);
}

/// In a replay, a venue account's journal trades take no MAE/MFE from the replay's prices.
#[test]
fn replay_prices_never_reach_venue_trades_in_the_journal() {
    let shared = Arc::new(Shared {
        v: Mutex::new(Venue {
            accounts: vec![sim_account()],
            rev: 1,
            ..Default::default()
        }),
        requests: AtomicUsize::new(0),
    });
    let t = start_with(&shared, false);
    let sim = AccountId::new("venue-sim");
    assert!(until(&t, |t| t.account(&sim).is_some()));
    let now = lines_feed::now_utc();
    let fill = |side, px, id: &str, dt| BrokerEvent::ExternalFill {
        account: sim.clone(),
        contract: es(),
        side,
        price: Price(px),
        qty: Qty(1),
        fee: Money::ZERO,
        at: UtcMillis(now.0 + dt),
        trade_id: id.into(),
    };
    shared
        .v
        .lock()
        .unwrap()
        .pending
        .push_back(fill(Side::Buy, 600_000, "R1", 0));
    assert!(until(&t, |t| {
        t.journal.sync();
        t.journal.snapshot().is_some_and(|s| s.open == 1)
    }));
    // The replay prints 10 points under the entry (the paper core follows it).
    t.follow(&store_at(599_000));
    t.settle();
    shared
        .v
        .lock()
        .unwrap()
        .pending
        .push_back(fill(Side::Sell, 600_100, "R2", 60_000));
    assert!(until(&t, |t| {
        t.journal.sync();
        t.journal
            .snapshot()
            .is_some_and(|s| s.trades.iter().any(|tr| tr.account == sim))
    }));
    let snap = t.journal.snapshot().unwrap();
    let tr = snap.trades.iter().find(|tr| tr.account == sim).unwrap();
    assert_eq!(tr.mae, lines_trading::Money::ZERO);
}
