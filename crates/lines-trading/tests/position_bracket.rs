//! Position brackets: one stop and one target for the whole manual position on an account and
//! contract (`BracketMode::Position`), with the paper broker, scripted books, trades and time.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::persist;
use lines_trading::service::trading_file;
use lines_trading::{
    AccountConfig, AccountId, BracketMode, BracketSpec, Broker, BrokerCaps, Command, Event,
    FillModel, MarketInput, Money, OrderId, OrderKind, OrderSpec, PaperBroker, PaperConfig,
    RiskLimits, Side, Source, TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

/// ES price units: 6000.25 → 600025.
fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

fn paper() -> PaperConfig {
    PaperConfig {
        latency_ms: LAT,
        fill_model: FillModel::TradeThrough,
        ..Default::default()
    }
}

/// 8 ticks (2.00) to the stop, 16 (4.00) to the target.
const MODE: BracketMode = BracketMode::Position {
    stop_ticks: 8,
    target_ticks: 16,
};

/// The paper broker, but it can drop modify requests (a venue that loses one) or lack OCO.
struct Wrapped {
    inner: PaperBroker,
    drop_modifies: Arc<AtomicUsize>,
    oco: bool,
}

impl Broker for Wrapped {
    fn name(&self) -> &str {
        "wrapped"
    }
    fn capabilities(&self) -> BrokerCaps {
        let mut c = self.inner.capabilities();
        c.oco = self.oco;
        c
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        if matches!(req, BrokerRequest::Modify { .. })
            && self
                .drop_modifies
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
        {
            return;
        }
        self.inner.request(now, req)
    }
    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
        self.inner.market(now, contract, input)
    }
    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>) {
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
    drop_modifies: Arc<AtomicUsize>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(AccountConfig::paper("paper-1", 50_000), true)
    }

    fn with(account: AccountConfig, oco: bool) -> Rig {
        let mut inner = PaperBroker::new(paper());
        inner.open_account(acct(), Money::whole(50_000));
        let drop_modifies = Arc::new(AtomicUsize::new(0));
        let mut core = TradingCore::new(Box::new(Wrapped {
            inner,
            drop_modifies: drop_modifies.clone(),
            oco,
        }));
        core.add_account(account, None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
            drop_modifies,
        };
        r.book(&[(6000.00, 50)], &[(6000.25, 50)]);
        r
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let id = self
            .core
            .submit(self.now(), Source::Test, Instant::now(), c);
        self.collect();
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("every command has a result")
    }

    fn mode(
        &mut self,
        mode: BracketMode,
        replace: bool,
    ) -> Result<Vec<OrderId>, lines_trading::Reject> {
        self.cmd(Command::SetBracketMode {
            account: acct(),
            contract: es(),
            mode,
            replace,
        })
    }

    fn order(&mut self, side: Side, qty: i64, kind: OrderKind) -> OrderId {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        }))
        .expect("accepted")[0]
    }

    /// A market order, filled (at the ask for a buy, the bid for a sell).
    fn market_order(&mut self, side: Side, qty: i64) {
        self.order(side, qty, OrderKind::Market);
        self.wait(LAT);
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn market(&mut self, inputs: &[MarketInput]) {
        self.t += 1;
        self.core.market(self.now(), es(), inputs);
        self.collect();
    }

    fn book(&mut self, bids: &[(f64, i64)], asks: &[(f64, i64)]) {
        let mut v = vec![MarketInput::BookClear];
        for &(p, q) in bids {
            v.push(MarketInput::Book {
                side: BookSide::Bid,
                price: px(p),
                qty: Qty(q),
            });
        }
        for &(p, q) in asks {
            v.push(MarketInput::Book {
                side: BookSide::Ask,
                price: px(p),
                qty: Qty(q),
            });
        }
        self.market(&v);
    }

    fn trade(&mut self, p: f64, q: i64) {
        let time = UtcMillis(self.t + 1);
        self.market(&[MarketInput::Trade {
            time,
            price: px(p),
            qty: Qty(q),
        }]);
    }

    fn pos(&self) -> i64 {
        self.core.position(&acct(), es())
    }

    /// The open exits on ES: (side, qty, kind), stops first.
    fn exits(&self) -> Vec<(Side, i64, OrderKind)> {
        let mut v: Vec<(Side, i64, OrderKind)> = self
            .core
            .open_orders(&acct())
            .filter(|o| o.contract == es() && o.reduce_only)
            .map(|o| (o.side, o.qty.0 - o.filled.0, o.kind))
            .collect();
        v.sort_by_key(|(_, _, k)| !matches!(k, OrderKind::Stop { .. }));
        v
    }

    fn exit_ids(&self) -> Vec<OrderId> {
        self.core
            .open_orders(&acct())
            .filter(|o| o.contract == es() && (o.reduce_only || o.parent.is_some()))
            .map(|o| o.id)
            .collect()
    }
}

