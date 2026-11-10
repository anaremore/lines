//! Account groups (decision 0006; roadmap 6): managing them by command, the group Cancel All
//! and Flatten (one member command and one result per member, never gated by risk, prop rules
//! or guardrails), and their place in the trading file. Deterministic: time is passed in.

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::group::{AccountGroup, MemberResult};
use lines_trading::persist;
use lines_trading::service::trading_file;
use lines_trading::strategy::StrategyId;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FillModel, MarketInput, Money, OrderId, OrderKind,
    OrderSpec, OrderState, PaperBroker, PaperConfig, RejectCode, RiskLimits, Side, Source,
    TradingCore,
};

const LAT: i64 = 20;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn nq() -> ContractKey {
    ContractKey::monthly(catalog::NQ, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn id(s: &str) -> AccountId {
    AccountId::new(s)
}

fn ids(v: &[&str]) -> Vec<AccountId> {
    v.iter().map(|s| id(s)).collect()
}

fn group(name: &str, members: &[&str]) -> AccountGroup {
    AccountGroup {
        name: name.into(),
        members: ids(members),
    }
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

fn paper() -> PaperConfig {
    PaperConfig {
        latency_ms: LAT,
        fill_model: FillModel::TradeThrough,
        ..Default::default()
    }
}

impl Rig {
    /// Accounts `a`, `b` and `c`, $50,000 each; `b` has a $100 daily loss limit.
    fn new() -> Rig {
        let mut b = PaperBroker::new(paper());
        for a in ["a", "b", "c"] {
            b.open_account(id(a), Money::whole(50_000));
        }
        let mut core = TradingCore::new(Box::new(b));
        for a in ["a", "b", "c"] {
            let mut cfg = AccountConfig::paper(a, 50_000);
            if a == "b" {
                cfg.limits = RiskLimits {
                    daily_loss: Some(100),
                    ..Default::default()
                };
            }
            core.add_account(cfg, None);
        }
        Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
        }
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd_from(
        &mut self,
        source: Source,
        c: Command,
    ) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let cid = self.core.submit(self.now(), source, Instant::now(), c);
        self.collect();
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

    fn place(
        &mut self,
        account: &str,
        contract: ContractKey,
        side: Side,
        qty: i64,
        kind: OrderKind,
    ) -> OrderId {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: id(account),
            contract,
            side,
            qty: Qty(qty),
            kind,
        }))
        .expect("accepted")[0]
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn market(&mut self, contract: ContractKey, bid: f64, ask: f64, last: f64) {
        self.t += 1;
        let t = UtcMillis(self.t);
        self.core.market(
            self.now(),
            contract,
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
        self.collect();
    }

    fn last_groups(&self) -> Vec<AccountGroup> {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Groups { groups } => Some(groups.clone()),
                _ => None,
            })
            .expect("groups published")
    }

    /// The newest `GroupCommand` event's group and results.
    fn group_results(&self) -> (String, Vec<MemberResult>) {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::GroupCommand { group, results, .. } => {
                    Some((group.clone(), results.clone()))
                }
                _ => None,
            })
            .expect("a group command's results")
    }
}

fn create(
    r: &mut Rig,
    name: &str,
    members: &[&str],
) -> Result<Vec<OrderId>, lines_trading::Reject> {
    r.cmd(Command::CreateGroup {
        name: name.into(),
        members: ids(members),
    })
}

/// Create, rename, set members, delete: each publishes every group, and the checks refuse what
/// would be ambiguous or wrong (a name taken in any letter case, an unknown or repeated member,
/// an unknown group). Nothing touches the accounts.
#[test]
fn groups_are_managed_by_command() {
    let mut r = Rig::new();
    create(&mut r, " Evals ", &["a", "b"]).unwrap();
    assert_eq!(r.core.groups(), [group("Evals", &["a", "b"])]);
    assert_eq!(r.last_groups(), vec![group("Evals", &["a", "b"])]);

    let e = create(&mut r, "evals", &[]).unwrap_err();
    assert_eq!(e.code, RejectCode::Invalid, "{}", e.message);
    assert_eq!(
        create(&mut r, " ", &[]).unwrap_err().code,
        RejectCode::Invalid
    );
    assert_eq!(
        create(&mut r, "Funded", &["a", "nobody"]).unwrap_err().code,
        RejectCode::UnknownAccount
    );
    assert_eq!(
        create(&mut r, "Funded", &["a", "a"]).unwrap_err().code,
        RejectCode::Invalid
    );
    // An account may be in several groups.
    create(&mut r, "Funded", &["a"]).unwrap();

    // A rename: to a free name, or its own name in other case; never to another group's.
    let e = r
        .cmd(Command::RenameGroup {
            group: "evals".into(),
            name: "FUNDED".into(),
        })
        .unwrap_err();
    assert_eq!(e.code, RejectCode::Invalid);
    r.cmd(Command::RenameGroup {
        group: "evals".into(),
        name: "EVALS".into(),
    })
    .unwrap();
    r.cmd(Command::RenameGroup {
        group: "EVALS".into(),
        name: "Combine".into(),
    })
    .unwrap();

    r.cmd(Command::SetGroupMembers {
        group: "combine".into(),
        members: ids(&["c", "b"]),
    })
    .unwrap();
    assert_eq!(
        r.cmd(Command::SetGroupMembers {
            group: "combine".into(),
            members: ids(&["c", "c"]),
        })
        .unwrap_err()
        .code,
        RejectCode::Invalid
    );
    assert_eq!(
        r.last_groups(),
        vec![group("Combine", &["c", "b"]), group("Funded", &["a"])]
    );

    r.cmd(Command::DeleteGroup {
        group: "Funded".into(),
    })
    .unwrap();
    assert_eq!(r.core.groups(), [group("Combine", &["c", "b"])]);
    for c in [
        Command::DeleteGroup {
            group: "Funded".into(),
        },
        Command::RenameGroup {
            group: "Funded".into(),
            name: "X".into(),
        },
        Command::SetGroupMembers {
            group: "Funded".into(),
            members: Vec::new(),
        },
        Command::GroupFlatten {
            group: "Funded".into(),
            contract: None,
        },
        Command::GroupCancelAll {
            group: "Funded".into(),
            contract: None,
        },
    ] {
        assert_eq!(r.cmd(c).unwrap_err().code, RejectCode::UnknownGroup);
    }
    // The accounts are all still there.
    assert_eq!(r.core.accounts().count(), 3);
}

