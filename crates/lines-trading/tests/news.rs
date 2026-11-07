//! News blackout (roadmap 8b) through the real core and the paper broker, on a manual clock:
//! the window's edges, exits always allowed, flatten at the window's start, warn, a prop
//! preset's rule, DST, and a missing or stale calendar (never taken as clear).

use std::sync::Arc;
use std::time::Instant;

use lines_core::econ::EconSchedule;
use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::guardrails::Guardrails;
use lines_trading::news::{NewsAction, NewsBlackout};
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FillModel, MarketInput, Money, OrderId, OrderKind,
    OrderSpec, PaperBroker, PaperConfig, Reject, RejectCode, RulePreset, Side, Source, TradingCore,
};

const LAT: i64 = 20;
const MIN: i64 = 60_000;

/// CPI at 08:30 Eastern on both sides of the March DST change, and a jobs report in November.
const SCHEDULE: &str = "\
format 1
version 2026-11-01
zone America/New_York
release cpi
name CPI
agency BLS
importance high
affects ES MES NQ MNQ ZN
coverage 2026-03-01 2026-12-31 verified
source https://www.bls.gov/schedule/news_release/cpi.htm
2026-03-06 08:30 verified February 2026
2026-03-10 08:30 verified March 2026
2026-11-10 08:30 verified October 2026
release ppi
name PPI
agency BLS
importance medium
affects ES MES NQ MNQ ZN
coverage 2026-03-01 2026-12-31 verified
source https://www.bls.gov/schedule/news_release/ppi.htm
2026-11-12 08:30 verified October 2026
";

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn btc() -> ContractKey {
    ContractKey::spot(catalog::BTC_USD)
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

fn at(iso: &str) -> i64 {
    UtcMillis::parse_iso(iso).unwrap().0
}

/// CPI 2026-11-10 08:30 EST.
fn cpi() -> i64 {
    at("2026-11-10T13:30:00Z")
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    fn with(g: Guardrails, preset: Option<RulePreset>, schedule: Option<&str>) -> Rig {
        let mut cfg = AccountConfig::paper("paper-1", 50_000);
        cfg.rules = preset;
        let mut b = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            fill_model: FillModel::TradeThrough,
            ..Default::default()
        });
        b.open_account(cfg.id.clone(), Money::whole(cfg.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.set_econ_schedule(schedule.map(|s| Arc::new(EconSchedule::parse(s).unwrap())));
        core.add_account(cfg, None);
        let mut r = Rig {
            core,
            t: cpi() - 30 * MIN,
            events: Vec::new(),
        };
        r.cmd(Command::SetGuardrails {
            account: acct(),
            guardrails: g,
        })
        .unwrap();
        r.book();
        r
    }

    fn new(action: NewsAction) -> Rig {
        Rig::with(
            Guardrails {
                news: Some(NewsBlackout::new(action)),
                ..Default::default()
            },
            None,
            Some(SCHEDULE),
        )
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

    fn order_on(
        &mut self,
        contract: ContractKey,
        side: Side,
        kind: OrderKind,
    ) -> Result<(), Reject> {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract,
            side,
            qty: Qty(1),
            kind,
        }))
        .map(|_| ())
    }

    fn buy_limit(&mut self) -> Result<(), Reject> {
        self.order_on(es(), Side::Buy, OrderKind::Limit { price: px(5990.00) })
    }

    fn denied(&mut self) -> String {
        let r = self.buy_limit().expect_err("denied");
        assert_eq!(r.code, RejectCode::Guardrail, "{r:?}");
        r.message
    }

    fn market_fill(&mut self, side: Side) {
        self.order_on(es(), side, OrderKind::Market)
            .expect("accepted");
        self.wait(LAT);
    }

    /// Move the clock to `t` and let the core run (as the service does at its wake times).
    fn go(&mut self, t: i64) {
        self.t = t;
        self.core.tick(self.now());
        self.events.extend(self.core.drain());
        self.book();
    }

    fn wait(&mut self, ms: i64) {
        self.go(self.t + ms);
    }

    fn book(&mut self) {
        self.t += 1;
        let v = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: px(5999.75),
                qty: Qty(100),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(6000.25),
                qty: Qty(100),
            },
            MarketInput::Last(px(5999.75)),
        ];
        self.core.market(self.now(), es(), &v);
        self.events.extend(self.core.drain());
        self.t -= 1;
    }

    fn position(&self) -> i64 {
        self.core.position(&acct(), es())
    }

    fn rule_flattens(&self) -> usize {
        self.events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Command {
                        source: Source::Rule,
                        result: Ok(_),
                        ..
                    }
                )
            })
            .count()
    }
}