fn stop(p: f64) -> OrderKind {
    OrderKind::Stop { trigger: px(p) }
}

fn limit(p: f64) -> OrderKind {
    OrderKind::Limit { price: px(p) }
}

#[test]
fn a_position_opens_with_one_pair_at_the_offsets_from_its_average() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    assert_eq!(r.core.bracket_mode(&acct(), es()), MODE);
    r.market_order(Side::Buy, 2);
    assert_eq!(r.pos(), 2);
    r.wait(LAT);
    // Bought 2 at 6000.25: stop 2.00 under, target 4.00 over, the whole position, one pair.
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 2, stop(5998.25)),
            (Side::Sell, 2, limit(6004.25))
        ]
    );
    let ids = r.exit_ids();
    let a = r.core.order(ids[0]).unwrap().clone();
    assert!(a.oco.is_some_and(|o| ids.contains(&o)), "one-cancels-other");
    // The target fills: flat, the stop is gone.
    r.book(&[(6004.50, 50)], &[(6004.75, 50)]);
    r.trade(6004.50, 2);
    r.wait(LAT);
    assert_eq!(r.pos(), 0);
    assert!(r.exits().is_empty());
}

#[test]
fn an_off_grid_average_rounds_both_prices_toward_the_entry() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    // 1 at 6000.25 and 1 at 6000.50: average 6000.375.
    r.market_order(Side::Buy, 1);
    r.book(&[(6000.25, 50)], &[(6000.50, 50)]);
    // Off the mode while the second fills, so the pair is placed for the average of both.
    r.mode(BracketMode::PerEntry, false).unwrap();
    for id in r.exit_ids() {
        r.cmd(Command::CancelOrder {
            account: acct(),
            order: id,
        })
        .unwrap();
    }
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    r.mode(MODE, false).unwrap();
    r.wait(LAT);
    // 6000.375 - 2.00 = 5998.375 → 5998.50 (a tighter stop); + 4.00 = 6004.375 → 6004.25.
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 2, stop(5998.50)),
            (Side::Sell, 2, limit(6004.25))
        ]
    );
}

#[test]
fn an_add_resizes_both_legs_and_keeps_a_moved_price() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    // The trader moves the stop up.
    let s = r.exit_ids()[0];
    let s = if matches!(r.core.order(s).unwrap().kind, OrderKind::Stop { .. }) {
        s
    } else {
        r.exit_ids()[1]
    };
    r.cmd(Command::ModifyOrder {
        account: acct(),
        order: s,
        qty: None,
        price: Some(px(5999.00)),
    })
    .unwrap();
    r.wait(LAT);
    r.market_order(Side::Buy, 2);
    r.wait(LAT);
    assert_eq!(r.pos(), 3);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 3, stop(5999.00)),
            (Side::Sell, 3, limit(6004.25))
        ],
        "both legs follow the size; the moved stop stays where it was put"
    );
}

#[test]
fn a_reduce_shrinks_both_legs() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 3);
    r.wait(LAT);
    r.market_order(Side::Sell, 1);
    r.wait(LAT);
    assert_eq!(r.pos(), 2);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 2, stop(5998.25)),
            (Side::Sell, 2, limit(6004.25))
        ]
    );
}

#[test]
fn flat_cancels_the_pair_and_a_flip_gets_a_new_one() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 2);
    r.wait(LAT);
    let old = r.exit_ids();
    assert_eq!(old.len(), 2);
    // Flat.
    r.market_order(Side::Sell, 2);
    r.wait(LAT);
    assert_eq!(r.pos(), 0);
    assert!(r.exits().is_empty());
    for id in &old {
        assert!(!r.core.order(*id).unwrap().is_open());
    }
    // Long 1, then a sell of 3 flips to short 2 (sold at the bid, 6000.00).
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    r.market_order(Side::Sell, 3);
    r.wait(LAT);
    r.wait(LAT);
    assert_eq!(r.pos(), -2);
    assert_eq!(
        r.exits(),
        [
            (Side::Buy, 2, stop(6002.00)),
            (Side::Buy, 2, limit(5996.00))
        ],
        "a buy pair for the short, from its own average"
    );
}

