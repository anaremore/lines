//! The trading core with the paper broker, driven by scripted commands, books, trades and time
//! (decisions 0004, 0005, 0006, 0012, 0018). Everything is deterministic: time is passed in.

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::persist;
use lines_trading::rules::{APPROXIMATION, DrawdownMode, RulePreset, TrailingDrawdown};
use lines_trading::service::trading_file;
use lines_trading::{
    AccountConfig, AccountId, BracketSpec, Command, Event, FillModel, MarketInput, Money, OrderId,
    OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, RejectCode, RiskLimits, Side,
    Source, TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn nq() -> ContractKey {
    ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap())
}

fn mnq() -> ContractKey {
    ContractKey::monthly(catalog::MNQ, 2026, Month::new(12).unwrap())
}

/// ES price units: 6000.25 → 600025.
fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

fn paper(model: FillModel) -> PaperConfig {
    let mut cfg = PaperConfig {
        latency_ms: LAT,
        fill_model: model,
        ..Default::default()
    };
    cfg.fees.insert(catalog::ES.0, Money::cents(200)); // $2.00 per contract per side
    cfg
}

impl Rig {
    fn with(model: FillModel, account: AccountConfig) -> Rig {
        let mut b = PaperBroker::new(paper(model));
        b.open_account(account.id.clone(), Money::whole(account.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(account, None);
        Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
        }
    }

    fn new(model: FillModel) -> Rig {
        Rig::with(model, AccountConfig::paper("paper-1", 50_000))
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

    fn place(&mut self, side: Side, qty: i64, kind: OrderKind) -> OrderId {
        self.place_on(es(), side, qty, kind)
    }

    fn place_on(
        &mut self,
        contract: ContractKey,
        side: Side,
        qty: i64,
        kind: OrderKind,
    ) -> OrderId {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract,
            side,
            qty: Qty(qty),
            kind,
        }))
        .expect("accepted")[0]
    }

    /// Let time pass (acks after the latency).
    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn market(&mut self, contract: ContractKey, inputs: &[MarketInput]) {
        self.t += 1;
        self.core.market(self.now(), contract, inputs);
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
        self.market(es(), &v);
    }

    fn trade(&mut self, p: f64, q: i64) {
        let t = UtcMillis(self.t + 1);
        self.market(
            es(),
            &[MarketInput::Trade {
                time: t,
                price: px(p),
                qty: Qty(q),
            }],
        );
    }

    /// The states an order went through, from the event stream.
    fn states(&self, id: OrderId) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for e in &self.events {
            if let Event::Order(o) = e
                && o.id == id
            {
                let s = o.state.label().to_owned();
                if v.last() != Some(&s) || s == "partially filled" {
                    v.push(s);
                }
            }
        }
        v
    }

    fn fills(&self, id: OrderId) -> Vec<(Price, i64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Fill(f) if f.order == id => Some((f.price, f.qty.0)),
                _ => None,
            })
            .collect()
    }

    fn state(&self, id: OrderId) -> OrderState {
        self.core.order(id).expect("known order").state.clone()
    }
}

fn limit(p: f64) -> OrderKind {
    OrderKind::Limit { price: px(p) }
}

#[test]
fn lifecycle_submitted_acked_working_partial_filled() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let id = r.place(Side::Buy, 3, limit(6000.00));
    assert_eq!(r.states(id), ["submitted"], "never working before the ack");
    r.wait(LAT - 1);
    assert_eq!(r.state(id), OrderState::Submitted);
    r.wait(1);
    assert_eq!(r.states(id), ["submitted", "acked", "working"]);
    r.trade(5999.75, 2); // through: 2 of 3
    assert_eq!(r.state(id), OrderState::PartiallyFilled);
    r.trade(5999.50, 5); // the rest
    assert_eq!(
        r.states(id),
        [
            "submitted",
            "acked",
            "working",
            "partially filled",
            "filled"
        ]
    );
    assert_eq!(r.fills(id), vec![(px(6000.00), 2), (px(6000.00), 1)]);
    assert_eq!(r.core.position(&acct(), es()), 3);
    // Fees: 3 contracts × $2.00.
    assert_eq!(
        r.core.balance(&acct()),
        Some(Money::whole(50_000) - Money::cents(600))
    );

    // Cancel path.
    let c = r.place(Side::Sell, 1, limit(6010.00));
    r.wait(LAT);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: c,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(r.states(c), ["submitted", "acked", "working", "canceled"]);
    // Cancelling a finished order is refused with the reason.
    let e = r
        .cmd(Command::CancelOrder {
            account: acct(),
            order: c,
        })
        .unwrap_err();
    assert_eq!(e.code, RejectCode::UnknownOrder);
}

#[test]
fn rejections_by_the_core_and_by_the_broker() {
    let mut r = Rig::new(FillModel::TradeThrough);
    // Off the 0.25 tick grid: refused before any broker.
    let e = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: limit(6000.10),
        }))
        .unwrap_err();
    assert_eq!(e.code, RejectCode::Invalid);
    assert!(e.message.contains("tick"), "{}", e.message);
    // Saturday: the broker rejects (market closed), after its latency.
    r.t = UtcMillis::parse_iso("2026-11-14T15:00:00Z").unwrap().0;
    let id = r.place(Side::Buy, 1, limit(6000.00));
    r.wait(LAT);
    assert_eq!(r.states(id), ["submitted", "rejected"]);
    let OrderState::Rejected { reason } = r.state(id) else {
        panic!()
    };
    assert!(reason.contains("closed"), "{reason}");
}

/// The same resting buy at 6000.00 under each model.
#[test]
fn fill_models_trade_through_touch_and_queue() {
    let run = |model, script: &dyn Fn(&mut Rig)| {
        let mut r = Rig::new(model);
        r.book(&[(6000.00, 10)], &[(6000.25, 10)]);
        let id = r.place(Side::Buy, 4, limit(6000.00));
        r.wait(LAT);
        script(&mut r);
        r.core.order(id).map(|o| o.filled.0).unwrap_or(0)
    };
    // A trade AT our price: touch fills, trade-through doesn't.
    let at = |r: &mut Rig| r.trade(6000.00, 5);
    assert_eq!(run(FillModel::Touch, &at), 4);
    assert_eq!(run(FillModel::TradeThrough, &at), 0);
    // A trade through it fills both, up to the traded size.
    let through = |r: &mut Rig| r.trade(5999.75, 3);
    assert_eq!(run(FillModel::TradeThrough, &through), 3);
    assert_eq!(run(FillModel::Touch, &through), 3);
    // Queue estimate: 10 ahead of us. 6 trade at our price (4 left ahead), the level shrinks to
    // 6 (cancels ahead), 7 more trade: 4 ahead, then 3 for us.
    let queue = |r: &mut Rig| {
        r.trade(6000.00, 6);
        r.market(
            es(),
            &[MarketInput::Book {
                side: BookSide::Bid,
                price: px(6000.00),
                qty: Qty(6 + 4),
            }],
        );
        r.trade(6000.00, 7);
    };
    assert_eq!(run(FillModel::QueueEstimate, &queue), 3);
    assert_eq!(run(FillModel::TradeThrough, &queue), 0);
}

