//! The strategy gate (decision 0025): strategy commands through the
//! trading core with the paper broker. Deterministic: time is passed in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::guardrails::Guardrails;
use lines_trading::{
    AccountConfig, AccountId, BracketSpec, Broker, BrokerCaps, Command, Event, FillModel,
    HoursWindow, MarketInput, Money, OrderId, OrderKind, OrderSpec, OrderState, PaperBroker,
    PaperConfig, Reject, RejectCode, Side, Source, StrategyConfig, StrategyId, StrategyLimits,
    StrategyState, TradingCore, TradingHours,
};

const ID: StrategyId = StrategyId(7);
const BOT: Source = Source::Strategy(ID);

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn nq() -> ContractKey {
    ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

fn limits() -> StrategyLimits {
    StrategyLimits {
        max_position: 2,
        max_daily_loss: 1_000,
        max_orders_per_min: 5,
        hours: TradingHours {
            tz: "America/New_York".into(),
            windows: vec![HoursWindow::new("09:30", "16:00")],
        },
    }
}

fn config() -> StrategyConfig {
    StrategyConfig {
        id: ID,
        label: "example #7".into(),
        account: acct(),
        contract: es(),
        limits: limits(),
    }
}

/// The paper broker, but it can be made not simulated (a live broker) or disconnected.
struct Wrapped {
    inner: PaperBroker,
    simulated: bool,
    up: Arc<AtomicBool>,
}

impl Broker for Wrapped {
    fn name(&self) -> &str {
        "wrapped"
    }
    fn capabilities(&self) -> BrokerCaps {
        self.inner.capabilities()
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
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
    fn connected(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }
    fn simulated(&self) -> bool {
        self.simulated
    }
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
    up: Arc<AtomicBool>,
}

impl Rig {
    fn with(simulated: bool, account: AccountConfig) -> Rig {
        let cfg = PaperConfig {
            latency_ms: 20,
            fill_model: FillModel::TradeThrough,
            ..Default::default()
        };
        let mut inner = PaperBroker::new(cfg);
        inner.open_account(account.id.clone(), Money::whole(account.starting_balance));
        let up = Arc::new(AtomicBool::new(true));
        let mut core = TradingCore::new(Box::new(Wrapped {
            inner,
            simulated,
            up: up.clone(),
        }));
        core.add_account(account, None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC = 10:00 New York: CME open, inside the hours.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
            up,
        };
        r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
        r
    }

    /// A paper rig with the example instance added and armed.
    fn armed() -> Rig {
        let mut r = Rig::with(true, AccountConfig::paper("paper-1", 50_000));
        r.manual(Command::AddStrategy(config())).unwrap();
        r.manual(Command::ArmStrategy { id: ID }).unwrap();
        assert_eq!(r.state(), StrategyState::Armed);
        r
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn send(&mut self, source: Source, c: Command) -> Result<Vec<OrderId>, Reject> {
        let id = self.core.submit(self.now(), source, Instant::now(), c);
        self.events.extend(self.core.drain());
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("every command has a result")
    }

    fn manual(&mut self, c: Command) -> Result<Vec<OrderId>, Reject> {
        self.send(Source::Dom, c)
    }

    fn bot(&mut self, c: Command) -> Result<Vec<OrderId>, Reject> {
        // The host is alive whenever its strategy acts.
        self.send(BOT, c)
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.events.extend(self.core.drain());
    }

    fn market(&mut self, inputs: &[MarketInput]) {
        self.t += 1;
        self.core.market(self.now(), es(), inputs);
        self.events.extend(self.core.drain());
    }

    fn book(&mut self, bids: &[(f64, i64)], asks: &[(f64, i64)]) {
        let mut v = vec![MarketInput::BookClear];
        v.extend(bids.iter().map(|&(p, q)| MarketInput::Book {
            side: BookSide::Bid,
            price: px(p),
            qty: Qty(q),
        }));
        v.extend(asks.iter().map(|&(p, q)| MarketInput::Book {
            side: BookSide::Ask,
            price: px(p),
            qty: Qty(q),
        }));
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

    fn state(&self) -> StrategyState {
        self.core.strategy_state(ID).expect("added").0
    }

    fn reason(&self) -> String {
        self.core
            .strategy_state(ID)
            .and_then(|(_, r)| r.map(str::to_owned))
            .unwrap_or_default()
    }

    fn order_state(&self, id: OrderId) -> OrderState {
        self.core.order(id).expect("known").state.clone()
    }
}

fn buy(contract: ContractKey, qty: i64, kind: OrderKind) -> Command {
    Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract,
        side: Side::Buy,
        qty: Qty(qty),
        kind,
    })
}

fn limit(p: f64) -> OrderKind {
    OrderKind::Limit { price: px(p) }
}

fn bracket(qty: i64, at: f64) -> Command {
    Command::PlaceBracket(BracketSpec {
        entry: OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(qty),
            kind: limit(at),
        },
        take_profit: Some(px(at + 10.0)),
        stop_loss: Some(px(at - 10.0)),
    })
}

fn denied(r: Result<Vec<OrderId>, Reject>) -> String {
    match r {
        Err(Reject {
            code: RejectCode::Strategy,
            message,
        }) => message,
        other => panic!("expected a strategy denial, got {other:?}"),
    }
}

#[test]
fn only_an_armed_instance_enters() {
    let mut r = Rig::with(true, AccountConfig::paper("paper-1", 50_000));
    // Unknown instance.
    assert!(denied(r.bot(buy(es(), 1, limit(5990.0)))).contains("no strategy 7"));
    r.manual(Command::AddStrategy(config())).unwrap();
    assert_eq!(r.state(), StrategyState::Disarmed);
    assert!(denied(r.bot(buy(es(), 1, limit(5990.0)))).contains("is disarmed"));
    r.manual(Command::ArmStrategy { id: ID }).unwrap();
    let ids = r.bot(buy(es(), 1, limit(5990.0))).expect("armed enters");
    r.wait(50);
    assert_eq!(r.order_state(ids[0]), OrderState::Working);
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Strategy {
            id: ID,
            state: StrategyState::Armed,
            ..
        }
    )));
}

