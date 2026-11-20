//! A synthetic broker extension for screenshots (`--demo-broker`), vendor-neutral and made up:
//! a SIM and a LIVE account, both read-only (observe mode), the SIM one long 2 with a working
//! sell limit on the recording's contract. With `--demo-accounts N`: N more read-only live
//! accounts, and the SIM account tradable, to show the account list's sections. Its broker
//! accepts nothing either way: a request that reached it would only be logged. With
//! `--demo-fills` it reports a few made-up executions placed outside Lines (no fees, like a
//! venue that reports none), for the journal: round trips on its LIVE account over three trade
//! dates, and one on the first extra account.

use std::sync::Arc;

use lines_core::{ContractKey, Price, Qty, UtcMillis};
use lines_trading::broker::{BrokerAccount, BrokerPosition};
use lines_trading::extension::{
    AccountKind, AccountSource, BrokerExtension, ExtensionAccount, Field, FieldKind, VenueStatus,
};
use lines_trading::order::Role;
use lines_trading::position::{Lot, Position};
use lines_trading::{
    AccountId, Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot, MarketInput, Money,
    Order, OrderId, OrderKind, OrderState, Side,
};

pub const SIM: &str = "demo-sim";
pub const LIVE: &str = "demo-live";

fn caps() -> BrokerCaps {
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

/// `extra`: more read-only live accounts (`--demo-accounts`), with the Sim account tradable
/// (the account list's sections and scrolling).
struct Accounts {
    extra: usize,
    /// The venue's warning (`--venue-warning`).
    warning: Option<String>,
}

/// The id of the `n`th extra account (from 1).
fn extra_id(n: usize) -> String {
    format!("demo-live-{n:02}")
}

impl AccountSource for Accounts {
    fn accounts(&self) -> Vec<ExtensionAccount> {
        let mut out = vec![
            ExtensionAccount {
                id: AccountId::new(SIM),
                label: "Demo Sim".into(),
                kind: AccountKind::Simulated,
                tradable: if self.extra > 0 {
                    Ok(())
                } else {
                    Err("Sim order entry off".into())
                },
            },
            ExtensionAccount {
                id: AccountId::new(LIVE),
                label: LIVE_NAME.into(),
                kind: AccountKind::Live,
                tradable: Err("live trading off".into()),
            },
        ];
        out.extend((1..=self.extra).map(|n| ExtensionAccount {
            id: AccountId::new(&extra_id(n)),
            label: format!("Demo ******{n:02}"),
            kind: AccountKind::Live,
            tradable: Err("live trading off".into()),
        }));
        out
    }
    fn revision(&self) -> u64 {
        1
    }
    fn status(&self) -> Option<VenueStatus> {
        // No message time: a screenshot never turns "stale" however long its replay ran.
        Some(VenueStatus {
            connected: true,
            detail: "connected".into(),
            last_message: None,
            warning: self.warning.clone(),
        })
    }
}

struct DemoBroker {
    snapshot: BrokerSnapshot,
    /// `--demo-fills`' executions, until the first poll.
    fills: Vec<BrokerEvent>,
}

impl Broker for DemoBroker {
    fn name(&self) -> &str {
        "demo"
    }
    fn capabilities(&self) -> BrokerCaps {
        caps()
    }
    fn request(&mut self, _now: UtcMillis, _req: BrokerRequest) {
        tracing::error!("demo broker: a request reached a read-only venue");
    }
    fn market(&mut self, _now: UtcMillis, _c: ContractKey, _i: &MarketInput) {}
    fn poll(&mut self, _now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        out.append(&mut self.fills);
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        None
    }
    fn snapshot(&self) -> BrokerSnapshot {
        self.snapshot.clone()
    }
}

struct Demo {
    extra: usize,
    warning: Option<String>,
    contract: ContractKey,
    last: Price,
    tick: i64,
    at: UtcMillis,
    fills: bool,
}

impl Demo {
    /// `--demo-fills`: executions made outside Lines, reported once at the first poll. Nets on
    /// LIVE (ES: $12.50 a tick): +$300 two trade dates back, +$100 the day before, +$50 just
    /// before `at` (inside the recording); on the first extra account, −$50 half an hour back.
    fn executions(&self) -> Vec<BrokerEvent> {
        if !self.fills {
            return Vec::new();
        }
        const DAY: i64 = 86_400_000;
        let t = self.tick;
        let mut n = 0;
        let mut fill = |account: &str, side, ticks: i64, qty, ago_ms: i64| {
            n += 1;
            BrokerEvent::ExternalFill {
                account: AccountId::new(account),
                contract: self.contract,
                side,
                price: Price(self.last.0 + ticks * t),
                qty: Qty(qty),
                fee: Money::ZERO,
                at: UtcMillis(self.at.0 - ago_ms),
                trade_id: format!("demo-x{n}"),
            }
        };
        let mut v = vec![
            fill(LIVE, Side::Buy, 0, 2, 2 * DAY + 600_000),
            fill(LIVE, Side::Sell, 12, 2, 2 * DAY),
            fill(LIVE, Side::Buy, -4, 1, DAY + 900_000),
            fill(LIVE, Side::Sell, 4, 1, DAY),
            fill(LIVE, Side::Sell, 2, 1, 90_000),
            fill(LIVE, Side::Buy, -2, 1, 30_000),
        ];
        if self.extra > 0 {
            let first = extra_id(1);
            v.push(fill(&first, Side::Buy, 0, 1, 1_800_000));
            v.push(fill(&first, Side::Sell, -4, 1, 1_500_000));
        }
        v
    }
}

impl BrokerExtension for Demo {
    fn name(&self) -> &str {
        "demo"
    }
    fn label(&self) -> &str {
        "Demo broker"
    }
    fn short_label(&self) -> &str {
        "Demo"
    }
    fn capabilities(&self) -> BrokerCaps {
        caps()
    }
    fn accounts(&self) -> Arc<dyn AccountSource> {
        Arc::new(Accounts {
            extra: self.extra,
            warning: self.warning.clone(),
        })
    }
    /// A local venue's settings, as an observe-only broker describes them (Settings ›
    /// Connections evidence).
    fn fields(&self) -> Vec<Field> {
        let text = |key, label| Field {
            key,
            label,
            kind: FieldKind::Text,
            required: false,
            env: None,
            keychain: None,
        };
        vec![
            text("host", "Host"),
            text("port", "Port"),
            text("username", "User name"),
            Field {
                kind: FieldKind::Secret,
                ..text("password", "Password")
            },
        ]
    }
    fn plan(&self) -> Option<String> {
        Some(if self.extra > 0 {
            "Sim orders on simulated accounts; live trading off".to_owned()
        } else {
            "Observe only: order entry disabled".to_owned()
        })
    }
    fn open(&self) -> anyhow::Result<Box<dyn Broker>> {
        let mut long2 = Position::default();
        long2.lots.push_back(Lot {
            price: self.last.0 - 2 * self.tick,
            qty: 2,
        });
        let order = Order {
            id: OrderId(9_000_001),
            account: AccountId::new(SIM),
            contract: self.contract,
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(self.last.0 + 4 * self.tick),
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
            created: self.at,
            updated: self.at,
        };
        Ok(Box::new(DemoBroker {
            fills: self.executions(),
            snapshot: BrokerSnapshot {
                accounts: vec![
                    BrokerAccount {
                        id: AccountId::new(SIM),
                        balance: Money::whole(50_000),
                        positions: vec![BrokerPosition {
                            contract: self.contract,
                            position: long2,
                        }],
                        orders: vec![order],
                    },
                    BrokerAccount {
                        id: AccountId::new(LIVE),
                        balance: Money::whole(25_000),
                        positions: Vec::new(),
                        orders: Vec::new(),
                    },
                ]
                .into_iter()
                .chain((1..=self.extra).map(|n| BrokerAccount {
                    id: AccountId::new(&extra_id(n)),
                    balance: Money::whole(25_000),
                    positions: Vec::new(),
                    orders: Vec::new(),
                }))
                .collect(),
            },
        }))
    }
}

/// The demo venue on `contract`, its position and order placed around `last`; `extra` more
/// read-only live accounts (`--demo-accounts`).
pub fn extension(
    contract: ContractKey,
    last: Price,
    tick: i64,
    at: UtcMillis,
    extra: usize,
    fills: bool,
    warning: Option<String>,
) -> Arc<dyn BrokerExtension> {
    Arc::new(Demo {
        extra,
        warning,
        contract,
        last,
        tick,
        at,
        fills,
    })
}

/// The demo LIVE account's name (Settings › Accounts keys prop rules by it).
pub const LIVE_NAME: &str = "Demo live";