#[test]
fn market_order_walks_a_thin_book() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 4)], &[(6000.25, 2), (6000.50, 1), (6001.00, 5)]);
    let id = r.place(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(
        r.fills(id),
        vec![(px(6000.25), 2), (px(6000.50), 1), (px(6001.00), 2)]
    );
    // Three fills: two partials, then filled.
    assert_eq!(
        r.states(id),
        [
            "submitted",
            "acked",
            "partially filled",
            "partially filled",
            "filled"
        ]
    );
    // Average: (2×600025 + 600050 + 2×600100) / 5 = 600060.
    assert_eq!(r.core.order(id).unwrap().avg_fill(), Some(600_060.0));
    // A second market order doesn't reuse the liquidity just taken.
    let id2 = r.place(Side::Buy, 4, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(id2), vec![(px(6001.00), 3)]);
    assert_eq!(
        r.state(id2),
        OrderState::Cancelled,
        "the book ran out: the rest is cancelled"
    );
}

#[test]
fn stop_triggers_on_a_trade_at_its_price() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.00, 1), (5998.75, 5)], &[(5999.25, 3)]);
    let id = r.place(
        Side::Sell,
        2,
        OrderKind::Stop {
            trigger: px(5999.00),
        },
    );
    r.wait(LAT);
    assert_eq!(r.state(id), OrderState::Working);
    r.trade(5999.25, 1);
    assert_eq!(r.state(id), OrderState::Working, "above the trigger");
    r.trade(5999.00, 1);
    assert_eq!(
        r.fills(id),
        vec![(px(5999.00), 1), (px(5998.75), 1)],
        "triggered, then walks the bids"
    );
}

fn bracket(r: &mut Rig) -> (OrderId, OrderId, OrderId) {
    let ids = r
        .cmd(Command::PlaceBracket(BracketSpec {
            entry: OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(2),
                kind: limit(6000.00),
            },
            take_profit: Some(px(6002.00)),
            stop_loss: Some(px(5998.00)),
        }))
        .unwrap();
    (ids[0], ids[1], ids[2])
}

#[test]
fn bracket_exits_wait_for_the_entry_and_cancel_each_other() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let (entry, tp, sl) = bracket(&mut r);
    r.wait(LAT);
    assert_eq!(
        r.state(tp),
        OrderState::Acked,
        "held server-side until the entry fills"
    );
    assert_eq!(r.state(sl), OrderState::Acked);
    r.trade(5999.75, 1); // entry 1 of 2
    assert_eq!(r.state(tp), OrderState::Working);
    assert_eq!(
        r.core.order(tp).unwrap().qty,
        Qty(1),
        "exits follow the filled quantity"
    );
    r.trade(5999.75, 5);
    assert_eq!(r.state(entry), OrderState::Filled);
    assert_eq!(r.core.order(sl).unwrap().qty, Qty(2));
    // Take-profit fills (through 6002.00): the stop is cancelled (OCO).
    r.trade(6002.25, 10);
    assert_eq!(r.state(tp), OrderState::Filled);
    assert_eq!(r.state(sl), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 0);
    // +2.00 points × 2 × $50 − 4 sides × $2 = $192.
    assert_eq!(
        r.core.balance(&acct()),
        Some(Money::whole(50_000) + Money::whole(200) - Money::cents(800))
    );
}

#[test]
fn bracket_survives_a_restart_and_the_core_rebuilds_from_the_broker() {
    let dir = std::env::temp_dir().join(format!("lines-trading-{}", std::process::id()));
    let path = dir.join("paper.json");
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let (_entry, tp, sl) = bracket(&mut r);
    r.wait(LAT);
    r.trade(5999.75, 5);
    let file = trading_file(&r.core, &paper(FillModel::TradeThrough)).unwrap();
    persist::save(&path, &file).unwrap();
    drop(r); // the app closes

    // The app starts again: the broker (paper, from its saved state) still holds the exits.
    let file = persist::load(&path).unwrap().unwrap();
    let broker = PaperBroker::from_state(file.paper.clone(), file.broker.clone());
    let mut core = TradingCore::new(Box::new(broker));
    for a in file.accounts {
        core.add_account(a.config, Some(a.tracker));
    }
    core.set_next_order_id(file.next_order_id);
    let mut r2 = Rig {
        core,
        t: UtcMillis::parse_iso("2026-11-10T15:30:00Z").unwrap().0,
        events: Vec::new(),
    };
    r2.core.rebuild(r2.now());
    r2.collect();
    assert!(
        r2.events
            .iter()
            .any(|e| matches!(e, Event::Rebuilt { open_orders: 2, .. }))
    );
    assert_eq!(
        r2.core.position(&acct(), es()),
        2,
        "the position came back from the broker"
    );
    assert_eq!(r2.state(tp), OrderState::Working);
    assert_eq!(r2.state(sl), OrderState::Working);
    // The stop still protects the position after the restart.
    r2.book(&[(5998.00, 5)], &[(5998.25, 5)]);
    r2.trade(5998.00, 1);
    assert_eq!(r2.state(sl), OrderState::Filled);
    assert_eq!(r2.state(tp), OrderState::Cancelled);
    assert_eq!(r2.core.position(&acct(), es()), 0);
    // A new order continues the numbering (no id reuse).
    let n = r2.place(Side::Buy, 1, limit(5990.00));
    assert!(n.0 > sl.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rebuild_drops_orders_the_broker_does_not_have() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let id = r.place(Side::Buy, 1, limit(5990.00));
    // The connection drops before the ack: the request was lost (the broker never saw it).
    // Simulate by rebuilding from a fresh broker with the same account.
    let mut fresh = PaperBroker::new(paper(FillModel::TradeThrough));
    fresh.open_account(acct(), Money::whole(50_000));
    r.core.set_broker(Box::new(fresh));
    r.core.rebuild(r.now());
    r.collect();
    // Sent with no answer and not at the broker: unresolved, never assumed working (or gone).
    assert_eq!(r.state(id), OrderState::Unresolved);
    assert!(!r.core.order(id).unwrap().is_open(), "not shown as working");
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::Rebuilt { open_orders: 0, .. }))
    );
}