#[test]
fn only_a_person_arms() {
    let mut r = Rig::with(true, AccountConfig::paper("paper-1", 50_000));
    r.manual(Command::AddStrategy(config())).unwrap();
    for source in [BOT, Source::Rule, Source::Api] {
        assert!(denied(r.send(source, Command::ArmStrategy { id: ID })).contains("only you"));
    }
    assert!(denied(r.bot(Command::AddStrategy(config()))).contains("can't add"));
    assert!(denied(r.bot(Command::RemoveStrategy { id: ID })).contains("can't remove"));
    assert_eq!(r.state(), StrategyState::Disarmed);
    r.send(Source::Palette, Command::ArmStrategy { id: ID })
        .unwrap();
    assert_eq!(r.state(), StrategyState::Armed);
}

#[test]
fn paper_only_until_live_is_enabled() {
    let mut r = Rig::with(false, AccountConfig::paper("paper-1", 50_000));
    r.manual(Command::AddStrategy(config())).unwrap();
    assert!(denied(r.manual(Command::ArmStrategy { id: ID })).contains("paper only"));
    assert!(denied(r.bot(buy(es(), 1, limit(5990.0)))).contains("paper only"));
    // A person still trades the live account as before.
    r.manual(buy(es(), 1, limit(5990.0))).unwrap();
}

