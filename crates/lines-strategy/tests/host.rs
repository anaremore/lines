//! The strategy host (decision 0025) with the trading service and the paper broker: strategies
//! run on their own thread, their orders go through the core's strategy gate, and the host
//! pauses them itself on stale levels, a contract change or a panic. Synthetic data only.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_feed::levels::runtime::LevelStatus;
use lines_feed::levels::{LevelMap, LevelStyle, LevelValue};
use lines_feed::provider::MarketEvent;
use lines_strategy::example::LevelTouchExt;
use lines_strategy::host::{Instance, InstanceView, LevelFeed, StrategyHost};
use lines_strategy::{Ctx, Decision, Input, LevelView, Strategy, StrategyExtension};
use lines_trading::{
    AccountConfig, Clock, Command, HoursWindow, Money, PaperBroker, PaperConfig, Source,
    StrategyConfig, StrategyId, StrategyLimits, StrategyState, TradingCore, TradingHandle,
    TradingHours, TradingService,
};

const ID: StrategyId = StrategyId(3);

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn t0() -> i64 {
    // Tuesday 2026-11-10 15:00 UTC = 10:00 New York.
    UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0
}

/// A level source the test controls.
/// (revision, status, map).
type Shared = Arc<Mutex<(u64, LevelStatus, Option<Arc<LevelMap>>)>>;

#[derive(Clone)]
struct FakeLevels(Shared);

impl FakeLevels {
    fn new() -> FakeLevels {
        let map = LevelMap::builder("demo", catalog::ES, UtcMillis(t0()))
            .line(
                "support",
                LevelValue::Tick(Price(599_750)),
                Some("S1"),
                LevelStyle::default(),
            )
            .build(25)
            .unwrap();
        FakeLevels(Arc::new(Mutex::new((
            1,
            LevelStatus::Current,
            Some(Arc::new(map)),
        ))))
    }

    fn set_status(&self, s: LevelStatus) {
        let mut g = self.0.lock().unwrap();
        g.0 += 1;
        g.1 = s;
    }
}

impl LevelFeed for FakeLevels {
    fn source(&self) -> &'static str {
        "demo"
    }
    fn revision(&self) -> u64 {
        self.0.lock().unwrap().0
    }
    fn view(&self, _now: UtcMillis) -> LevelView {
        let g = self.0.lock().unwrap();
        LevelView {
            source: "demo",
            status: g.1,
            map: g.2.clone(),
            received: Some(UtcMillis(t0())),
        }
    }
}

struct Rig {
    _service: TradingService,
    handle: TradingHandle,
    host: StrategyHost,
    market: Sender<Arc<[MarketEvent]>>,
    levels: FakeLevels,
    t: i64,
}

fn config(account: &str) -> StrategyConfig {
    StrategyConfig {
        id: ID,
        label: "example #3".into(),
        account: lines_trading::AccountId::new(account),
        contract: es(),
        limits: StrategyLimits {
            max_position: 2,
            max_daily_loss: 500,
            max_orders_per_min: 10,
            hours: TradingHours {
                tz: "America/New_York".into(),
                windows: vec![HoursWindow::new("09:30", "16:00")],
            },
        },
    }
}

fn params() -> serde_json::Value {
    serde_json::json!({"kind": "support", "qty": 1, "stop_ticks": 8, "target_ticks": 8,
        "max_distance_ticks": 8})
}

impl Rig {
    fn new(ext: Arc<dyn StrategyExtension>, account: &str) -> Rig {
        let paper = PaperConfig {
            latency_ms: 0,
            check_sessions: false,
            ..Default::default()
        };
        let mut broker = PaperBroker::new(paper);
        broker.open_account(
            lines_trading::AccountId::new("paper-1"),
            Money::whole(50_000),
        );
        let mut core = TradingCore::new(Box::new(broker));
        core.add_account(AccountConfig::paper("paper-1", 50_000), None);
        let service = TradingService::start_with(core, None, Clock::Manual).unwrap();
        let handle = service.handle();
        handle.advance_to(UtcMillis(t0()));
        let host = StrategyHost::start(handle.clone(), Clock::Manual);
        let (tx, rx) = mpsc::channel();
        let levels = FakeLevels::new();
        host.add(Instance {
            ext,
            params: params(),
            config: config(account),
            product: catalog::get(catalog::ES).unwrap(),
            market: rx,
            levels: vec![Box::new(levels.clone())],
            stale_after: Duration::from_secs(3600),
        })
        .unwrap();
        Rig {
            _service: service,
            handle,
            host,
            market: tx,
            levels,
            t: t0(),
        }
    }

    /// The host's view once `ok` holds (bounded: the host and the core run on their own
    /// threads).
    fn until(&self, what: &str, ok: impl Fn(&InstanceView) -> bool) -> InstanceView {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            self.handle.sync();
            self.host.sync();
            if let Some(v) = self.host.snapshot().into_iter().find(|v| v.id == ID)
                && ok(&v)
            {
                return v;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for: {what}: {:#?}",
                self.host.snapshot()
            );
        }
    }

    fn arm(&self) {
        self.until("added", |_| true);
        self.handle
            .submit(Source::Test, Command::ArmStrategy { id: ID });
        self.until("armed", |v| v.state == StrategyState::Armed);
    }

    /// A trade to both the core (paper fills, marks) and the host (the strategy's feed).
    fn trade(&mut self, p: f64) {
        self.t += 1000;
        let time = UtcMillis(self.t);
        let price = Price((p * 100.0).round() as i64);
        let e = MarketEvent::Trade {
            time,
            price,
            qty: Qty(1),
            aggressor: None,
        };
        self.handle.market(
            es(),
            vec![lines_trading::MarketInput::Trade {
                time,
                price,
                qty: Qty(1),
            }],
        );
        self.market.send(Arc::from(vec![e])).unwrap();
    }
}