#[test]
fn risk_rejects_but_flatten_and_cancel_all_always_work() {
    let mut cfg = AccountConfig::paper("paper-1", 50_000);
    cfg.limits = RiskLimits {
        max_order_qty: Some(5),
        max_position: Some(2),
        price_band_ticks: Some(40),
        daily_loss: Some(500),
        ..Default::default()
    };
    let mut r = Rig::with(FillModel::TradeThrough, cfg);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    r.trade(6000.00, 1);
    let too_big = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(6),
        kind: limit(6000.00),
    }));
    assert_eq!(too_big.unwrap_err().code, RejectCode::Risk);
    let fat_finger = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: limit(6100.00),
    }));
    assert!(
        fat_finger
            .unwrap_err()
            .message
            .contains("ticks from the market")
    );
    // Two long (via market orders), then a third would breach the position limit.
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 2);
    let resting = r.place(Side::Sell, 1, limit(6005.00));
    r.wait(LAT);
    let over = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: limit(5999.00),
    }));
    assert_eq!(over.unwrap_err().code, RejectCode::Risk);
    // The market drops 6 points: −$600 today, over the $500 daily loss: everything new blocked.
    r.book(&[(5994.00, 10)], &[(5994.25, 10)]);
    r.trade(5994.00, 1);
    let blocked = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: limit(5995.00),
    }));
    assert!(blocked.unwrap_err().message.contains("daily loss"));
    // Flatten still works: cancels the resting order and sells the position at market.
    r.cmd(Command::Flatten {
        account: acct(),
        contract: None,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(r.state(resting), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert!(
        r.cmd(Command::CancelAll {
            account: acct(),
            contract: None
        })
        .is_ok()
    );
}

fn prop_account(preset: RulePreset) -> AccountConfig {
    let mut cfg = AccountConfig::paper("paper-1", 50_000);
    cfg.rules = Some(preset);
    cfg
}

#[test]
fn trailing_drawdown_breach_liquidates_and_blocks() {
    let preset = RulePreset {
        name: "test".into(),
        label: APPROXIMATION.into(),
        account_size: 50_000,
        trailing_drawdown: Some(TrailingDrawdown {
            amount: 1_000,
            mode: DrawdownMode::Intraday,
            lock_at_start_plus: None,
        }),
        daily_loss_limit: None,
        max_micro_contracts: None,
        profit_target: Some(3_000),
        consistency: None,
        liquidate_on_breach: true,
        on_breach: Default::default(),
    };
    let mut r = Rig::with(FillModel::TradeThrough, prop_account(preset));
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    // Up 4 points (+$400 open): the intraday high-water rises to 50,400 − fees.
    r.book(&[(6004.25, 50)], &[(6004.50, 50)]);
    r.trade(6004.25, 1);
    // Down to 5994.00: equity ≈ 49,390 < threshold ≈ 49,396: breached.
    r.book(&[(5994.00, 50)], &[(5994.25, 50)]);
    r.trade(5994.00, 1);
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Breach {
            breach: lines_trading::rules::Breach::TrailingDrawdown,
            ..
        }
    )));
    // Liquidation went through the command path as a rule command.
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Command {
            source: Source::Rule,
            result: Ok(_),
            ..
        }
    )));
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0, "flattened");
    let e = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: limit(5990.00),
        }))
        .unwrap_err();
    assert_eq!(e.code, RejectCode::PropRule);
    // The account event carries the distances.
    let last = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Account(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap();
    let rules = last.rules.unwrap();
    assert!(rules.to_drawdown.unwrap() <= Money::ZERO);
    assert!(last.blocked.is_some());
}

#[test]
fn contract_cap_counts_micro_equivalents() {
    let preset = RulePreset {
        name: "cap".into(),
        label: APPROXIMATION.into(),
        account_size: 50_000,
        trailing_drawdown: None,
        daily_loss_limit: None,
        max_micro_contracts: Some(25),
        profit_target: None,
        consistency: None,
        liquidate_on_breach: false,
        on_breach: Default::default(),
    };
    let mut r = Rig::with(FillModel::TradeThrough, prop_account(preset));
    // 2 NQ = 20 micro-equivalents (1 NQ = 10 MNQ): allowed; working orders count too.
    r.place_on(
        nq(),
        Side::Buy,
        2,
        OrderKind::Limit {
            price: Price(2_000_000),
        },
    );
    // 5 MNQ more = 25: allowed.
    r.place_on(
        mnq(),
        Side::Buy,
        5,
        OrderKind::Limit {
            price: Price(2_000_000),
        },
    );
    // 1 more MNQ = 26: refused.
    let e = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: mnq(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(2_000_000),
            },
        }))
        .unwrap_err();
    assert_eq!(e.code, RejectCode::PropRule);
    assert!(e.message.contains("26 micro"), "{}", e.message);
}