#[test]
fn an_instance_trades_only_its_account_contract_and_orders() {
    let mut r = Rig::armed();
    assert!(denied(r.bot(buy(nq(), 1, limit(21000.0)))).contains("own account and contract"));
    let theirs = r.manual(buy(es(), 1, limit(5980.0))).unwrap()[0];
    // That manual order paused it: arm again to check scope alone.
    r.manual(Command::ArmStrategy { id: ID }).unwrap();
    assert!(
        denied(r.bot(Command::CancelOrder {
            account: acct(),
            order: theirs,
        }))
        .contains("only its own orders")
    );
    assert!(
        denied(r.bot(Command::Flatten {
            account: acct(),
            contract: None,
        }))
        .contains("only its own contract")
    );
    assert!(
        denied(r.bot(Command::SetGuardrails {
            account: acct(),
            guardrails: Guardrails::default(),
        }))
        .contains("can't send")
    );
    r.bot(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .expect("its own contract");
}

#[test]
fn one_instance_per_account_and_contract() {
    let mut r = Rig::armed();
    let mut other = config();
    other.id = StrategyId(8);
    assert!(denied(r.manual(Command::AddStrategy(other))).contains("already trades"));
    // An armed instance can't be replaced underneath itself.
    assert!(denied(r.manual(Command::AddStrategy(config()))).contains("is armed"));
    let mut bad = config();
    bad.limits.max_position = 0;
    r.manual(Command::PauseStrategy {
        id: ID,
        reason: "edit".into(),
    })
    .unwrap();
    assert!(denied(r.manual(Command::AddStrategy(bad))).contains("max position"));
}

#[test]
fn max_position_counts_open_orders() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, limit(5990.0))).expect("2 of 2");
    let m = denied(r.bot(buy(es(), 1, limit(5989.0))));
    assert!(
        m.contains("would hold up to 3, over its max position of 2"),
        "{m}"
    );
    // A sell that would net against the open buy is not exposure.
    r.bot(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(2),
        kind: limit(6010.0),
    }))
    .expect("worst case is still 2");
}

#[test]
fn orders_per_minute_but_cancels_always() {
    let mut r = Rig::armed();
    let mut ids = Vec::new();
    for i in 0..5 {
        let at = 5990.0 - i as f64;
        let id = r.bot(buy(es(), 1, limit(at))).expect("within rate")[0];
        ids.push(id);
        r.bot(Command::CancelOrder {
            account: acct(),
            order: id,
        })
        .expect("cancels never limited");
        r.wait(30);
    }
    assert!(denied(r.bot(buy(es(), 1, limit(5980.0)))).contains("more than 5 orders in a minute"));
    r.wait(60_000);
    r.bot(buy(es(), 1, limit(5980.0))).expect("a minute later");
}

#[test]
fn outside_allowed_hours_no_entries_but_exits() {
    let mut r = Rig::armed();
    let id = r.bot(buy(es(), 1, limit(5990.0))).unwrap()[0];
    // 16:30 New York.
    r.t = UtcMillis::parse_iso("2026-11-10T21:30:00Z").unwrap().0;
    assert!(denied(r.bot(buy(es(), 1, limit(5989.0)))).contains("outside its allowed hours"));
    r.bot(Command::CancelOrder {
        account: acct(),
        order: id,
    })
    .expect("exits are never limited by time");
}

#[test]
fn daily_loss_closes_the_trade_and_pauses_for_the_trade_date() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 2);
    // A protective stop of its own, below the loss limit.
    let stop = r
        .bot(sell(
            2,
            OrderKind::Stop {
                trigger: px(5970.0),
            },
        ))
        .unwrap()[0];
    r.wait(30);
    // $50/pt x 2 contracts: 10 points down is $1,000, the limit.
    r.book(&[(5990.0, 10)], &[(5990.25, 10)]);
    r.trade(5990.0, 1);
    assert_eq!(r.state(), StrategyState::Paused);
    assert_eq!(
        r.reason(),
        lines_trading::DAILY_LOSS,
        "the exact text the panel matches"
    );
    r.wait(30);
    // The owner's rule: the trade is closed and its working orders are gone.
    assert_eq!(r.core.position(&acct(), es()), 0, "flattened");
    assert!(!r.core.order(stop).unwrap().is_open(), "its stop cancelled");
    // No entries for the rest of the trade date.
    assert!(
        denied(r.manual(Command::ArmStrategy { id: ID }))
            .contains(lines_trading::LOSS_CLOSED_TODAY)
    );
    // The next trade date (Wednesday 10:00 New York) starts over.
    r.t = UtcMillis::parse_iso("2026-11-11T15:00:00Z").unwrap().0;
    r.wait(10);
    r.manual(Command::ArmStrategy { id: ID })
        .expect("a new trade date");
}

