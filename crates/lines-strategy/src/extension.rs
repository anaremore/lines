//! The strategy extension point (decisions 0020, 0025): strategies registered from outside
//! the public crates. Official builds pass theirs to `lines_app::Extensions::with_strategy`;
//! the public build ships only [`crate::example`]. Nothing here names a vendor.

use std::sync::Arc;

use anyhow::{Result, bail};
use lines_core::Product;
use lines_feed::levels::SettingSpec;

use crate::Strategy;

/// A strategy kind: its identity, what it needs, and a factory for instances.
pub trait StrategyExtension: Send + Sync {
    /// Stable name (lowercase letters, digits, `-`), saved with instances and reports.
    fn name(&self) -> &str;
    /// Shown in the strategy panel and the backtest list.
    fn label(&self) -> &str;
    /// Its own version: reports carry it, and Live automation (decision 0025) is dropped
    /// when it changes.
    fn version(&self) -> &str;
    /// Whether it can trade this product.
    fn serves(&self, product: &Product) -> bool;
    /// Level source ids it needs (decision 0022). Missing or stale maps pause it.
    fn level_sources(&self) -> &[&'static str] {
        &[]
    }
    /// Its parameters (decision 0021: settings are data).
    fn settings(&self) -> &'static [SettingSpec] {
        &[]
    }
    /// A new instance. Bad parameters refuse it with a reason; they are never defaulted
    /// silently.
    fn create(&self, params: &serde_json::Value) -> Result<Box<dyn Strategy>, String>;
}

/// Check a set of strategies: well-formed, unique names.
pub fn validate(strategies: &[Arc<dyn StrategyExtension>]) -> Result<()> {
    for (i, s) in strategies.iter().enumerate() {
        let name = s.name();
        let ok = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if !ok {
            bail!("strategy name {name:?}: use 1-32 lowercase letters, digits and '-'");
        }
        if strategies[..i].iter().any(|o| o.name() == name) {
            bail!("two strategies are named {name:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    impl StrategyExtension for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn label(&self) -> &str {
            "x"
        }
        fn version(&self) -> &str {
            "1"
        }
        fn serves(&self, _: &Product) -> bool {
            true
        }
        fn create(&self, _: &serde_json::Value) -> Result<Box<dyn Strategy>, String> {
            Err("test".into())
        }
    }

    #[test]
    fn names_are_checked() {
        let ok: Vec<Arc<dyn StrategyExtension>> =
            vec![Arc::new(Named("a-1")), Arc::new(Named("b"))];
        assert!(validate(&ok).is_ok());
        for bad in ["", "Caps", "has space", "a_b"] {
            assert!(validate(&[Arc::new(Named(bad)) as Arc<dyn StrategyExtension>]).is_err());
        }
        let dup: Vec<Arc<dyn StrategyExtension>> = vec![Arc::new(Named("a")), Arc::new(Named("a"))];
        assert!(validate(&dup).is_err());
    }
}