#[test]
fn trading_file_round_trips_and_migrates_v0() {
    let dir = std::env::temp_dir().join(format!("lines-trading-file-{}", std::process::id()));
    let path = dir.join("paper.json");
    let mut r = Rig::new(FillModel::Touch);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    r.place(Side::Buy, 1, OrderKind::Market);
    r.place(Side::Sell, 1, limit(6010.00));
    r.wait(LAT);
    let file = trading_file(&r.core, &paper(FillModel::Touch)).unwrap();
    persist::save(&path, &file).unwrap();
    let back = persist::load(&path).unwrap().unwrap();
    assert_eq!(back, file, "round trip");
    assert_eq!(back.broker.orders.len(), 1, "the resting order");
    assert_eq!(back.paper.fill_model, FillModel::Touch);

    // v0: no version, accounts only.
    std::fs::write(
        &path,
        r#"{"accounts":[{"id":"eval-1","starting_balance":50000},{"id":"eval-2","starting_balance":100000}]}"#,
    )
    .unwrap();
    let f = persist::load(&path).unwrap().unwrap();
    assert_eq!(f.version, persist::VERSION);
    assert_eq!(f.accounts.len(), 2);
    assert_eq!(f.accounts[1].config.starting_balance, 100_000);
    assert_eq!(f.broker.accounts["eval-2"].balance, Money::whole(100_000));
    // A file from a newer build is refused, not half-read.
    std::fs::write(&path, r#"{"version": 99}"#).unwrap();
    assert!(
        persist::load(&path)
            .unwrap_err()
            .to_string()
            .contains("newer")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn account_groups_exist_in_the_model() {
    let mut b = PaperBroker::new(paper(FillModel::TradeThrough));
    let mut core = {
        b.open_account(AccountId::new("a"), Money::whole(50_000));
        b.open_account(AccountId::new("b"), Money::whole(50_000));
        TradingCore::new(Box::new(b))
    };
    for id in ["a", "b"] {
        let mut c = AccountConfig::paper(id, 50_000);
        c.group = Some("evals".into());
        core.add_account(c, None);
    }
    assert_eq!(
        core.groups()["evals"],
        vec![AccountId::new("a"), AccountId::new("b")]
    );
    let _ = core.broker().capabilities();
}

// ---- OMS checklist (docs/inspiration.md, "Trading core, OMS, risk") ---------------------
//
// One test per item that applies to lines-trading; behaviour only. Items that need exact
// broker event orderings use `Scripted`, a broker whose events the test delivers.

use std::sync::{Arc, Mutex};

use lines_trading::broker::{
    BrokerAccount, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot,
};
use lines_trading::core::OverfillPolicy;
use lines_trading::rules::{BreachStates, TradingState};
use lines_trading::{Broker, Order};

#[derive(Default)]
struct Script {
    requests: Vec<BrokerRequest>,
    due: Vec<BrokerEvent>,
    snapshot: BrokerSnapshot,
    /// A broker that can't hold one-cancels-other orders.
    no_oco: bool,
}

/// A broker the test drives: it records requests and emits exactly the events pushed.
struct Scripted(Arc<Mutex<Script>>);

impl Broker for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> BrokerCaps {
        BrokerCaps {
            market_orders: true,
            stop_orders: true,
            modify: true,
            server_brackets: true,
            oco: !self.0.lock().unwrap().no_oco,
            fractional_qty: false,
            external_fills: false,
        }
    }
    fn request(&mut self, _: UtcMillis, req: BrokerRequest) {
        self.0.lock().unwrap().requests.push(req);
    }
    fn market(&mut self, _: UtcMillis, _: ContractKey, _: &MarketInput) {}
    fn poll(&mut self, _: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.append(&mut self.0.lock().unwrap().due);
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        self.0.lock().unwrap().snapshot.clone()
    }
}

fn scripted() -> (Rig, Arc<Mutex<Script>>) {
    let s = Arc::new(Mutex::new(Script::default()));
    let mut core = TradingCore::new(Box::new(Scripted(s.clone())));
    core.add_account(AccountConfig::paper("paper-1", 50_000), None);
    let rig = Rig {
        core,
        t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
        events: Vec::new(),
    };
    (rig, s)
}

impl Rig {
    /// Deliver broker events and let the core apply them.
    fn deliver(&mut self, s: &Arc<Mutex<Script>>, evs: Vec<BrokerEvent>) {
        s.lock().unwrap().due.extend(evs);
        self.wait(1);
    }
}

fn at(r: &Rig) -> UtcMillis {
    r.now()
}

fn fill(id: OrderId, trade_id: &str, p: f64, q: i64, t: UtcMillis) -> BrokerEvent {
    BrokerEvent::Fill {
        id,
        trade_id: trade_id.into(),
        price: px(p),
        qty: Qty(q),
        fee: Money::ZERO,
        at: t,
    }
}

fn working(r: &mut Rig, s: &Arc<Mutex<Script>>, qty: i64) -> OrderId {
    let id = r.place(Side::Buy, qty, limit(6000.00));
    let t = at(r);
    r.deliver(
        s,
        vec![
            BrokerEvent::Acked { id, at: t },
            BrokerEvent::Working { id, at: t },
        ],
    );
    id
}

/// Item 1: Denied (by the core's risk or rules, never sent) is not rejected (by the broker).
#[test]
fn checklist_01_denied_is_not_rejected() {
    let (mut r, s) = scripted();
    // Denied: an off-grid price never reaches the broker and creates no order.
    let denied = r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: limit(6000.10),
    }));
    assert_eq!(denied.unwrap_err().code, RejectCode::Invalid);
    assert!(s.lock().unwrap().requests.is_empty(), "never sent");
    assert!(!r.events.iter().any(|e| matches!(e, Event::Order(_))));
    // Rejected: sent, then refused by the broker: the order says so, with the reason.
    let id = r.place(Side::Buy, 1, limit(6000.00));
    assert_eq!(s.lock().unwrap().requests.len(), 1);
    let t = at(&r);
    r.deliver(
        &s,
        vec![BrokerEvent::Rejected {
            id,
            reason: "insufficient margin".into(),
            at: t,
        }],
    );
    assert_eq!(
        r.state(id),
        OrderState::Rejected {
            reason: "insufficient margin".into()
        }
    );
}

/// Item 2: Cancel All covers orders still submitted (no ack yet) and acked (held exits).
#[test]
fn checklist_02_cancel_all_covers_submitted_and_acked_orders() {
    let (mut r, s) = scripted();
    let submitted = r.place(Side::Buy, 1, limit(6000.00));
    let acked = r.place(Side::Buy, 1, limit(5999.00));
    let t = at(&r);
    r.deliver(&s, vec![BrokerEvent::Acked { id: acked, at: t }]);
    let ids = r
        .cmd(Command::CancelAll {
            account: acct(),
            contract: None,
        })
        .unwrap();
    assert_eq!(ids, vec![submitted, acked]);
    let cancels: Vec<OrderId> = s
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter_map(|q| match q {
            BrokerRequest::Cancel { id } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(cancels, vec![submitted, acked]);
}

/// Item 3: A fill that arrives while a cancel is in flight applies; the order ends Filled, or
/// Cancelled with its filled quantity.
#[test]
fn checklist_03_fill_during_a_pending_cancel_applies() {
    let (mut r, s) = scripted();
    // Filled before the cancel lands: Filled; the broker's late "too late to cancel" can't
    // turn it into anything else.
    let a = working(&mut r, &s, 2);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: a,
    })
    .unwrap();
    assert_eq!(
        r.state(a),
        OrderState::Working,
        "still working while the cancel is in flight"
    );
    let t = at(&r);
    r.deliver(&s, vec![fill(a, "x-1", 6000.00, 2, t)]);
    assert_eq!(r.state(a), OrderState::Filled);
    r.deliver(
        &s,
        vec![BrokerEvent::Rejected {
            id: a,
            reason: "too late to cancel".into(),
            at: t,
        }],
    );
    assert_eq!(r.state(a), OrderState::Filled);
    assert_eq!(r.core.position(&acct(), es()), 2);
    // Part-filled, then cancelled: Cancelled, and the fill counts.
    let b = working(&mut r, &s, 3);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: b,
    })
    .unwrap();
    let t = at(&r);
    r.deliver(
        &s,
        vec![
            fill(b, "x-2", 6000.00, 1, t),
            BrokerEvent::Cancelled {
                id: b,
                reason: "canceled".into(),
                at: t,
            },
        ],
    );
    assert_eq!(r.state(b), OrderState::Cancelled);
    assert_eq!(r.core.order(b).unwrap().filled, Qty(1));
    assert_eq!(r.core.position(&acct(), es()), 3);
}

