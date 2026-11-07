//! Prop rules changed while trading (`Command::SetRules`) through the real core and the paper
//! broker: a tighter rule checks the next order and leaves working orders alone, a looser one
//! waits for the next trade date (during a breach too), the account size is fixed, and the
//! rules still waiting survive a restart.

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::persist;
use lines_trading::rules::{self, RulePreset};
use lines_trading::service::trading_file;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FillModel, MarketInput, Money, OrderId, OrderKind,
    OrderSpec, OrderState, PaperBroker, PaperConfig, Reject, RejectCode, Side, Source, TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

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

/// The 50K end-of-day preset (daily loss $1,000, 50 micros), no liquidation (the tests
/// read the account's state, not a flatten).
fn eod() -> RulePreset {
    let mut p = rules::presets()
        .into_iter()
        .find(|p| p.daily_loss_limit == Some(1_000) && p.account_size == 50_000)
        .expect("the 50K end-of-day preset");
    p.liquidate_on_breach = false;
    p
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    fn new(rules: RulePreset) -> Rig {
        let mut cfg = AccountConfig::paper("paper-1", 50_000);
        cfg.rules = Some(rules);
        let mut b = PaperBroker::new(paper());
        b.open_account(cfg.id.clone(), Money::whole(cfg.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(cfg, None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open, trade date 2026-11-10.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
        };
        r.book_at(5999.75, 6000.25);
        r
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, Reject> {
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

    fn set(&mut self, rules: Option<RulePreset>) -> Result<Vec<OrderId>, Reject> {
        self.cmd(Command::SetRules {
            account: acct(),
            rules,
        })
    }

    fn order(&mut self, side: Side, qty: i64, kind: OrderKind) -> Result<Vec<OrderId>, Reject> {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        }))
    }

    fn resting(&mut self, qty: i64) -> Result<Vec<OrderId>, Reject> {
        self.order(Side::Buy, qty, OrderKind::Limit { price: px(5990.00) })
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.events.extend(self.core.drain());
    }

    fn book_at(&mut self, bid: f64, ask: f64) {
        self.t += 1;
        let v = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: px(bid),
                qty: Qty(100),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(ask),
                qty: Qty(100),
            },
            MarketInput::Last(px(bid)),
        ];
        self.core.market(self.now(), es(), &v);
        self.events.extend(self.core.drain());
    }

    fn state_of(&self, id: OrderId) -> OrderState {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Order(o) if o.id == id => Some(o.state.clone()),
                _ => None,
            })
            .expect("the order was reported")
    }

    fn next_day(&mut self) {
        self.t = UtcMillis::parse_iso("2026-11-11T15:00:00Z").unwrap().0;
        self.book_at(5999.75, 6000.25);
    }
}

/// A tighter contract cap checks the next order at once; the order already working is left
/// alone (never changed mid-order).
#[test]
fn tighter_rules_check_the_next_order_and_leave_working_ones() {
    let mut r = Rig::new(eod());
    let working = r.resting(2).expect("20 of 50 micros")[0];
    r.wait(LAT);
    assert_eq!(r.state_of(working), OrderState::Working);
    r.set(Some(RulePreset {
        max_micro_contracts: Some(30),
        ..eod()
    }))
    .unwrap();
    let e = r.resting(2).expect_err("40 of 30 micros");
    assert_eq!(e.code, RejectCode::PropRule, "{e:?}");
    r.resting(1).expect("30 of 30 micros");
    r.wait(LAT);
    assert_eq!(r.state_of(working), OrderState::Working, "untouched");
    assert!(r.core.pending_rules(&acct()).is_none(), "nothing waits");
}

/// A looser cap waits for the next trade date.
#[test]
fn looser_rules_wait_for_the_next_trade_date() {
    let mut r = Rig::new(RulePreset {
        max_micro_contracts: Some(20),
        ..eod()
    });
    r.set(Some(eod())).unwrap();
    assert!(r.core.pending_rules(&acct()).is_some(), "50 micros waits");
    r.resting(2).expect("20 of 20 micros");
    let e = r.resting(1).expect_err("still 20 today");
    assert_eq!(e.code, RejectCode::PropRule);
    r.next_day();
    r.resting(3).expect("50 micros from today");
    assert!(r.core.pending_rules(&acct()).is_none());
}

/// During a breach a looser rule doesn't lift it: it waits for the next trade date. A
/// tighter one still applies.
#[test]
fn a_breach_is_not_loosened_away() {
    let mut r = Rig::new(RulePreset {
        daily_loss_limit: Some(100),
        ..eod()
    });
    // Lose more than $100: buy 1 at the ask, the market drops 4 points.
    r.order(Side::Buy, 1, OrderKind::Market).expect("accepted");
    r.wait(LAT);
    r.book_at(5995.75, 5996.25);
    let e = r.resting(1).expect_err("daily loss reached");
    assert_eq!(e.code, RejectCode::PropRule, "{e:?}");
    r.set(Some(eod())).unwrap();
    let e = r.resting(1).expect_err("still breached today");
    assert_eq!(e.code, RejectCode::PropRule);
    // Tighter is fine during a breach.
    r.set(Some(RulePreset {
        daily_loss_limit: Some(50),
        ..eod()
    }))
    .unwrap();
    assert_eq!(
        r.core
            .accounts()
            .find(|(c, _)| c.id == acct())
            .and_then(|(c, _)| c.rules.as_ref())
            .and_then(|p| p.daily_loss_limit),
        Some(50)
    );
}

