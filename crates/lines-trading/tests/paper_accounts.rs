//! Adding and removing paper accounts by command (`core/accounts.rs`): the new account is the
//! broker's and the core's and is saved; a remove is refused while anything is left on the
//! account, and takes it out of its groups. Deterministic: time is passed in.

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::group::AccountGroup;
use lines_trading::persist;
use lines_trading::service::trading_file;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FillModel, MarketInput, Money, OrderId, OrderKind,
    OrderSpec, PaperBroker, PaperConfig, RejectCode, Side, Source, TradingCore,
};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn paper() -> PaperConfig {
    PaperConfig {
        latency_ms: 20,
        fill_model: FillModel::TradeThrough,
        ..Default::default()
    }
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    /// One paper account `a` ($50,000).
    fn new() -> Rig {
        let mut b = PaperBroker::new(paper());
        b.open_account(AccountId::new("a"), Money::whole(50_000));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(AccountConfig::paper("a", 50_000), None);
        Rig {
            core,
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
        }
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn cmd_from(
        &mut self,
        source: Source,
        c: Command,
    ) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let cid = self.core.submit(self.now(), source, Instant::now(), c);
        self.events.extend(self.core.drain());
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == cid => Some(result.clone()),
                _ => None,
            })
            .expect("every command has a result")
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
        self.cmd_from(Source::Test, c)
    }

    fn add(&mut self, name: &str, balance: i64) -> Result<AccountId, lines_trading::Reject> {
        self.events.clear();
        self.cmd(Command::AddPaperAccount {
            name: name.into(),
            starting_balance: balance,
            rules: None,
        })?;
        Ok(self
            .events
            .iter()
            .find_map(|e| match e {
                Event::AccountAdded { config } => Some(config.id.clone()),
                _ => None,
            })
            .expect("an added account is announced"))
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.events.extend(self.core.drain());
    }

    fn market(&mut self, bid: f64, ask: f64, last: f64) {
        self.t += 1;
        let t = UtcMillis(self.t);
        self.core.market(
            self.now(),
            es(),
            &[
                MarketInput::BookClear,
                MarketInput::Book {
                    side: BookSide::Bid,
                    price: px(bid),
                    qty: Qty(50),
                },
                MarketInput::Book {
                    side: BookSide::Ask,
                    price: px(ask),
                    qty: Qty(50),
                },
                MarketInput::Trade {
                    time: t,
                    price: px(last),
                    qty: Qty(1),
                },
            ],
        );
        self.events.extend(self.core.drain());
    }
}