/// Item 4: A fill reported after the order finished (Lines has no GTD/DAY orders yet; the same
/// rule covers a cancel confirmed before a fill report) is applied without reopening it.
#[test]
fn checklist_04_late_fill_applies_without_reopening() {
    let (mut r, s) = scripted();
    let id = working(&mut r, &s, 2);
    let t = at(&r);
    r.deliver(
        &s,
        vec![
            BrokerEvent::Cancelled {
                id,
                reason: "canceled".into(),
                at: t,
            },
            fill(id, "late-1", 6000.00, 1, t),
        ],
    );
    let o = r.core.order(id).unwrap();
    assert_eq!(o.state, OrderState::Cancelled, "not reopened");
    assert_eq!(o.filled, Qty(1));
    assert_eq!(r.core.position(&acct(), es()), 1, "the fill counts");
}

fn breach_preset() -> RulePreset {
    RulePreset {
        name: "states".into(),
        label: APPROXIMATION.into(),
        account_size: 50_000,
        trailing_drawdown: Some(TrailingDrawdown {
            amount: 400,
            mode: DrawdownMode::Intraday,
            lock_at_start_plus: None,
        }),
        daily_loss_limit: Some(200),
        max_micro_contracts: None,
        profit_target: None,
        consistency: None,
        liquidate_on_breach: false,
        on_breach: BreachStates::default(),
    }
}

fn spec(side: Side, qty: i64, kind: OrderKind) -> Command {
    Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side,
        qty: Qty(qty),
        kind,
    })
}

/// Items 5 and 6: Trading states after prop breaches: daily loss → reducing only (orders that would
/// increase or flip are refused; reduce-only orders shrink with the position and are
/// cancelled when it is flat), drawdown → halted (only cancels and Flatten).
#[test]
fn checklist_05_06_reducing_only_and_halted() {
    let mut r = Rig::with(FillModel::TradeThrough, prop_account(breach_preset()));
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 3);
    assert_eq!(r.core.trading_state(&acct()), TradingState::Active);
    // Down 1.5 points × 3 × $50 = −$225 (plus fees): daily loss (200) reached, drawdown (400)
    // not: reducing only.
    r.book(&[(5998.75, 50)], &[(5999.00, 50)]);
    r.trade(5998.75, 1);
    assert_eq!(r.core.trading_state(&acct()), TradingState::ReducingOnly);
    let last_account = |r: &Rig| {
        r.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Account(a) => Some(a.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(last_account(&r).trading, TradingState::ReducingOnly);
    // Adding or flipping: refused.
    let add = r.cmd(spec(Side::Buy, 1, limit(5998.00))).unwrap_err();
    assert_eq!(add.code, RejectCode::PropRule);
    assert!(add.message.contains("reducing only"));
    let flip = r.cmd(spec(Side::Sell, 4, limit(6010.00))).unwrap_err();
    assert_eq!(flip.code, RejectCode::PropRule);
    // Reducing: allowed, up to what open orders don't already close.
    let exit = r.cmd(spec(Side::Sell, 2, limit(6010.00))).unwrap()[0];
    assert!(r.core.order(exit).unwrap().reduce_only);
    assert!(
        r.cmd(spec(Side::Sell, 2, limit(6010.00))).is_err(),
        "3 − 2 open = 1 left"
    );
    let exit2 = r.cmd(spec(Side::Sell, 1, limit(6011.00))).unwrap()[0];
    r.wait(LAT);
    // The position shrinks elsewhere (a manual Flatten of part is not possible; use a fill
    // of the first exit): the remaining reduce-only order never closes more than is left.
    r.book(&[(6010.25, 50)], &[(6010.50, 50)]);
    r.trade(6010.25, 2); // exit fills 2: position 1, exit2 (1) still fits
    assert_eq!(r.core.position(&acct(), es()), 1);
    assert_eq!(r.state(exit2), OrderState::Working);
    // Flatten closes the last one: exit2 has nothing left to close and is cancelled.
    r.cmd(Command::Flatten {
        account: acct(),
        contract: None,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_eq!(r.state(exit2), OrderState::Cancelled);

    // Drawdown breach: halted. Nothing new, not even a reducing order; cancels still work.
    let mut r = Rig::with(FillModel::TradeThrough, prop_account(breach_preset()));
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    r.book(&[(5997.00, 50)], &[(5997.25, 50)]);
    r.trade(5997.00, 1); // −$487.50: drawdown (400) breached
    assert_eq!(r.core.trading_state(&acct()), TradingState::Halted);
    let e = r.cmd(spec(Side::Sell, 1, limit(6010.00))).unwrap_err();
    assert!(e.message.contains("halted"), "{e:?}");
    assert!(
        r.cmd(Command::CancelAll {
            account: acct(),
            contract: None
        })
        .is_ok()
    );
    r.cmd(Command::Flatten {
        account: acct(),
        contract: None,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(
        r.core.position(&acct(), es()),
        0,
        "Flatten works when halted"
    );
}

/// Item 7: Submit and modify rate limits (separate), and the maximum notional per order.
#[test]
fn checklist_07_rate_limits_and_notional() {
    let mut cfg = AccountConfig::paper("paper-1", 50_000);
    cfg.limits = RiskLimits {
        max_submits_per_sec: Some(3),
        max_modifies_per_sec: Some(2),
        // ES at 6000 × $50 = $300,000 a contract.
        max_notional: Some(700_000),
        ..Default::default()
    };
    let mut r = Rig::with(FillModel::TradeThrough, cfg);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let big = r.cmd(spec(Side::Buy, 3, limit(5990.00))).unwrap_err();
    assert!(big.message.contains("order value"), "{big:?}");
    let ids: Vec<OrderId> = (0..3)
        .map(|i| r.place(Side::Buy, 1, limit(5990.00 - i as f64)))
        .collect();
    let fourth = r.cmd(spec(Side::Buy, 1, limit(5980.00))).unwrap_err();
    assert!(fourth.message.contains("new orders in a second"));
    // Modifies are counted on their own: still allowed (the submit limit is used up).
    r.wait(LAT);
    for (i, id) in ids.iter().take(2).enumerate() {
        r.cmd(Command::ModifyOrder {
            account: acct(),
            order: *id,
            qty: None,
            price: Some(px(5985.00 - i as f64)),
        })
        .unwrap();
    }
    let third = r
        .cmd(Command::ModifyOrder {
            account: acct(),
            order: ids[2],
            qty: None,
            price: Some(px(5984.00)),
        })
        .unwrap_err();
    assert!(third.message.contains("modifies in a second"));
    // Cancels and Flatten are never rate limited.
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: None,
    })
    .unwrap();
    // A second later the window has moved on.
    r.wait(1000);
    r.place(Side::Buy, 1, limit(5990.00));
}

/// Item 8: Fills are de-duplicated by the broker's execution id on every path; reconciliation's
/// own fills have deterministic ids, so repeating it (another reconnect, a restart) never
/// counts them twice.
#[test]
fn checklist_08_fills_deduplicated_by_trade_id() {
    let (mut r, s) = scripted();
    let id = working(&mut r, &s, 3);
    let t = at(&r);
    // The stream delivers an execution twice (a reconnect replays it), even with another
    // price: counted once.
    r.deliver(&s, vec![fill(id, "exec-7", 6000.00, 1, t)]);
    r.deliver(&s, vec![fill(id, "exec-7", 5999.00, 1, t)]);
    assert_eq!(r.core.position(&acct(), es()), 1);
    assert_eq!(r.fills(id).len(), 1);
    // The broker filled one more while we weren't listening: the snapshot says 2 filled.
    let mut at_broker: Order = r.core.order(id).unwrap().clone();
    at_broker.filled = Qty(2);
    at_broker.fill_value += px(6000.00).0 as i128;
    at_broker.state = OrderState::PartiallyFilled;
    s.lock().unwrap().snapshot = BrokerSnapshot {
        accounts: vec![BrokerAccount {
            id: acct(),
            balance: Money::whole(50_000),
            positions: vec![lines_trading::broker::BrokerPosition {
                contract: es(),
                position: {
                    let mut p = lines_trading::position::Position::default();
                    p.apply_fill(Side::Buy, px(6000.00), Qty(2));
                    p
                },
            }],
            orders: vec![at_broker.clone()],
        }],
    };
    r.core.rebuild(r.now());
    r.collect();
    let recon: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Fill(f) if f.reconciled => Some((f.trade_id.clone(), f.qty.0, f.price)),
            _ => None,
        })
        .collect();
    assert_eq!(
        recon,
        vec![(format!("recon-{}-2", id.0), 1, px(6000.00))],
        "one synthesised fill, deterministic id"
    );
    assert_eq!(
        r.core.position(&acct(), es()),
        2,
        "position from the broker"
    );
    // Reconcile again (another reconnect): nothing new.
    r.events.clear();
    r.core.rebuild(r.now());
    r.collect();
    assert!(!r.events.iter().any(|e| matches!(e, Event::Fill(_))));
    // The same execution replayed by the broker later is still ignored.
    r.deliver(&s, vec![fill(id, "exec-7", 6000.00, 1, t)]);
    assert_eq!(r.core.position(&acct(), es()), 2);
}

/// Item 9: Overfill: recorded (default) or refused, always reported, never clamped.
#[test]
fn checklist_09_overfill_recorded_or_refused() {
    let (mut r, s) = scripted();
    let id = working(&mut r, &s, 2);
    let t = at(&r);
    r.deliver(&s, vec![fill(id, "o-1", 6000.00, 3, t)]);
    let o = r.core.order(id).unwrap();
    assert_eq!(
        (o.filled, o.overfill, o.state.clone()),
        (Qty(3), Qty(1), OrderState::Filled)
    );
    assert_eq!(
        r.core.position(&acct(), es()),
        3,
        "the broker's quantity, not clamped"
    );
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Overfill {
            excess: Qty(1),
            recorded: true,
            ..
        }
    )));

    let (mut r, s) = scripted();
    r.core.set_overfill_policy(OverfillPolicy::Reject);
    let id = working(&mut r, &s, 2);
    let t = at(&r);
    r.deliver(&s, vec![fill(id, "o-2", 6000.00, 3, t)]);
    assert_eq!(
        r.core.order(id).unwrap().filled,
        Qty(0),
        "refused whole, not clamped to 2"
    );
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Overfill {
            excess: Qty(1),
            recorded: false,
            ..
        }
    )));
}