#[test]
fn the_account_size_is_fixed() {
    let mut r = Rig::new(eod());
    let e = r
        .set(Some(RulePreset {
            account_size: 100_000,
            ..eod()
        }))
        .expect_err("another size");
    assert_eq!(e.code, RejectCode::Invalid);
    assert!(e.message.contains("50000"), "{}", e.message);
}

/// Looser rules still waiting are saved with the account and come back after a restart.
#[test]
fn waiting_rules_survive_a_restart() {
    let mut r = Rig::new(RulePreset {
        max_micro_contracts: Some(20),
        ..eod()
    });
    r.set(None).unwrap();
    let dir = std::env::temp_dir().join(format!("lines-prop-rules-{}", std::process::id()));
    let path = dir.join("paper.json");
    let file = trading_file(&r.core, &paper()).expect("a paper core saves");
    persist::save(&path, &file).unwrap();
    let back = persist::load(&path).unwrap().expect("saved");
    let _ = std::fs::remove_dir_all(&dir);
    let a = back.accounts.into_iter().next().unwrap();
    assert_eq!(
        a.pending_rules,
        Some(rules::PendingRules { rules: None }),
        "\"no prop rules\" waits"
    );
    assert_eq!(
        a.config.rules.as_ref().and_then(|p| p.max_micro_contracts),
        Some(20)
    );
    // Restored, they still wait.
    let id = a.config.id.clone();
    let rules_now = a.config.rules.clone();
    let mut core = TradingCore::new(Box::new(PaperBroker::from_state(back.paper, back.broker)));
    core.add_account(a.config, Some(a.tracker));
    core.restore_rules(&id, rules_now, a.pending_rules);
    assert!(core.pending_rules(&id).is_some());
}

/// A tighter rule the account is already past is refused with the reason: it would breach
/// (and liquidate) now, mid-order, or block the account's own exits.
#[test]
fn a_tightening_the_account_is_already_past_is_refused() {
    let mut r = Rig::new(eod());
    // Long 3 (30 micros), then down a few points on it.
    r.order(Side::Buy, 3, OrderKind::Market).expect("accepted");
    r.wait(LAT);
    r.book_at(5997.75, 5998.25);
    let e = r
        .set(Some(RulePreset {
            max_micro_contracts: Some(20),
            ..eod()
        }))
        .expect_err("30 micros are open");
    assert_eq!(e.code, RejectCode::Invalid);
    assert!(e.message.contains("30 micro"), "{}", e.message);
    let e = r
        .set(Some(RulePreset {
            daily_loss_limit: Some(100),
            ..eod()
        }))
        .expect_err("today's loss is past $100");
    assert!(e.message.contains("daily loss"), "{}", e.message);
    // Within what the account holds: fine.
    r.set(Some(RulePreset {
        max_micro_contracts: Some(30),
        ..eod()
    }))
    .expect("30 of 30");
    // Nothing was liquidated.
    assert!(
        !r.events.iter().any(|e| matches!(e, Event::Breach { .. })),
        "no breach from a setting"
    );
}

/// Settings commands never breach by themselves: an account whose saved high-water mark
/// locked its drawdown at the start balance, before its real balance is known, isn't failed.
#[test]
fn a_settings_change_never_breaches() {
    let mut cfg = AccountConfig::paper("paper-1", 50_000);
    let mut p = eod();
    p.liquidate_on_breach = true;
    cfg.rules = Some(p.clone());
    let mut tracker = rules::RuleTracker::new(Money::whole(50_000));
    tracker.high_water = Money::whole(52_100);
    tracker.locked = true;
    let mut b = PaperBroker::new(paper());
    b.open_account(cfg.id.clone(), Money::whole(50_000));
    let mut core = TradingCore::new(Box::new(b));
    core.add_account(cfg, Some(tracker));
    let t = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap();
    core.submit(
        t,
        Source::Test,
        Instant::now(),
        Command::SetRules {
            account: acct(),
            rules: Some(p),
        },
    );
    core.submit(
        t,
        Source::Test,
        Instant::now(),
        Command::SetGuardrails {
            account: acct(),
            guardrails: Default::default(),
        },
    );
    let events = core.drain();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Breach { .. })),
        "{events:?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Account(v) if v.blocked.is_none() && v.limits.account_size == 50_000
    )));
}