#[test]
fn a_strategy_trades_through_the_core_from_its_own_thread() {
    let mut r = Rig::new(Arc::new(LevelTouchExt::new("demo")), "paper-1");
    let v = r.until("added", |_| true);
    assert_eq!(v.state, StrategyState::Disarmed, "the host never arms");
    r.arm();
    r.trade(6000.00);
    r.trade(5999.00);
    let v = r.until("an order", |v| {
        v.decisions
            .iter()
            .any(|d| d.decision.rule == "touch-long" && matches!(d.result, Some(Ok(_))))
    });
    assert!(v.decisions[0].decision.reason.contains("S1 5997.50"));
    // Its order is in the core, placed by the strategy.
    let open: Vec<_> = r
        .until("still armed", |v| v.state == StrategyState::Armed)
        .decisions;
    assert_eq!(open.len(), 1);
}

#[test]
fn a_person_trading_the_same_contract_pauses_it() {
    let mut r = Rig::new(Arc::new(LevelTouchExt::new("demo")), "paper-1");
    r.arm();
    r.trade(6010.00);
    r.handle.submit(
        Source::Dom,
        Command::PlaceOrder(lines_trading::OrderSpec {
            account: lines_trading::AccountId::new("paper-1"),
            contract: es(),
            side: lines_trading::Side::Buy,
            qty: Qty(1),
            kind: lines_trading::OrderKind::Limit {
                price: Price(595_000),
            },
        }),
    );
    let v = r.until("paused", |v| v.state == StrategyState::Paused);
    assert!(v.reason.unwrap().contains("manual order activity"));
}

#[test]
fn stale_levels_pause_it() {
    let r = Rig::new(Arc::new(LevelTouchExt::new("demo")), "paper-1");
    r.arm();
    r.levels.set_status(LevelStatus::Stale { age_ms: 999_999 });
    let v = r.until("paused", |v| v.state == StrategyState::Paused);
    assert_eq!(v.reason.as_deref(), Some("levels demo are stale"));
}

#[test]
fn a_contract_change_pauses_it() {
    let r = Rig::new(Arc::new(LevelTouchExt::new("demo")), "paper-1");
    r.arm();
    let mar = ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap());
    r.market
        .send(Arc::from(vec![MarketEvent::ContractChanged(mar)]))
        .unwrap();
    let v = r.until("paused", |v| v.state == StrategyState::Paused);
    assert!(
        v.reason.unwrap().contains("ESH27"),
        "names the new contract"
    );
}

/// A strategy that panics on market data.
struct Panics;

impl Strategy for Panics {
    fn on_input(&mut self, _: &Ctx<'_>, input: Input<'_>, _: &mut Vec<Decision>) {
        if let Input::Market(_) = input {
            panic!("boom");
        }
    }
}

struct PanicsExt;

impl StrategyExtension for PanicsExt {
    fn name(&self) -> &str {
        "panics"
    }
    fn label(&self) -> &str {
        "Panics"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn serves(&self, _: &lines_core::Product) -> bool {
        true
    }
    fn level_sources(&self) -> &[&'static str] {
        &["demo"]
    }
    fn create(&self, _: &serde_json::Value) -> Result<Box<dyn Strategy>, String> {
        Ok(Box::new(Panics))
    }
}

#[test]
fn a_panicking_strategy_is_paused_and_never_called_again() {
    let mut r = Rig::new(Arc::new(PanicsExt), "paper-1");
    r.arm();
    r.trade(6000.00);
    let v = r.until("faulted and paused", |v| {
        v.fault.is_some() && v.state == StrategyState::Paused
    });
    assert_eq!(v.fault.as_deref(), Some("the strategy failed: boom"));
    // Re-armed by a person, it still never runs.
    r.handle
        .submit(Source::Test, Command::ArmStrategy { id: ID });
    r.trade(6001.00);
    r.until("armed, not called", |v| v.state == StrategyState::Armed);
}

#[test]
fn an_instance_the_core_refuses_faults() {
    let r = Rig::new(Arc::new(LevelTouchExt::new("demo")), "no-such-account");
    let v = r.until("faulted", |v| v.fault.is_some());
    assert!(v.fault.unwrap().contains("no account"), "names why");
}

#[test]
fn bad_parameters_are_refused_when_added() {
    let paper = PaperConfig::default();
    let core = TradingCore::new(Box::new(PaperBroker::new(paper)));
    let service = TradingService::start_with(core, None, Clock::Manual).unwrap();
    let host = StrategyHost::start(service.handle(), Clock::Manual);
    let (_tx, rx) = mpsc::channel();
    let e = host
        .add(Instance {
            ext: Arc::new(LevelTouchExt::new("demo")),
            params: serde_json::json!({"kind": "support"}),
            config: config("paper-1"),
            product: catalog::get(catalog::ES).unwrap(),
            market: rx,
            levels: vec![Box::new(FakeLevels::new())],
            stale_after: Duration::from_secs(60),
        })
        .unwrap_err();
    assert!(e.contains("missing field"), "{e}");
    let (_tx, rx) = mpsc::channel();
    let e = host
        .add(Instance {
            ext: Arc::new(LevelTouchExt::new("demo")),
            params: params(),
            config: config("paper-1"),
            product: catalog::get(catalog::ES).unwrap(),
            market: rx,
            levels: Vec::new(),
            stale_after: Duration::from_secs(60),
        })
        .unwrap_err();
    assert!(e.contains("needs the level sources"), "{e}");
}