#[test]
fn a_new_paper_account_trades_and_is_saved() {
    let mut r = Rig::new();
    let id = r.add("  Eval 2 ", 25_000).expect("added");
    assert_eq!(id.0, "paper-eval-2");
    let (cfg, _) = r
        .core
        .accounts()
        .find(|(c, _)| c.id == id)
        .expect("the core has it");
    assert_eq!(
        (cfg.name.as_str(), cfg.starting_balance),
        ("Eval 2", 25_000)
    );
    // Its view comes at once, at its starting balance.
    assert!(r.events.iter().any(|e| matches!(
        e,
        Event::Account(v) if v.account == id && v.balance == Money::whole(25_000)
    )));
    // It trades like any paper account.
    r.market(5999.75, 6000.0, 6000.0);
    let placed = r.cmd(Command::PlaceOrder(OrderSpec {
        account: id.clone(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Market,
    }));
    assert!(placed.is_ok(), "{placed:?}");
    // Saved with the rest: the trading file lists it, and the broker's state holds it.
    let file = trading_file(&r.core, &paper()).expect("paper");
    assert!(file.accounts.iter().any(|a| a.config.id == id));
    assert!(file.broker.accounts.contains_key(&id.0));
    let dir = lines_core::scratch::Scratch::new("paper-accounts");
    let path = dir.path().join("t.json");
    persist::save(&path, &file).unwrap();
    let back = persist::load(&path).unwrap().unwrap();
    assert!(back.accounts.iter().any(|a| a.config.name == "Eval 2"));
}

#[test]
fn names_are_unique_and_balances_and_rules_checked() {
    let mut r = Rig::new();
    r.add("Evals", 50_000).unwrap();
    let e = r.add("evals", 50_000).unwrap_err();
    assert_eq!(e.code, RejectCode::Invalid);
    assert!(e.message.contains("already an account"), "{}", e.message);
    assert_eq!(r.add(" ", 50_000).unwrap_err().code, RejectCode::Invalid);
    assert_eq!(r.add("Zero", 0).unwrap_err().code, RejectCode::Invalid);
    // Prop rules are for their account size.
    let preset = lines_trading::rules::presets().remove(0);
    let wrong = r.cmd(Command::AddPaperAccount {
        name: "Prop".into(),
        starting_balance: preset.account_size + 1,
        rules: Some(preset.clone()),
    });
    assert_eq!(wrong.unwrap_err().code, RejectCode::Invalid);
    r.cmd(Command::AddPaperAccount {
        name: "Prop".into(),
        starting_balance: preset.account_size,
        rules: Some(preset.clone()),
    })
    .expect("its size");
    let (cfg, _) = r.core.accounts().find(|(c, _)| c.name == "Prop").unwrap();
    assert_eq!(cfg.rules.as_ref(), Some(&preset));
    // Two names with the same letters get two ids.
    let one = r.add("X 1", 1_000).unwrap();
    let two = r.add("X-1", 1_000).unwrap();
    assert_ne!(one, two);
}

#[test]
fn a_strategy_cant_add_or_remove_accounts() {
    let mut r = Rig::new();
    let s = Source::Strategy(lines_trading::strategy::StrategyId(1));
    let add = r.cmd_from(
        s,
        Command::AddPaperAccount {
            name: "Bot".into(),
            starting_balance: 1_000,
            rules: None,
        },
    );
    assert_eq!(add.unwrap_err().code, RejectCode::Strategy);
    let rm = r.cmd_from(
        s,
        Command::RemovePaperAccount {
            account: AccountId::new("a"),
        },
    );
    assert_eq!(rm.unwrap_err().code, RejectCode::Strategy);
}

#[test]
fn a_remove_waits_until_nothing_is_left_and_leaves_its_groups() {
    let mut r = Rig::new();
    let b = r.add("B", 50_000).unwrap();
    r.cmd(Command::CreateGroup {
        name: "Evals".into(),
        members: vec![AccountId::new("a"), b.clone()],
    })
    .unwrap();
    r.market(5999.75, 6000.0, 6000.0);
    // A working order: refused, and nothing changes.
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: b.clone(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit { price: px(5990.0) },
    }))
    .unwrap();
    r.wait(50);
    let rm = Command::RemovePaperAccount { account: b.clone() };
    let e = r.cmd(rm.clone()).unwrap_err();
    assert!(e.message.contains("cancel its orders"), "{}", e.message);
    // A position: refused too.
    r.cmd(Command::CancelAll {
        account: b.clone(),
        contract: None,
    })
    .unwrap();
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: b.clone(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Market,
    }))
    .unwrap();
    r.wait(50);
    let e = r.cmd(rm.clone()).unwrap_err();
    assert!(e.message.contains("flatten it first"), "{}", e.message);
    assert!(r.core.accounts().any(|(c, _)| c.id == b));
    // Flat with nothing open: removed, out of its group first.
    r.cmd(Command::Flatten {
        account: b.clone(),
        contract: None,
    })
    .unwrap();
    r.wait(50);
    r.market(5999.75, 6000.0, 6000.0);
    r.wait(50);
    r.events.clear();
    r.cmd(rm).expect("removed");
    assert!(!r.core.accounts().any(|(c, _)| c.id == b));
    let groups_at = r
        .events
        .iter()
        .position(|e| matches!(e, Event::Groups { .. }))
        .expect("its group changed");
    let removed_at = r
        .events
        .iter()
        .position(|e| matches!(e, Event::AccountRemoved { account } if *account == b))
        .expect("announced");
    assert!(groups_at < removed_at);
    assert_eq!(
        r.core.groups(),
        &[AccountGroup {
            name: "Evals".into(),
            members: vec![AccountId::new("a")],
        }]
    );
    let file = trading_file(&r.core, &paper()).unwrap();
    assert!(!file.accounts.iter().any(|a| a.config.id == b));
    assert!(!file.broker.accounts.contains_key(&b.0));
}