#[test]
fn block_holds_from_two_minutes_before_to_two_after() {
    let mut r = Rig::new(NewsAction::Block);
    r.t = cpi() - 2 * MIN - 1;
    r.buy_limit().expect("just before the window");
    r.t = cpi() - 2 * MIN;
    assert_eq!(r.denied(), "News blackout: CPI, October 2026, 4 min left");
    r.t = cpi() + 2 * MIN - 1;
    assert_eq!(r.denied(), "News blackout: CPI, October 2026, 1 min left");
    r.t = cpi() + 2 * MIN;
    r.buy_limit().expect("the window has ended");
    // A medium release is outside the default rule.
    r.t = at("2026-11-12T13:30:00Z");
    r.buy_limit().expect("PPI is medium importance");
}

#[test]
fn exits_flatten_and_cancels_always_pass() {
    let mut r = Rig::new(NewsAction::Block);
    r.market_fill(Side::Buy);
    assert_eq!(r.position(), 1);
    r.t = cpi();
    r.denied();
    // Adding to the position is an entry; closing it isn't.
    r.order_on(es(), Side::Sell, OrderKind::Limit { price: px(6010.00) })
        .expect("an exit");
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: None,
    })
    .expect("cancel all");
    r.cmd(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .expect("flatten");
    r.wait(LAT);
    assert_eq!(r.position(), 0);
}

#[test]
fn flatten_closes_affected_positions_once_when_the_window_opens() {
    let mut r = Rig::new(NewsAction::Flatten);
    r.market_fill(Side::Buy);
    assert_eq!(r.position(), 1);
    // The core asks to be woken exactly when the window opens.
    assert_eq!(r.core.next_wake(), Some(UtcMillis(cpi() - 2 * MIN)));
    r.go(cpi() - 2 * MIN - 1);
    assert_eq!(r.position(), 1, "not before the window");
    assert_eq!(r.rule_flattens(), 0);
    r.go(cpi() - 2 * MIN);
    r.wait(LAT);
    assert_eq!(r.position(), 0);
    assert_eq!(r.rule_flattens(), 1);
    // Later ticks in the same window don't flatten again; entries stay refused.
    r.wait(MIN);
    assert_eq!(r.rule_flattens(), 1);
    r.denied();
    // After the window: trading as usual.
    r.go(cpi() + 2 * MIN);
    r.market_fill(Side::Buy);
    assert_eq!(r.position(), 1);
}

#[test]
fn warn_refuses_nothing_and_never_flattens() {
    let mut r = Rig::new(NewsAction::Warn);
    r.market_fill(Side::Buy);
    r.go(cpi());
    r.buy_limit().expect("warn only");
    assert_eq!(r.position(), 1);
    assert_eq!(r.rule_flattens(), 0);
}

#[test]
fn products_the_release_does_not_move_trade_on() {
    let mut r = Rig::new(NewsAction::Block);
    r.t = cpi();
    // BTC/USD isn't in CPI's list: the blackout doesn't apply (whatever else refuses it).
    if let Err(e) = r.order_on(btc(), Side::Buy, OrderKind::Limit { price: Price(100) }) {
        assert_ne!(e.code, RejectCode::Guardrail, "{e:?}");
    }
}