#[test]
fn manual_activity_pauses_cancels_entries_keeps_exits() {
    let mut r = Rig::armed();
    // A filled bracket (a marketable limit: position 1, exits working) and a resting entry.
    let b = r.bot(bracket(1, 6000.25)).unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 1);
    let entry = r.bot(buy(es(), 1, limit(5985.0))).unwrap()[0];
    r.wait(30);
    // A person touches the same account and contract.
    r.manual(buy(es(), 1, limit(5950.0))).unwrap();
    r.wait(30);
    assert_eq!(r.state(), StrategyState::Paused);
    assert!(
        r.reason().contains("manual order activity on"),
        "{}",
        r.reason()
    );
    assert_eq!(r.order_state(entry), OrderState::Cancelled);
    // Protective exits stay while the position is open.
    for id in &b[1..] {
        let o = r.core.order(*id).expect("known");
        assert!(o.is_open(), "{:?} {:?}", o.role, o.state);
    }
    // No automatic resume.
    r.wait(10_000);
    assert_eq!(r.state(), StrategyState::Paused);
    assert!(denied(r.bot(buy(es(), 1, limit(5980.0)))).contains("is paused: manual order"));
    // A person re-arms.
    r.manual(Command::ArmStrategy { id: ID }).unwrap();
    assert_eq!(r.state(), StrategyState::Armed);
}

#[test]
fn manual_on_another_contract_does_not_pause() {
    let mut r = Rig::armed();
    r.manual(buy(nq(), 1, limit(21000.0))).unwrap();
    assert_eq!(r.state(), StrategyState::Armed);
}

#[test]
fn stop_all_is_immediate_and_optionally_flattens() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 1, OrderKind::Market)).unwrap();
    r.wait(30);
    let resting = r.bot(buy(es(), 1, limit(5985.0))).unwrap()[0];
    // Stop without flatten: entries cancelled, position stays the trader's.
    r.manual(Command::StopStrategies {
        id: None,
        flatten: false,
    })
    .unwrap();
    assert_eq!(r.state(), StrategyState::Stopped);
    assert!(denied(r.bot(buy(es(), 1, limit(5984.0)))).contains("is stopped"));
    r.wait(30);
    assert_eq!(r.order_state(resting), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 1);
    // Stop all with flatten closes it.
    r.manual(Command::StopStrategies {
        id: None,
        flatten: true,
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

#[test]
fn stop_is_never_gated() {
    // Even with the account halted by a guardrail lockout and the broker disconnected.
    let mut r = Rig::armed();
    r.manual(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_daily_loss: Some(1),
            ..Default::default()
        },
    })
    .unwrap();
    r.up.store(false, Ordering::Relaxed);
    r.wait(10);
    r.manual(Command::StopStrategies {
        id: Some(ID),
        flatten: true,
    })
    .expect("always allowed");
    assert_eq!(r.state(), StrategyState::Stopped);
}

#[test]
fn a_strategy_stops_or_pauses_only_itself() {
    let mut r = Rig::armed();
    assert!(
        denied(r.bot(Command::StopStrategies {
            id: None,
            flatten: false,
        }))
        .contains("only itself")
    );
    assert!(
        denied(r.bot(Command::PauseStrategy {
            id: StrategyId(9),
            reason: "x".into(),
        }))
        .contains("only itself")
    );
    r.bot(Command::PauseStrategy {
        id: ID,
        reason: "levels are stale".into(),
    })
    .unwrap();
    assert_eq!(r.state(), StrategyState::Paused);
    assert_eq!(r.reason(), "levels are stale");
}