#[test]
fn partial_fills_are_followed_exactly() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.order(Side::Buy, 3, limit(5999.00));
    r.wait(LAT);
    r.trade(5998.75, 1); // through: 1 of 3
    r.wait(LAT);
    assert_eq!(r.pos(), 1);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 1, stop(5997.00)),
            (Side::Sell, 1, limit(6003.00))
        ]
    );
    r.trade(5998.75, 1); // 2 of 3
    r.wait(LAT);
    assert_eq!(r.exits()[0].1, 2);
    assert_eq!(r.exits()[1].1, 2);
    r.trade(5998.75, 5); // the rest
    r.wait(LAT);
    assert_eq!(r.pos(), 3);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 3, stop(5997.00)),
            (Side::Sell, 3, limit(6003.00))
        ],
        "prices from the first fill, sizes from the position"
    );
}

#[test]
fn a_resize_racing_an_add_ends_at_the_position() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    // An add of 2 fills: the resize to 3 is on its way (the core still sees the legs at 1)
    // when a sell of 1 fills, so the position is 2 before the broker answers.
    r.order(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.pos(), 3);
    assert_eq!(r.exits()[0].1, 1, "the resize isn't answered yet");
    r.wait(LAT / 4);
    r.order(Side::Sell, 1, OrderKind::Market);
    r.wait(LAT);
    r.wait(LAT);
    r.wait(LAT);
    assert_eq!(r.pos(), 2);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 2, stop(5998.25)),
            (Side::Sell, 2, limit(6004.25))
        ],
        "settled at the position, never left larger"
    );
}

#[test]
fn switching_with_exits_working_needs_a_confirmation_naming_them() {
    let mut r = Rig::new();
    // Two entries, each with its own bracket.
    for _ in 0..2 {
        r.cmd(Command::PlaceBracket(BracketSpec {
            entry: OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(1),
                kind: OrderKind::Market,
            },
            take_profit: Some(px(6010.00)),
            stop_loss: Some(px(5990.00)),
        }))
        .unwrap();
        r.wait(LAT);
    }
    r.wait(LAT);
    assert_eq!(r.pos(), 2);
    assert_eq!(r.exit_ids().len(), 4);
    assert_eq!(r.core.exits_replaced_by_position_bracket(&acct(), es()), 4);
    let no = r.mode(MODE, false).unwrap_err();
    assert!(
        no.message.contains("cancels 4 working exit orders"),
        "{}",
        no.message
    );
    assert_eq!(r.core.bracket_mode(&acct(), es()), BracketMode::PerEntry);
    // Confirmed: the four go, then one pair for the position.
    let cancelled = r.mode(MODE, true).unwrap();
    assert_eq!(cancelled.len(), 4);
    r.wait(LAT);
    r.wait(LAT);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 2, stop(5998.25)),
            (Side::Sell, 2, limit(6004.25))
        ]
    );
    assert_eq!(r.exit_ids().len(), 2, "never more exits than the pair");
}

#[test]
fn entries_carry_no_exits_and_a_second_exit_is_refused() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    let ids = r
        .cmd(Command::PlaceBracket(BracketSpec {
            entry: OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(1),
                kind: OrderKind::Market,
            },
            take_profit: Some(px(6010.00)),
            stop_loss: Some(px(5990.00)),
        }))
        .unwrap();
    assert_eq!(ids.len(), 1, "the entry alone");
    r.wait(LAT);
    r.wait(LAT);
    assert_eq!(r.exit_ids().len(), 2, "the position's pair only");
    let no = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            kind: stop(5995.00),
        }))
        .unwrap_err();
    assert!(
        no.message.contains("move its stop or target"),
        "{}",
        no.message
    );
}

#[test]
fn back_to_per_entry_leaves_the_pair_to_the_trader() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    r.mode(BracketMode::PerEntry, false).unwrap();
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    assert_eq!(r.pos(), 2);
    assert_eq!(r.exits()[0].1, 1, "no longer resized");
}