/// Item 10: Several resting orders never fill more than a trade printed (paper broker).
#[test]
fn checklist_10_orders_never_fill_more_than_printed() {
    for model in [
        FillModel::TradeThrough,
        FillModel::Touch,
        FillModel::QueueEstimate,
    ] {
        let mut r = Rig::new(model);
        r.book(&[(5999.75, 0)], &[(6000.25, 10)]);
        let ids: Vec<OrderId> = (0..3)
            .map(|_| r.place(Side::Buy, 2, limit(6000.00)))
            .collect();
        r.wait(LAT);
        r.trade(5999.75, 3); // through 6000.00, 3 printed
        let filled: i64 = ids
            .iter()
            .map(|id| r.core.order(*id).unwrap().filled.0)
            .sum();
        assert_eq!(filled, 3, "{model:?}: exactly what printed");
        // Oldest first.
        assert_eq!(r.core.order(ids[0]).unwrap().filled, Qty(2), "{model:?}");
        assert_eq!(r.core.order(ids[1]).unwrap().filled, Qty(1), "{model:?}");
    }
}

/// Item 11: Contingent orders, one-updates-other: a partial fill of one exit shrinks its sibling;
/// a partially filled entry protects exactly what it filled, and exits follow later fills.
#[test]
fn checklist_11_one_updates_other_on_partial_fills() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let ids = r
        .cmd(Command::PlaceBracket(BracketSpec {
            entry: OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(3),
                kind: limit(6000.00),
            },
            take_profit: Some(px(6002.00)),
            stop_loss: Some(px(5998.00)),
        }))
        .unwrap();
    let (entry, tp, sl) = (ids[0], ids[1], ids[2]);
    r.wait(LAT);
    let qty = |r: &Rig, id| {
        let o = r.core.order(id).unwrap();
        (o.qty.0, o.filled.0)
    };
    // Entry fills 1 of 3: both exits protect 1.
    r.trade(5999.75, 1);
    assert_eq!((qty(&r, tp), qty(&r, sl)), ((1, 0), (1, 0)));
    // Take-profit fills that 1 while the entry still works: the stop has nothing to close
    // and is held, not cancelled (the entry may fill more).
    r.book(&[(6002.25, 10)], &[(6002.50, 10)]);
    r.trade(6002.25, 1);
    assert_eq!(r.state(tp), OrderState::Filled);
    assert_ne!(
        r.state(sl),
        OrderState::Cancelled,
        "kept for the rest of the entry"
    );
    assert_eq!(qty(&r, sl), (0, 0));
    assert_eq!(r.core.position(&acct(), es()), 0);
    // The entry fills its other 2: the stop protects exactly those 2.
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    r.trade(5999.75, 5);
    assert_eq!(r.state(entry), OrderState::Filled);
    assert_eq!(r.core.position(&acct(), es()), 2);
    assert_eq!(qty(&r, sl), (2, 0), "the stop covers the open 2");
    // The stop, now the only exit, closes the 2 (walking a thin bid, then at the trade).
    r.book(&[(5997.75, 1)], &[(5998.00, 10)]);
    r.trade(5997.75, 1);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_eq!(r.state(sl), OrderState::Filled);

    // Fully filled entry, partial take-profit: the stop shrinks to what is left (OUO).
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let ids = r
        .cmd(Command::PlaceBracket(BracketSpec {
            entry: OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(3),
                kind: limit(6000.00),
            },
            take_profit: Some(px(6002.00)),
            stop_loss: Some(px(5998.00)),
        }))
        .unwrap();
    let (tp, sl) = (ids[1], ids[2]);
    r.wait(LAT);
    r.trade(5999.75, 5);
    assert_eq!(qty(&r, sl), (3, 0));
    r.book(&[(6002.25, 10)], &[(6002.50, 10)]);
    r.trade(6002.25, 2); // take-profit 2 of 3
    assert_eq!(qty(&r, tp), (3, 2));
    assert_eq!(
        qty(&r, sl),
        (1, 0),
        "the sibling shrank by the partial fill"
    );
    r.trade(6002.25, 5);
    assert_eq!(r.state(tp), OrderState::Filled);
    assert_eq!(
        r.state(sl),
        OrderState::Cancelled,
        "OCO done once nothing is open"
    );
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// Item 12: Shutdown and restart: a submit with no broker outcome comes back unresolved, never
/// assumed working; one the broker knows comes back as the broker has it.
#[test]
fn checklist_12_unanswered_submits_are_unresolved_after_restart() {
    let dir = std::env::temp_dir().join(format!("lines-trading-12-{}", std::process::id()));
    let path = dir.join("paper.json");
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 10)], &[(6000.25, 10)]);
    let acked = r.place(Side::Buy, 1, limit(5990.00));
    r.wait(LAT);
    let in_flight = r.place(Side::Buy, 1, limit(5991.00)); // the app stops before the ack
    let file = trading_file(&r.core, &paper(FillModel::TradeThrough)).unwrap();
    assert_eq!(file.in_flight.len(), 1);
    persist::save(&path, &file).unwrap();
    drop(r);

    let file = persist::load(&path).unwrap().unwrap();
    // The paper broker's queue of not-yet-handled requests isn't saved: the in-flight order
    // never reached it (like a request lost in a crash).
    let broker = PaperBroker::from_state(file.paper.clone(), file.broker.clone());
    let mut core = TradingCore::new(Box::new(broker));
    for a in file.accounts {
        core.add_account(a.config, Some(a.tracker));
    }
    core.set_next_order_id(file.next_order_id);
    core.restore_in_flight(file.in_flight);
    let mut r2 = Rig {
        core,
        t: UtcMillis::parse_iso("2026-11-10T15:05:00Z").unwrap().0,
        events: Vec::new(),
    };
    r2.core.rebuild(r2.now());
    r2.collect();
    assert_eq!(r2.state(acked), OrderState::Working, "as the broker has it");
    assert_eq!(r2.state(in_flight), OrderState::Unresolved);
    assert!(
        !r2.core.order(in_flight).unwrap().is_open(),
        "never shown as working"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Chart trading (roadmap 8): a stop or target added for an open position is reduce-only at
/// the broker: refused without a position, on the opening side or larger than the position;
/// once placed it shrinks with the position and is cancelled when it is flat, so it can never
/// open a new position after the other exit closed this one.
#[test]
fn place_exit_is_reduce_only_and_never_outlives_the_position() {
    let exit = |side, qty, kind| {
        Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        })
    };
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    let stop = OrderKind::Stop {
        trigger: px(5995.00),
    };
    let none = r.cmd(exit(Side::Sell, 1, stop)).unwrap_err();
    assert!(
        none.message.contains("no open position"),
        "{}",
        none.message
    );
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 2);
    assert!(
        r.cmd(exit(Side::Buy, 1, limit(5990.00))).is_err(),
        "a buy can't close a long"
    );
    assert!(
        r.cmd(exit(Side::Sell, 3, stop)).is_err(),
        "larger than the position"
    );
    let s = r.cmd(exit(Side::Sell, 2, stop)).unwrap()[0];
    let t = r.cmd(exit(Side::Sell, 2, limit(6005.00))).unwrap()[0];
    r.wait(LAT);
    assert!(r.core.order(s).unwrap().reduce_only);
    assert!(r.core.order(t).unwrap().reduce_only);
    // Code review: the two whole-position exits are one OCO pair, budgeted once.
    assert_eq!(r.core.order(t).unwrap().oco, Some(s));
    r.book(&[(6005.25, 50)], &[(6005.50, 50)]);
    r.trade(6005.25, 2);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_eq!(r.state(t), OrderState::Filled);
    assert_eq!(r.state(s), OrderState::Cancelled);
}

