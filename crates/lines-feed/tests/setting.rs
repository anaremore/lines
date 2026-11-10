//! A source's settings (decision 0030 §3–4): from the store the app installs, unless the
//! field's environment variable is set. Its own test binary: it sets an environment variable,
//! which no other test in a process may read at the same time.

use std::sync::Arc;

use anyhow::{Result, bail};
use lines_core::{ContractKey, Product, ProductId};
use lines_feed::extension::{
    API_VERSION, BoxFuture, DynTransport, Field, FieldKind, SettingsStore, SourceExtension,
    register, set_settings_store, setting,
};
use lines_feed::provider::Provider;

struct Keyed;

impl SourceExtension for Keyed {
    fn api_version(&self) -> u32 {
        API_VERSION
    }
    fn name(&self) -> &'static str {
        "keyed-test"
    }
    fn help(&self) -> &'static str {
        ""
    }
    fn provider(&self) -> Arc<dyn Provider> {
        Arc::new(lines_feed::coinbase::CoinbaseProvider)
    }
    fn default_product(&self) -> ProductId {
        lines_feed::catalog::BTC_USD
    }
    fn connect(
        &self,
        _: &Product,
        _: Option<ContractKey>,
    ) -> BoxFuture<'static, Result<Box<dyn DynTransport>>> {
        Box::pin(async { bail!("never connects") })
    }
    fn fields(&self) -> Vec<Field> {
        vec![Field {
            key: "api-key",
            label: "API key",
            kind: FieldKind::Secret,
            required: true,
            env: Some("LINES_KEYED_TEST_KEY"),
            keychain: None,
        }]
    }
}

struct Held;

impl SettingsStore for Held {
    fn get(&self, source: &str, field: &Field) -> Option<String> {
        (source == "keyed-test" && field.key == "api-key").then(|| "stored-1234".to_owned())
    }
}

#[test]
fn a_sources_setting_comes_from_the_store_unless_its_variable_is_set() {
    register(Arc::new(Keyed)).unwrap();
    set_settings_store(Arc::new(Held));
    assert_eq!(
        setting("keyed-test", "api-key").as_deref(),
        Some("stored-1234")
    );
    assert_eq!(setting("keyed-test", "no-such-field"), None);
    assert_eq!(setting("no-such-source", "api-key"), None);
    // SAFETY: the only test in this binary, so nothing reads the environment meanwhile.
    unsafe { std::env::set_var("LINES_KEYED_TEST_KEY", "from-env-9999") };
    assert_eq!(
        setting("keyed-test", "api-key").as_deref(),
        Some("from-env-9999")
    );
    unsafe { std::env::remove_var("LINES_KEYED_TEST_KEY") };
}