/// The app starts again: the broker still holds the pair; setting the mode again (from the
/// saved setting) adopts it, places nothing new, and it keeps following the position. A
/// position with no exits gets a pair at once.
#[test]
fn a_restart_adopts_the_pair_and_keeps_following_the_position() {
    let dir = std::env::temp_dir().join(format!("lines-pb-{}", std::process::id()));
    let path = dir.join("paper.json");
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 2);
    r.wait(LAT);
    let pair = r.exit_ids();
    assert_eq!(pair.len(), 2);
    let file = trading_file(&r.core, &paper()).unwrap();
    persist::save(&path, &file).unwrap();
    drop(r);

    let restart = |path: &std::path::Path| {
        let file = persist::load(path).unwrap().unwrap();
        let broker = PaperBroker::from_state(file.paper.clone(), file.broker.clone());
        let mut core = TradingCore::new(Box::new(broker));
        for a in file.accounts {
            core.add_account(a.config, Some(a.tracker));
        }
        core.set_next_order_id(file.next_order_id);
        let mut r = Rig {
            core,
            t: UtcMillis::parse_iso("2026-11-10T15:30:00Z").unwrap().0,
            events: Vec::new(),
            drop_modifies: Arc::new(AtomicUsize::new(0)),
        };
        r.core.rebuild(r.now());
        r.collect();
        r.book(&[(6000.00, 50)], &[(6000.25, 50)]);
        r
    };
    let mut r2 = restart(&path);
    assert_eq!(r2.pos(), 2);
    assert_eq!(r2.core.exits_replaced_by_position_bracket(&acct(), es()), 0);
    assert_eq!(r2.mode(MODE, false).unwrap(), Vec::<OrderId>::new());
    r2.wait(LAT);
    let mut now = r2.exit_ids();
    now.sort();
    let mut was = pair.clone();
    was.sort();
    assert_eq!(now, was, "the same pair, nothing new placed");
    r2.market_order(Side::Buy, 1);
    r2.wait(LAT);
    assert_eq!(r2.exits()[0].1, 3);
    assert_eq!(r2.exits()[1].1, 3);

    // A position whose exits were lost: setting the mode places a pair.
    for id in r2.exit_ids() {
        r2.cmd(Command::CancelOrder {
            account: acct(),
            order: id,
        })
        .unwrap();
    }
    r2.mode(BracketMode::PerEntry, false).unwrap();
    r2.wait(LAT);
    assert!(r2.exits().is_empty());
    r2.mode(MODE, false).unwrap();
    r2.wait(LAT);
    assert_eq!(r2.exits().len(), 2);
    assert_eq!(r2.exits()[0].1, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A guardrail lockout refuses entries, never protection: a resting add that fills during the
/// lockout still has both legs resized to the position.
#[test]
fn a_lockout_never_holds_back_a_resize() {
    let mut r = Rig::new();
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: lines_trading::guardrails::Guardrails {
            max_daily_loss: Some(200),
            ..Default::default()
        },
    })
    .unwrap();
    // A wide stop (10.00), so the drop below doesn't reach it.
    r.mode(
        BracketMode::Position {
            stop_ticks: 40,
            target_ticks: 16,
        },
        false,
    )
    .unwrap();
    r.market_order(Side::Buy, 3);
    r.wait(LAT);
    r.order(Side::Buy, 1, limit(5997.00));
    r.wait(LAT);
    // Down 2.25 on 3: $337.50 > $200, locked.
    r.book(&[(5998.00, 50)], &[(5998.25, 50)]);
    r.market(&[MarketInput::Last(px(5998.00))]);
    let locked = r.events.iter().any(|e| {
        matches!(
            e,
            Event::Guardrail {
                state: lines_trading::GuardState::Locked,
                ..
            }
        )
    });
    assert!(locked, "the daily loss locked the account");
    // The resting add fills during the lockout.
    r.trade(5996.75, 5);
    r.wait(LAT);
    r.wait(LAT);
    assert_eq!(r.pos(), 4);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 4, stop(5990.25)),
            (Side::Sell, 4, limit(6004.25))
        ]
    );
}

/// Strategies keep their own exits: the mode is refused while one trades the account and
/// contract.
#[test]
fn a_strategy_on_the_contract_keeps_its_own_exits() {
    use lines_trading::{HoursWindow, StrategyConfig, StrategyId, StrategyLimits, TradingHours};
    let mut r = Rig::new();
    r.cmd(Command::AddStrategy(StrategyConfig {
        id: StrategyId(7),
        label: "example #7".into(),
        account: acct(),
        contract: es(),
        limits: StrategyLimits {
            max_position: 2,
            max_daily_loss: 1_000,
            max_orders_per_min: 5,
            hours: TradingHours {
                tz: "America/New_York".into(),
                windows: vec![HoursWindow::new("09:30", "16:00")],
            },
        },
    }))
    .unwrap();
    let no = r.mode(MODE, false).unwrap_err();
    assert!(
        no.message.contains("its exits are its own"),
        "{}",
        no.message
    );
    assert_eq!(r.core.bracket_mode(&acct(), es()), BracketMode::PerEntry);
}