/// Code review round 1: a stop and a target added for a 2-lot position are an OCO pair, so a
/// partial fill of one leaves both covering what is left (never an unprotected contract), and
/// the position flat cancels the survivor.
#[test]
fn place_exit_pair_survives_a_partial_fill() {
    let exit = |side, qty, kind| {
        Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        })
    };
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    let t = r.cmd(exit(Side::Sell, 2, limit(6005.00))).unwrap()[0];
    let s = r
        .cmd(exit(
            Side::Sell,
            2,
            OrderKind::Stop {
                trigger: px(5995.00),
            },
        ))
        .unwrap()[0];
    r.wait(LAT);
    // One lot of the target fills: position 1; the stop still protects it.
    r.book(&[(6005.25, 50)], &[(6005.50, 50)]);
    r.trade(6005.25, 1);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 1);
    let stop = r.core.order(s).unwrap();
    assert!(stop.is_open(), "the stop survives: {:?}", stop.state);
    assert_eq!(stop.remaining(), Qty(1), "and covers the one lot left");
    assert!(r.core.order(t).unwrap().is_open());
    // The rest of the target fills: flat; the stop is cancelled.
    r.trade(6005.25, 5);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_eq!(r.state(s), OrderState::Cancelled);
}

/// Code review round 2: cancel the stop of a chart-added pair and add a new one: the new stop
/// pairs with the target (the link is both ways and a finished sibling doesn't hold it), so a
/// partial target fill leaves the new stop covering what is left.
#[test]
fn place_exit_repairs_after_a_cancel() {
    let exit = |side, qty, kind| {
        Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        })
    };
    let stop = |p| OrderKind::Stop { trigger: px(p) };
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    let s = r.cmd(exit(Side::Sell, 2, stop(5995.00))).unwrap()[0];
    let t = r.cmd(exit(Side::Sell, 2, limit(6005.00))).unwrap()[0];
    r.wait(LAT);
    assert_eq!(r.core.order(s).unwrap().oco, Some(t), "linked both ways");
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: s,
    })
    .unwrap();
    r.wait(LAT);
    let s2 = r.cmd(exit(Side::Sell, 2, stop(5994.00))).unwrap()[0];
    r.wait(LAT);
    assert_eq!(r.core.order(s2).unwrap().oco, Some(t));
    r.book(&[(6005.25, 50)], &[(6005.50, 50)]);
    r.trade(6005.25, 1);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 1);
    let s2o = r.core.order(s2).unwrap();
    assert!(s2o.is_open(), "{:?}", s2o.state);
    assert_eq!(s2o.remaining(), Qty(1));
}