#[test]
fn silent_host_pauses() {
    let mut r = Rig::armed();
    // The trading service calls this when no heartbeat came for HEARTBEAT_TIMEOUT (wall clock).
    r.core.strategy_host_silent(r.now());
    r.events.extend(r.core.drain());
    assert_eq!(r.state(), StrategyState::Paused);
    assert!(r.reason().contains("stopped responding"), "{}", r.reason());
}

#[test]
fn disconnect_and_lockout_pause() {
    let mut r = Rig::armed();
    r.up.store(false, Ordering::Relaxed);
    r.wait(10);
    assert_eq!(r.state(), StrategyState::Paused);
    assert!(r.reason().contains("disconnected"));
    r.up.store(true, Ordering::Relaxed);
    r.manual(Command::ArmStrategy { id: ID }).unwrap();
    // A guardrail lockout on the account: one round trip a day, then a trade.
    r.manual(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_trades_per_day: Some(1),
            ..Default::default()
        },
    })
    .unwrap();
    r.bot(buy(es(), 1, OrderKind::Market)).unwrap();
    r.wait(30);
    r.bot(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.state(), StrategyState::Paused, "{}", r.reason());
}

#[test]
fn remove_stops_and_forgets() {
    let mut r = Rig::armed();
    let id = r.bot(buy(es(), 1, limit(5985.0))).unwrap()[0];
    r.manual(Command::RemoveStrategy { id: ID }).unwrap();
    r.wait(30);
    assert_eq!(r.order_state(id), OrderState::Cancelled);
    assert!(r.core.strategy_state(ID).is_none());
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Strategy {
            id: ID,
            removed: true,
            ..
        }
    )));
}

#[test]
fn saved_forms() {
    // The journal saves each fill's command source: a strategy's is tagged with its id.
    assert_eq!(serde_json::to_string(&BOT).unwrap(), r#"{"strategy":7}"#);
    assert_eq!(serde_json::to_string(&Source::Dom).unwrap(), r#""dom""#);
    let back: Source = serde_json::from_str(r#"{"strategy":7}"#).unwrap();
    assert_eq!(back, BOT);
    let c = Command::StopStrategies {
        id: None,
        flatten: true,
    };
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(json, r#"{"command":"stop_strategies","flatten":true}"#);
    assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), c);
}

#[test]
fn a_flatten_by_someone_else_closes_the_instance_books() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 2);
    // Down 5 points ($500 on 2 lots, under the $1,000 limit), then a person stops it and
    // flattens.
    r.book(&[(5995.0, 10)], &[(5995.25, 10)]);
    r.manual(Command::StopStrategies {
        id: Some(ID),
        flatten: true,
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
    // The market falls another 20 points. Had the instance kept its long on its own books,
    // that would be a $2,500 loss and arming would be refused.
    r.book(&[(5975.0, 10)], &[(5975.25, 10)]);
    r.trade(5975.0, 1);
    r.manual(Command::ArmStrategy { id: ID })
        .expect("its own loss is the $500 realised");
}

fn sell(qty: i64, kind: OrderKind) -> Command {
    Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(qty),
        kind,
    })
}