/// Cancel All takes the pair; the trader's own exit is then accepted, and it becomes the leg
/// the position follows.
#[test]
fn after_cancel_all_a_traders_own_exit_becomes_the_leg() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 2);
    r.wait(LAT);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(LAT);
    assert!(
        r.exits().is_empty(),
        "nothing put back over the trader's cancel"
    );
    r.wait(2_000);
    assert!(r.exits().is_empty());
    r.cmd(Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(2),
        kind: stop(5995.00),
    }))
    .expect("an exit of their own is accepted");
    r.wait(LAT);
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    assert_eq!(r.exits(), [(Side::Sell, 3, stop(5995.00))]);
}

/// A leg the core refuses (here the submit rate: the entry and the stop use the second's two
/// submits) is tried again a second later.
#[test]
fn a_refused_leg_is_tried_again() {
    let mut cfg = AccountConfig::paper("paper-1", 50_000);
    cfg.limits = RiskLimits {
        max_submits_per_sec: Some(2),
        ..Default::default()
    };
    let mut r = Rig::with(cfg, true);
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    assert_eq!(
        r.exits(),
        [(Side::Sell, 1, stop(5998.25))],
        "the target was refused"
    );
    r.wait(1_000);
    r.wait(LAT);
    assert_eq!(
        r.exits(),
        [
            (Side::Sell, 1, stop(5998.25)),
            (Side::Sell, 1, limit(6004.25))
        ]
    );
}

/// The broker loses a resize: the legs are still short of the position, so they are asked
/// again.
#[test]
fn a_lost_resize_is_sent_again() {
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    r.market_order(Side::Buy, 1);
    r.wait(LAT);
    r.drop_modifies.store(2, Ordering::Relaxed);
    r.market_order(Side::Buy, 2);
    r.wait(LAT);
    assert_eq!(r.exits()[0].1, 1, "both resizes lost");
    r.wait(1_000);
    r.wait(LAT);
    assert_eq!(r.exits()[0].1, 3);
    assert_eq!(r.exits()[1].1, 3);
}

/// Switching on with a position whose target the market is already past is refused: the pair
/// would close it at once.
#[test]
fn a_pair_that_would_fill_at_once_is_not_placed() {
    let mut r = Rig::new();
    r.market_order(Side::Buy, 2);
    r.book(&[(6010.00, 50)], &[(6010.25, 50)]);
    r.market(&[MarketInput::Bid(px(6010.00)), MarketInput::Ask(px(6010.25))]);
    let no = r.mode(MODE, false).unwrap_err();
    assert!(
        no.message.contains("past the bracket's target"),
        "{}",
        no.message
    );
    assert_eq!(r.pos(), 2);
    assert!(r.exits().is_empty());
}

#[test]
fn a_broker_without_oco_cant_hold_one() {
    let mut r = Rig::with(AccountConfig::paper("paper-1", 50_000), false);
    let no = r.mode(MODE, false).unwrap_err();
    assert!(
        no.message.contains("can't hold a position bracket"),
        "{}",
        no.message
    );
}

/// A strategy can't be added where the position bracket is on (its exits are its own).
#[test]
fn a_strategy_cant_join_a_position_bracket() {
    use lines_trading::{HoursWindow, StrategyConfig, StrategyId, StrategyLimits, TradingHours};
    let mut r = Rig::new();
    r.mode(MODE, false).unwrap();
    let no = r
        .cmd(Command::AddStrategy(StrategyConfig {
            id: StrategyId(7),
            label: "example #7".into(),
            account: acct(),
            contract: es(),
            limits: StrategyLimits {
                max_position: 2,
                max_daily_loss: 1_000,
                max_orders_per_min: 5,
                hours: TradingHours {
                    tz: "America/New_York".into(),
                    windows: vec![HoursWindow::new("09:30", "16:00")],
                },
            },
        }))
        .unwrap_err();
    assert!(
        no.message.contains("switch it back to per entry"),
        "{}",
        no.message
    );
}