#[test]
fn a_preset_rule_combines_with_the_guardrail() {
    let preset = RulePreset {
        name: "Eval".into(),
        label: lines_trading::rules::APPROXIMATION.into(),
        account_size: 50_000,
        trailing_drawdown: None,
        daily_loss_limit: None,
        max_micro_contracts: None,
        profit_target: None,
        consistency: None,
        liquidate_on_breach: false,
        on_breach: Default::default(),
        news: Some(NewsBlackout {
            before_min: 10,
            ..NewsBlackout::new(NewsAction::Block)
        }),
    };
    // The trader's own rule only warns, for a minute: the preset's block and its wider window
    // win.
    let mut r = Rig::with(
        Guardrails {
            news: Some(NewsBlackout {
                before_min: 1,
                after_min: 5,
                ..NewsBlackout::new(NewsAction::Warn)
            }),
            ..Default::default()
        },
        Some(preset),
        Some(SCHEDULE),
    );
    let rule = r.core.news_rule(&acct()).unwrap();
    assert_eq!(
        (rule.action, rule.before_min, rule.after_min),
        (NewsAction::Block, 10, 5)
    );
    r.t = cpi() - 10 * MIN;
    r.denied();
    r.t = cpi() + 5 * MIN - 1;
    r.denied();
    r.t = cpi() + 5 * MIN;
    r.buy_limit().expect("after both windows");
}

#[test]
fn eastern_release_times_follow_dst() {
    // 08:30 EST is 13:30 UTC on March 6; 08:30 EDT is 12:30 UTC on March 10.
    for (window_open, clear) in [
        ("2026-03-06T13:28:00Z", "2026-03-06T12:28:00Z"),
        ("2026-03-10T12:28:00Z", "2026-03-10T13:28:00Z"),
    ] {
        let mut r = Rig::new(NewsAction::Block);
        r.t = at(window_open);
        r.book();
        assert!(r.denied().starts_with("News blackout: CPI, "));
        r.t = at(clear);
        r.book();
        r.buy_limit().expect("an hour off is outside the window");
    }
}

#[test]
fn a_missing_or_stale_calendar_is_never_taken_as_clear() {
    // No calendar at all.
    let mut r = Rig::with(
        Guardrails {
            news: Some(NewsBlackout::new(NewsAction::Block)),
            ..Default::default()
        },
        None,
        None,
    );
    assert_eq!(
        r.denied(),
        "News blackout: no economic calendar loaded; can't confirm no release, so entries are refused"
    );
    // Past the calendar's coverage.
    let mut r = Rig::new(NewsAction::Flatten);
    r.t = at("2027-01-05T15:00:00Z");
    r.book();
    assert_eq!(
        r.denied(),
        "News blackout: economic calendar missing CPI for 2027-01-05; can't confirm no release, so entries are refused"
    );
    // Covered, but checked too long ago.
    let stale = SCHEDULE.replace("version 2026-11-01", "version 2026-08-01");
    let mut r = Rig::with(
        Guardrails {
            news: Some(NewsBlackout::new(NewsAction::Block)),
            ..Default::default()
        },
        None,
        Some(&stale),
    );
    r.t = at("2026-11-09T15:00:00Z");
    assert_eq!(
        r.denied(),
        "News blackout: economic calendar stale (checked 2026-08-01); can't confirm no release, so entries are refused"
    );
    // Warn never refuses, even without a calendar; no rule at all, nothing is asked.
    let mut r = Rig::with(
        Guardrails {
            news: Some(NewsBlackout::new(NewsAction::Warn)),
            ..Default::default()
        },
        None,
        None,
    );
    r.buy_limit().expect("warn");
    let mut r = Rig::with(Guardrails::default(), None, None);
    r.buy_limit().expect("no rule");
}

#[test]
fn the_rule_is_saved_with_the_guardrails_and_older_files_read() {
    let g = Guardrails {
        news: Some(NewsBlackout::new(NewsAction::Flatten)),
        ..Default::default()
    };
    let j = serde_json::to_string(&g).unwrap();
    assert_eq!(
        j,
        r#"{"news":{"action":"flatten","before_min":2,"after_min":2}}"#
    );
    assert_eq!(serde_json::from_str::<Guardrails>(&j).unwrap(), g);
    // A file from before the rule has none.
    let old: Guardrails = serde_json::from_str(r#"{"max_trades_per_day":3}"#).unwrap();
    assert_eq!(old.news, None);
    // Tighter now: a rule where there was none applies at once.
    let mut t = lines_trading::GuardrailTracker::default();
    t.request(g.clone());
    assert_eq!(t.active, g);
    assert_eq!(t.pending, None);
    // Looser (removing it) waits for the next trade date.
    t.request(Guardrails::default());
    assert_eq!(t.active, g);
    assert_eq!(t.pending, Some(Guardrails::default()));
}