#[test]
fn exits_are_never_limited_by_hours_size_or_rate() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 2);
    // After hours, and paused: closing orders still go, one at a time up to the position.
    r.t = UtcMillis::parse_iso("2026-11-10T21:30:00Z").unwrap().0;
    r.bot(Command::PauseStrategy {
        id: ID,
        reason: "test".into(),
    })
    .unwrap();
    let stop = r
        .bot(sell(
            1,
            OrderKind::Stop {
                trigger: px(5990.0),
            },
        ))
        .expect("an exit after hours, while paused");
    r.bot(sell(1, limit(6010.0)))
        .expect("a second exit: still within the position");
    // A third would go beyond the position: it is an entry, and refused.
    assert!(denied(r.bot(sell(1, limit(6020.0)))).contains("is paused"));
    // Moving the stop many times is never rate limited.
    for i in 0..8 {
        r.bot(Command::ModifyOrder {
            account: acct(),
            order: stop[0],
            qty: None,
            price: Some(px(5990.0 + 0.25 * i as f64)),
        })
        .expect("a stop adjustment");
        r.wait(30);
    }
}

#[test]
fn one_instance_per_account_and_contract_among_many() {
    let mut r = Rig::armed();
    let nq_cfg = |id| StrategyConfig {
        id: StrategyId(id),
        label: format!("nq #{id}"),
        contract: nq(),
        ..config()
    };
    r.manual(Command::AddStrategy(nq_cfg(8))).unwrap();
    assert!(denied(r.manual(Command::AddStrategy(nq_cfg(9)))).contains("nq #8 already trades"));
}

#[test]
fn arming_reconciles_its_books_with_the_account() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 1, OrderKind::Market)).unwrap();
    r.wait(30);
    // A person adds to the position: paused, and its books (1) disagree with the account (2).
    r.manual(buy(es(), 1, OrderKind::Market)).unwrap();
    r.wait(30);
    assert_eq!(r.state(), StrategyState::Paused);
    assert!(denied(r.manual(Command::ArmStrategy { id: ID })).contains("flatten first"));
    // A person closes everything with a plain order: flat adopts flat.
    r.manual(sell(2, OrderKind::Market)).unwrap();
    r.wait(30);
    r.manual(Command::ArmStrategy { id: ID }).expect("flat");
    r.bot(buy(es(), 2, limit(5990.0)))
        .expect("its own books are flat again: the max position of 2 is free");
}

#[test]
fn removing_forgets_its_orders() {
    let mut r = Rig::armed();
    r.bot(bracket(1, 6000.25)).unwrap();
    r.wait(30);
    r.manual(Command::RemoveStrategy { id: ID }).unwrap();
    r.manual(Command::AddStrategy(config())).unwrap();
    // The old bracket's exits fill now: the new instance's books stay flat, so a person
    // flattening and arming is not refused for a mismatch.
    r.manual(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
    r.manual(Command::ArmStrategy { id: ID })
        .expect("flat books");
}

#[test]
fn a_person_closing_the_position_doesnt_count_as_its_loss() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    // A person closes with a plain order (not Flatten): paused.
    r.manual(sell(2, OrderKind::Market)).unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
    // The market falls 20 points: its stale books would read -$2,000.
    r.book(&[(5980.0, 10)], &[(5980.25, 10)]);
    r.trade(5980.0, 1);
    r.manual(Command::ArmStrategy { id: ID })
        .expect("the books adopt flat before the loss check");
}

#[test]
fn a_person_moving_its_entry_moves_it() {
    let mut r = Rig::armed();
    let entry = r.bot(buy(es(), 1, limit(5985.0))).unwrap()[0];
    r.wait(30);
    r.manual(Command::ModifyOrder {
        account: acct(),
        order: entry,
        qty: None,
        price: Some(px(5986.0)),
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.state(), StrategyState::Paused);
    let o = r.core.order(entry).unwrap();
    assert!(o.is_open(), "{:?}", o.state);
    assert_eq!(o.kind, limit(5986.0));
}

#[test]
fn its_flatten_never_takes_a_persons_orders() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 1, OrderKind::Market)).unwrap();
    r.wait(30);
    // A person's resting buy (pauses the instance), then the instance flattens its contract.
    let theirs = r.manual(buy(es(), 1, limit(5950.0))).unwrap()[0];
    r.bot(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
    // Had it taken the person's order, a later fill of it would land on its books: here
    // the books are flat and arming works.
    let _ = theirs;
    r.manual(Command::ArmStrategy { id: ID })
        .expect("flat books");
    assert!(
        denied(r.bot(Command::CancelOrder {
            account: acct(),
            order: theirs,
        }))
        .contains("only its own orders")
    );
}