/// A strategy trades its own account: it can't manage groups or act on one.
#[test]
fn a_strategy_cant_manage_or_act_on_groups() {
    let mut r = Rig::new();
    create(&mut r, "Evals", &["a", "b"]).unwrap();
    let s = Source::Strategy(StrategyId(1));
    for c in [
        Command::CreateGroup {
            name: "Mine".into(),
            members: Vec::new(),
        },
        Command::DeleteGroup {
            group: "Evals".into(),
        },
        Command::GroupFlatten {
            group: "Evals".into(),
            contract: None,
        },
        Command::GroupCancelAll {
            group: "Evals".into(),
            contract: None,
        },
    ] {
        assert_eq!(r.cmd_from(s, c).unwrap_err().code, RejectCode::Strategy);
    }
    assert_eq!(r.core.groups(), [group("Evals", &["a", "b"])]);
}

/// A group Flatten is each member's own Flatten: every member closes, `b` too although its
/// daily loss blocks new orders (never gated by risk or rules), and an account outside the
/// group is untouched. One result per member, in the group's order, each its own command's.
#[test]
fn a_group_flatten_closes_every_member_and_nothing_else() {
    let mut r = Rig::new();
    create(&mut r, "Evals", &["b", "a"]).unwrap();
    r.market(es(), 5999.75, 6000.25, 6000.00);
    for (a, q) in [("a", 2), ("b", 2), ("c", 1)] {
        r.place(a, es(), Side::Buy, q, OrderKind::Market);
    }
    let resting = r.place(
        "a",
        es(),
        Side::Sell,
        1,
        OrderKind::Limit { price: px(6010.00) },
    );
    r.wait(LAT);
    assert_eq!(r.core.position(&id("b"), es()), 2);
    // Down 2 points: b is $200 down, past its $100 daily loss: new orders are refused.
    r.market(es(), 5997.75, 5998.25, 5998.00);
    let blocked = r.cmd(Command::PlaceOrder(OrderSpec {
        account: id("b"),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit { price: px(5999.00) },
    }));
    assert_eq!(blocked.unwrap_err().code, RejectCode::Risk);

    let sent = r
        .cmd(Command::GroupFlatten {
            group: "evals".into(),
            contract: None,
        })
        .unwrap();
    let (name, results) = r.group_results();
    assert_eq!(name, "Evals");
    assert_eq!(
        results
            .iter()
            .map(|m| m.account.clone())
            .collect::<Vec<_>>(),
        ids(&["b", "a"])
    );
    // Each member's result is its own command's, and the group's names their orders.
    let mut named = Vec::new();
    for m in &results {
        let own = r
            .events
            .iter()
            .find_map(|e| match e {
                Event::Command {
                    id, result, source, ..
                } if *id == m.command => {
                    assert_eq!(*source, Source::Test, "the group command's source");
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("each member command has its own result");
        assert_eq!(own, m.result);
        named.extend(m.result.clone().unwrap());
    }
    assert_eq!(sent, named);
    assert!(!sent.is_empty());
    // In order: each member's own result, then the group's results, then the group's answer.
    let at = |f: &dyn Fn(&Event) -> bool| r.events.iter().position(|e| f(e)).unwrap();
    let members: Vec<usize> = results
        .iter()
        .map(|m| at(&|e| matches!(e, Event::Command { id, .. } if *id == m.command)))
        .collect();
    let listed = at(&|e| matches!(e, Event::GroupCommand { .. }));
    let answered = at(&|e| matches!(e, Event::Command { result: Ok(o), .. } if *o == sent));
    assert!(members.windows(2).all(|w| w[0] < w[1]));
    assert!(members.iter().all(|m| *m < listed) && listed < answered);

    r.wait(LAT);
    r.market(es(), 5997.75, 5998.25, 5998.00);
    r.wait(LAT);
    assert_eq!(r.core.position(&id("a"), es()), 0);
    assert_eq!(r.core.position(&id("b"), es()), 0);
    assert_eq!(r.core.order(resting).unwrap().state, OrderState::Cancelled);
    assert_eq!(r.core.position(&id("c"), es()), 1, "not in the group");
}

/// A group Cancel All cancels the members' orders, of one contract when it names one; other
/// accounts' orders stay.
#[test]
fn a_group_cancel_all_cancels_the_members_orders() {
    let mut r = Rig::new();
    create(&mut r, "Evals", &["a", "b"]).unwrap();
    r.market(es(), 5999.75, 6000.25, 6000.00);
    r.market(nq(), 20999.75, 21000.25, 21000.00);
    let limit = |p: f64| OrderKind::Limit { price: px(p) };
    let a_es = r.place("a", es(), Side::Buy, 1, limit(5990.00));
    let b_es = r.place("b", es(), Side::Buy, 1, limit(5990.00));
    let b_nq = r.place("b", nq(), Side::Buy, 1, limit(20990.00));
    let c_es = r.place("c", es(), Side::Buy, 1, limit(5990.00));
    r.wait(LAT);

    let sent = r
        .cmd(Command::GroupCancelAll {
            group: "Evals".into(),
            contract: Some(es()),
        })
        .unwrap();
    assert_eq!(sent.len(), 2);
    let (_, results) = r.group_results();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|m| m.result.is_ok()));
    r.wait(LAT);
    let state = |r: &Rig, o| r.core.order(o).unwrap().state.clone();
    assert_eq!(state(&r, a_es), OrderState::Cancelled);
    assert_eq!(state(&r, b_es), OrderState::Cancelled);
    assert_eq!(state(&r, b_nq), OrderState::Working, "another contract");
    assert_eq!(state(&r, c_es), OrderState::Working, "not in the group");

    r.cmd(Command::GroupCancelAll {
        group: "Evals".into(),
        contract: None,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(state(&r, b_nq), OrderState::Cancelled);
}

/// A member that can't act (an account not known: a saved group can name one a broker venue
/// hasn't listed yet) gets its own refusal; the others still act. An empty group answers with
/// no results.
#[test]
fn one_members_refusal_doesnt_stop_the_others() {
    let mut r = Rig::new();
    r.core
        .restore_groups(vec![group("Evals", &["ghost", "a"]), group("Empty", &[])]);
    r.collect();
    r.market(es(), 5999.75, 6000.25, 6000.00);
    let o = r.place(
        "a",
        es(),
        Side::Buy,
        1,
        OrderKind::Limit { price: px(5990.00) },
    );
    r.wait(LAT);
    r.cmd(Command::GroupCancelAll {
        group: "Evals".into(),
        contract: None,
    })
    .unwrap();
    let (_, results) = r.group_results();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].result.as_ref().unwrap_err().code,
        RejectCode::UnknownAccount
    );
    assert_eq!(results[1].result.as_ref().unwrap(), &vec![o]);
    r.wait(LAT);
    assert_eq!(r.core.order(o).unwrap().state, OrderState::Cancelled);

    assert_eq!(
        r.cmd(Command::GroupFlatten {
            group: "Empty".into(),
            contract: None,
        })
        .unwrap(),
        Vec::<OrderId>::new()
    );
    assert!(r.group_results().1.is_empty());

    // Every member refused: the group command is refused too, never "done".
    r.core
        .restore_groups(vec![group("Ghosts", &["ghost", "phantom"])]);
    r.collect();
    let e = r
        .cmd(Command::GroupFlatten {
            group: "Ghosts".into(),
            contract: None,
        })
        .unwrap_err();
    assert_eq!(e.code, RejectCode::UnknownAccount);
    assert!(e.message.contains("ghost"), "{}", e.message);
    assert_eq!(
        r.group_results().1.len(),
        2,
        "each member's reason is still listed"
    );
}

/// Groups are saved in the trading file and come back as they were, in order.
#[test]
fn groups_survive_a_restart() {
    let mut r = Rig::new();
    create(&mut r, "Evals", &["b", "a"]).unwrap();
    create(&mut r, "Funded", &["c"]).unwrap();
    let file = trading_file(&r.core, &paper()).unwrap();
    assert_eq!(file.version, persist::VERSION);
    let dir = std::env::temp_dir().join(format!("lines-trading-groups-{}", std::process::id()));
    let path = dir.join("trading.json");
    persist::save(&path, &file).unwrap();
    let back = persist::load(&path).unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        back.groups,
        vec![group("Evals", &["b", "a"]), group("Funded", &["c"])]
    );

    let mut core = TradingCore::new(Box::new(PaperBroker::from_state(paper(), back.broker)));
    for a in back.accounts {
        core.add_account(a.config, Some(a.tracker));
    }
    core.restore_groups(back.groups);
    assert_eq!(
        core.groups(),
        [group("Evals", &["b", "a"]), group("Funded", &["c"])]
    );
}