#[test]
fn the_last_account_stays() {
    let mut r = Rig::new();
    let e = r
        .cmd(Command::RemovePaperAccount {
            account: AccountId::new("a"),
        })
        .unwrap_err();
    assert!(e.message.contains("only account"), "{}", e.message);
    let e = r
        .cmd(Command::RemovePaperAccount {
            account: AccountId::new("nobody"),
        })
        .unwrap_err();
    assert_eq!(e.code, RejectCode::UnknownAccount);
}

#[test]
fn a_removed_id_is_never_given_again_and_is_saved() {
    let mut r = Rig::new();
    let first = r.add("Eval 2", 50_000).unwrap();
    r.cmd(Command::RemovePaperAccount {
        account: first.clone(),
    })
    .unwrap();
    // The same name again: a new id (the old one's journal trades keep it).
    let again = r.add("Eval 2", 50_000).unwrap();
    assert_ne!(again, first);
    let file = trading_file(&r.core, &paper()).unwrap();
    assert_eq!(file.retired_accounts, vec![first.clone()]);
    // After a restart too.
    let mut core = TradingCore::new(Box::new(PaperBroker::new(paper())));
    core.add_account(AccountConfig::paper("a", 50_000), None);
    core.restore_retired(file.retired_accounts);
    let mut r2 = Rig {
        core,
        t: r.t,
        events: Vec::new(),
    };
    let id = r2.add("Eval 2", 50_000).unwrap();
    assert_ne!(id, first);
}

#[test]
fn a_new_account_takes_the_runs_limits_and_its_guardrails() {
    let mut r = Rig::new();
    r.core.set_new_paper_limits(lines_trading::RiskLimits {
        max_order_qty: Some(2),
        ..Default::default()
    });
    let mut wanted = std::collections::BTreeMap::new();
    let g = lines_trading::Guardrails {
        max_trades_per_day: Some(3),
        ..Default::default()
    };
    wanted.insert("Small".to_owned(), g.clone());
    r.core.set_join_guardrails(wanted, Default::default());
    let id = r.add("Small", 10_000).unwrap();
    let (cfg, _) = r.core.accounts().find(|(c, _)| c.id == id).unwrap();
    assert_eq!(cfg.limits.max_order_qty, Some(2));
    assert_eq!(r.core.guardrails(&id).unwrap().active, g);
    r.market(5999.75, 6000.0, 6000.0);
    let big = r.cmd(Command::PlaceOrder(OrderSpec {
        account: id,
        contract: es(),
        side: Side::Buy,
        qty: Qty(3),
        kind: OrderKind::Market,
    }));
    assert!(big.is_err(), "over the run's largest order");
}

#[test]
fn a_remove_waits_for_its_strategies() {
    use lines_trading::{HoursWindow, StrategyConfig, StrategyId, StrategyLimits, TradingHours};
    let mut r = Rig::new();
    let b = r.add("B", 50_000).unwrap();
    r.cmd(Command::AddStrategy(StrategyConfig {
        id: StrategyId(7),
        label: "example #7".into(),
        account: b.clone(),
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
    let e = r
        .cmd(Command::RemovePaperAccount { account: b.clone() })
        .unwrap_err();
    assert!(e.message.contains("remove its strategies"), "{}", e.message);
    r.cmd(Command::RemoveStrategy { id: StrategyId(7) })
        .unwrap();
    r.cmd(Command::RemovePaperAccount { account: b }).unwrap();
}