#[test]
fn limits_reject_unknown_fields() {
    let json = r#"{"max_position":1,"max_daily_loss":500,"max_orders_per_min":5,
        "hours":{"tz":"America/New_York","windows":[{"start":"09:30","end":"16:00"}],
        "weekdays":["mon"]}}"#;
    assert!(serde_json::from_str::<StrategyLimits>(json).is_err());
}

#[test]
fn a_paused_instance_cant_reprice_an_entry_into_a_fill() {
    let mut r = Rig::armed();
    let entry = r.bot(buy(es(), 1, limit(5985.0))).unwrap()[0];
    r.wait(30);
    // A person moves it: paused, the order left to them but still the instance's.
    r.manual(Command::ModifyOrder {
        account: acct(),
        order: entry,
        qty: None,
        price: Some(px(5986.0)),
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.state(), StrategyState::Paused);
    // Repricing it to a marketable price is an entry: refused.
    assert!(
        denied(r.bot(Command::ModifyOrder {
            account: acct(),
            order: entry,
            qty: None,
            price: Some(px(6000.25)),
        }))
        .contains("is paused")
    );
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

#[test]
fn its_cancel_all_and_flatten_leave_the_traders_orders() {
    let mut r = Rig::armed();
    let mine = r.bot(buy(es(), 1, limit(5985.0))).unwrap()[0];
    r.wait(30);
    let theirs = r.manual(buy(es(), 1, limit(5950.0))).unwrap()[0];
    r.wait(30);
    r.bot(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.bot(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(30);
    assert!(!r.core.order(mine).unwrap().is_open());
    assert!(
        r.core.order(theirs).unwrap().is_open(),
        "the trader's order stays"
    );
}

#[test]
fn working_bracket_exits_count_as_closing() {
    let mut r = Rig::armed();
    // Long 1 with a bracket's exits working.
    r.bot(bracket(1, 6000.25)).unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 1);
    r.bot(Command::PauseStrategy {
        id: ID,
        reason: "test".into(),
    })
    .unwrap();
    // A further sell is not an exit: the bracket already closes the position.
    assert!(denied(r.bot(sell(1, limit(6010.0)))).contains("is paused"));
}

#[test]
fn repricing_counts_other_closing_orders() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 1, OrderKind::Market)).unwrap();
    r.wait(30);
    // One closing sell, then a second one: that one reverses (an entry, allowed while armed).
    r.bot(sell(1, limit(6010.0))).expect("closing");
    let second = r.bot(sell(1, limit(6020.0))).expect("armed: a reversal")[0];
    r.wait(30);
    r.bot(Command::PauseStrategy {
        id: ID,
        reason: "test".into(),
    })
    .unwrap();
    // Making the reversal marketable while paused is an entry: refused.
    assert!(
        denied(r.bot(Command::ModifyOrder {
            account: acct(),
            order: second,
            qty: None,
            price: Some(px(5999.75)),
        }))
        .contains("is paused")
    );
}

#[test]
fn its_flatten_with_a_position_leaves_nothing_to_reopen_it() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    // The trader protects the position (pausing the instance); the strategy then flattens.
    let stop = r
        .manual(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Stop {
                trigger: px(5990.0),
            },
        }))
        .unwrap()[0];
    r.wait(30);
    r.bot(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert!(
        !r.core.order(stop).unwrap().is_open(),
        "no stop left to open a short"
    );
}

