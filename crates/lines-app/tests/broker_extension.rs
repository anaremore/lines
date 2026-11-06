//! The broker extension point through the app's entry point (decision 0020): official builds
//! register brokers with `Extensions::with_broker`; bad registrations stop the run before
//! anything starts. The dummy broker is made up; nothing names a vendor.

use std::sync::Arc;

use anyhow::Result;
use lines_app::{AccountKind, AccountSource, BrokerExtension, ExtensionAccount, Extensions};
use lines_trading::{AccountId, Broker, BrokerCaps, Money, PaperBroker, PaperConfig};

struct OneAccount;

impl AccountSource for OneAccount {
    fn accounts(&self) -> Vec<ExtensionAccount> {
        vec![ExtensionAccount {
            id: AccountId::new("dummy-sim"),
            label: "Dummy Sim".into(),
            kind: AccountKind::Simulated,
            tradable: Ok(()),
        }]
    }
    fn revision(&self) -> u64 {
        1
    }
}

struct DummyBroker(&'static str);

impl BrokerExtension for DummyBroker {
    fn name(&self) -> &str {
        self.0
    }
    fn label(&self) -> &str {
        "Dummy broker"
    }
    fn capabilities(&self) -> BrokerCaps {
        BrokerCaps {
            market_orders: true,
            stop_orders: true,
            modify: true,
            server_brackets: true,
            oco: true,
            fractional_qty: false,
        }
    }
    fn accounts(&self) -> Arc<dyn AccountSource> {
        Arc::new(OneAccount)
    }
    fn open(&self) -> Result<Box<dyn Broker>> {
        let mut b = PaperBroker::new(PaperConfig::default());
        b.open_account(AccountId::new("dummy-sim"), Money::whole(10_000));
        Ok(Box::new(b))
    }
}

fn run(ext: Extensions) -> Result<()> {
    lines_app::run_with_args(
        ["lines", "--source", "mock", "--headless", "1", "--no-store"],
        ext,
    )
}

#[test]
fn registered_broker_is_kept_and_the_app_runs() {
    let ext = Extensions::new().with_broker(Arc::new(DummyBroker("dummy-broker")));
    assert_eq!(ext.brokers().len(), 1);
    assert_eq!(ext.brokers()[0].name(), "dummy-broker");
    assert!(ext.validate().is_ok());
    let broker = ext.brokers()[0].open().unwrap();
    assert_eq!(broker.snapshot().accounts.len(), 1);
    run(ext).expect("headless run with a broker extension");
}

#[test]
fn bad_broker_registrations_stop_the_run() {
    let reserved = Extensions::new().with_broker(Arc::new(DummyBroker("paper")));
    let e = run(reserved).unwrap_err();
    assert!(e.to_string().contains("built-in broker"), "{e}");

    let dup = Extensions::new()
        .with_broker(Arc::new(DummyBroker("same")))
        .with_broker(Arc::new(DummyBroker("same")));
    assert!(dup.validate().is_err());
    assert!(run(dup).is_err());
}

#[test]
fn the_public_binary_registers_no_brokers() {
    assert!(Extensions::new().brokers().is_empty());
}