fn place_exit_cmd(side: Side, qty: i64, kind: OrderKind) -> Command {
    Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: es(),
        side,
        qty: Qty(qty),
        kind,
    })
}

/// Review round 3: a chart exit is never refused as an entry. While the account may only
/// reduce (a prop breach), both the target and the stop of the position go in, though the
/// first already "closes" the whole position: only one of the pair can ever fill.
#[test]
fn place_exit_protects_while_reducing_only() {
    let mut r = Rig::with(FillModel::TradeThrough, prop_account(breach_preset()));
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    r.book(&[(5998.75, 50)], &[(5999.00, 50)]);
    r.trade(5998.75, 1);
    assert_eq!(r.core.trading_state(&acct()), TradingState::ReducingOnly);
    let t = r.cmd(place_exit_cmd(Side::Sell, 3, limit(6010.00)));
    assert!(t.is_ok(), "{t:?}");
    let s = r.cmd(place_exit_cmd(
        Side::Sell,
        3,
        OrderKind::Stop {
            trigger: px(5990.00),
        },
    ));
    assert!(s.is_ok(), "the stop is protection, not an entry: {s:?}");
}

/// Review round 3: on a broker that can't hold one-cancels-other orders, a chart exit is
/// refused (the stop would outlive a filled target and could open a position there).
#[test]
fn place_exit_needs_a_broker_with_oco() {
    let (mut r, s) = scripted();
    let id = working(&mut r, &s, 2);
    let t = at(&r);
    r.deliver(&s, vec![fill(id, "f1", 6000.00, 2, t)]);
    assert_eq!(r.core.position(&acct(), es()), 2);
    s.lock().unwrap().no_oco = true;
    let e = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: limit(6010.00),
        }))
        .unwrap_err();
    assert_eq!(e.code, RejectCode::Broker, "{}", e.message);
}

/// Focused review: an exit sent while the position is open but arriving after it closed
/// (the target filled during the stop's latency) never opens a position: the broker cancels it
/// on arrival, and a trade through its trigger leaves the account flat.
#[test]
fn place_exit_arriving_after_flat_never_opens_a_position() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    let t = r
        .cmd(place_exit_cmd(Side::Sell, 2, limit(6005.00)))
        .unwrap()[0];
    r.wait(LAT);
    // The stop is sent; before it reaches the broker, the target fills: flat.
    let s = r
        .cmd(place_exit_cmd(
            Side::Sell,
            2,
            OrderKind::Stop {
                trigger: px(5995.00),
            },
        ))
        .unwrap()[0];
    r.book(&[(6005.25, 50)], &[(6005.50, 50)]);
    r.trade(6005.25, 5);
    assert_eq!(r.state(t), OrderState::Filled);
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    // A trade through the stop's trigger: nothing fills, nothing opens.
    r.book(&[(5994.75, 50)], &[(5995.00, 50)]);
    r.trade(5994.75, 5);
    r.wait(LAT);
    assert_eq!(r.state(s), OrderState::Cancelled);
    assert_eq!(
        r.core.position(&acct(), es()),
        0,
        "the exit never opened a short"
    );
}

/// Focused review: a guardrail lockout (daily loss) blocks entries, never the position's
/// protection: both exits go in while locked.
#[test]
fn place_exit_protects_during_a_guardrail_lockout() {
    let mut r = Rig::new(FillModel::TradeThrough);
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: lines_trading::Guardrails {
            max_daily_loss: Some(50),
            ..Default::default()
        },
    })
    .unwrap();
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    // Down a point on 2 ES: -$100 + fees, past the $50 guardrail: locked.
    r.book(&[(5999.00, 50)], &[(5999.25, 50)]);
    r.trade(5999.00, 1);
    r.wait(LAT);
    let entry = r.cmd(spec(Side::Buy, 1, limit(5990.00)));
    assert!(entry.is_err(), "entries are locked out");
    assert!(r.cmd(place_exit_cmd(Side::Sell, 2, limit(6010.00))).is_ok());
    let s = r.cmd(place_exit_cmd(
        Side::Sell,
        2,
        OrderKind::Stop {
            trigger: px(5990.00),
        },
    ));
    assert!(s.is_ok(), "the stop is protection, not an entry: {s:?}");
}

/// Focused review: a halted account (prop drawdown) refuses even a protective exit; Flatten is
/// the way out there (deliberate, `check_exit`).
#[test]
fn place_exit_refused_while_halted() {
    let mut r = Rig::with(FillModel::TradeThrough, prop_account(breach_preset()));
    r.book(&[(5999.75, 50)], &[(6000.25, 50)]);
    r.place(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    r.book(&[(5997.00, 50)], &[(5997.25, 50)]);
    r.trade(5997.00, 1);
    assert_eq!(r.core.trading_state(&acct()), TradingState::Halted);
    let e = r
        .cmd(place_exit_cmd(
            Side::Sell,
            3,
            OrderKind::Stop {
                trigger: px(5990.00),
            },
        ))
        .unwrap_err();
    assert_eq!(e.code, RejectCode::PropRule, "{}", e.message);
}