#[test]
fn commands_the_core_issues_never_take_a_callers_id() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 1, limit(5985.0))).unwrap();
    r.wait(30);
    // A person's order pauses the instance: the core cancels its entry with its own command.
    let before: Vec<u64> = ids(&r.events);
    r.manual(buy(es(), 1, limit(5950.0))).unwrap();
    r.wait(30);
    let internal: Vec<u64> = ids(&r.events)
        .into_iter()
        .filter(|i| !before.contains(i))
        .filter(|i| *i >= lines_trading::INTERNAL_COMMAND_IDS)
        .collect();
    assert!(
        !internal.is_empty(),
        "the pause's cancel is an internal command"
    );
    // The next caller's command still gets the next small id.
    let max_external = ids(&r.events)
        .into_iter()
        .filter(|i| *i < lines_trading::INTERNAL_COMMAND_IDS)
        .max()
        .unwrap();
    r.manual(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    let next = ids(&r.events)
        .into_iter()
        .filter(|i| *i < lines_trading::INTERNAL_COMMAND_IDS)
        .max()
        .unwrap();
    assert_eq!(next, max_external + 1);
}

fn ids(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Command { id, .. } => Some(id.0),
            _ => None,
        })
        .collect()
}

#[test]
fn daily_loss_closes_a_trade_the_host_paused_with() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    // The host pauses it (levels went stale) with the trade open.
    r.bot(Command::PauseStrategy {
        id: ID,
        reason: "levels demo are stale".into(),
    })
    .unwrap();
    r.book(&[(5990.0, 10)], &[(5990.25, 10)]);
    r.trade(5990.0, 1);
    r.wait(30);
    assert_eq!(
        r.core.position(&acct(), es()),
        0,
        "closed at the daily loss"
    );
}

#[test]
fn daily_loss_never_closes_a_trade_a_person_took_over() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    // A person's own order pauses it: the trade is theirs now.
    r.manual(sell(1, limit(6100.0))).unwrap();
    r.wait(30);
    assert!(r.reason().contains("manual order activity"));
    r.book(&[(5990.0, 10)], &[(5990.25, 10)]);
    r.trade(5990.0, 1);
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 2, "left to the person");
}

#[test]
fn a_flatten_better_than_the_limit_still_ends_the_day() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    // The limit is hit on a print at 5990.00, but the flatten fills at 5994.00 (a better bid):
    // the realised loss ends under the limit.
    r.book(&[(5994.0, 10)], &[(5994.25, 10)]);
    r.trade(5990.0, 1);
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 0);
    let m = denied(r.manual(Command::ArmStrategy { id: ID }));
    assert!(m.contains("no entries until the next trade date"), "{m}");
}

#[test]
fn a_person_taking_over_a_paused_trade_keeps_it_at_the_daily_loss() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    r.bot(Command::PauseStrategy {
        id: ID,
        reason: "levels demo are stale".into(),
    })
    .unwrap();
    // While it is paused, a person works the trade.
    r.manual(sell(1, limit(6100.0))).unwrap();
    r.wait(30);
    r.book(&[(5990.0, 10)], &[(5990.25, 10)]);
    r.trade(5990.0, 1);
    r.wait(30);
    assert_eq!(r.core.position(&acct(), es()), 2, "the person's trade now");
}

#[test]
fn adding_it_again_after_a_person_took_over_keeps_the_trade_theirs() {
    let mut r = Rig::armed();
    r.bot(buy(es(), 2, OrderKind::Market)).unwrap();
    r.wait(30);
    r.manual(sell(1, limit(6100.0))).unwrap();
    r.wait(30);
    // Its settings sent again (Disarmed, no reason) don't hand the trade back to it.
    r.manual(Command::AddStrategy(config())).unwrap();
    r.book(&[(5990.0, 10)], &[(5990.25, 10)]);
    r.trade(5990.0, 1);
    r.wait(30);
    assert_eq!(
        r.core.position(&acct(), es()),
        2,
        "still the person's trade"
    );
}
