//! What a pane is given: a shared feed subscription for one contract, plus what switching it
//! needs. The app-wide [`FeedRegistry`] (lines-feed, no GPUI) is a GPUI global, so every
//! window and pane subscribes through the same registry: two panes on one contract share one
//! connection and one store.
//!
//! A pane can also hold an *unavailable* feed: its target is one the source can't serve
//! (e.g. NQ on the ES-only mock) or that this build doesn't know. It has an empty store, no
//! subscription, and the reason, which the pane shows greyed (docs/design.md §7: missing data
//! shows as missing, labelled). The target is kept, so the saved workspace doesn't change and
//! the pane comes back on a source that serves it.

use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Global};
use lines_core::Capabilities;
use lines_feed::FeedConfig;
use lines_feed::availability::Availability;
use lines_feed::catalog::{self, Target};
use lines_feed::registry::{FeedRegistry, Subscription};
use lines_feed::store::{ChartStore, FeedStatus};

use crate::wsdoc::TargetDoc;

/// The app's feed registry.
pub struct Feeds(pub FeedRegistry);

impl Global for Feeds {}

impl Feeds {
    pub fn registry(cx: &App) -> FeedRegistry {
        cx.try_global::<Feeds>()
            .map(|f| f.0.clone())
            .unwrap_or_default()
    }
}

/// A pane's feed: the shared subscription (absent when unavailable), the config it came from
/// (switching keeps the source and changes the target) and what the source can serve (the
/// picker's disabled rows).
#[derive(Clone)]
pub struct PaneFeed {
    pub sub: Option<Subscription>,
    store: Arc<ChartStore>,
    pub config: FeedConfig,
    pub availability: Availability,
    /// The target as saved (kept verbatim for unavailable panes, so nothing is lost).
    pub target_doc: Option<TargetDoc>,
    /// Why there is no data, with the symbol it was for: shown greyed on the pane.
    pub unavailable: Option<Unavailable>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Unavailable {
    /// What the pane was asked to show (e.g. `NQ`, `ESH27`).
    pub symbol: String,
    pub reason: String,
}

impl PaneFeed {
    pub fn store(&self) -> &Arc<ChartStore> {
        &self.store
    }

    /// A pane's feed for `target` on `base`'s source. Never fails: a target the source can't
    /// serve (or a subscription that fails) becomes an unavailable feed with the reason.
    pub fn open(
        registry: &FeedRegistry,
        base: &FeedConfig,
        availability: &Availability,
        target: Option<&TargetDoc>,
    ) -> PaneFeed {
        let mut config = base.clone();
        let resolved = match target.map(TargetDoc::to_target) {
            None => Ok(None),
            Some(Ok(t)) => availability.check(&t).map(|()| Some(t)),
            Some(Err(e)) => Err(e),
        };
        let unavailable = |reason: String, config: FeedConfig| {
            let symbol = target.map_or_else(|| "default".to_owned(), symbol_of);
            PaneFeed::unavailable(
                config,
                availability.clone(),
                target.cloned(),
                symbol,
                reason,
            )
        };
        match resolved {
            Err(reason) => {
                config.record = None;
                unavailable(reason, config)
            }
            Ok(t) => {
                config.target = t;
                match registry.subscribe(config.clone()) {
                    Ok(sub) => PaneFeed {
                        store: sub.store().clone(),
                        sub: Some(sub),
                        target_doc: target.cloned(),
                        config,
                        availability: availability.clone(),
                        unavailable: None,
                    },
                    Err(e) => {
                        config.record = None;
                        unavailable(format!("{e:#}"), config)
                    }
                }
            }
        }
    }

    fn unavailable(
        config: FeedConfig,
        availability: Availability,
        target_doc: Option<TargetDoc>,
        symbol: String,
        reason: String,
    ) -> PaneFeed {
        // An empty store for the product (or a stand-in when the product is unknown): the
        // panes draw nothing from it, they show `unavailable`.
        let product = target_doc
            .as_ref()
            .and_then(|t| catalog::get(lines_core::ProductId(t.product)))
            .or_else(|| catalog::get(catalog::ES))
            .expect("catalog has ES");
        let store = Arc::new(ChartStore::new(
            product,
            None,
            Capabilities::default(),
            availability.provider_id(),
            config.bar_period_ms,
            1,
        ));
        store.set_status(FeedStatus::Failed(reason.clone()));
        PaneFeed {
            sub: None,
            store,
            config,
            availability,
            target_doc,
            unavailable: Some(Unavailable { symbol, reason }),
        }
    }

    /// The same source, another product or contract.
    pub fn retarget(&self, registry: &FeedRegistry, target: Target) -> Result<PaneFeed> {
        let mut config = self.config.clone();
        config.target = Some(target.clone());
        if config.record.take().is_some() {
            // A recording holds exactly one contract (format v3); don't overwrite it.
            tracing::warn!("recording stopped: switching symbol does not continue the recording");
        }
        let sub = registry.subscribe(config.clone())?;
        Ok(PaneFeed {
            store: sub.store().clone(),
            sub: Some(sub),
            config,
            availability: self.availability.clone(),
            target_doc: Some(TargetDoc::from_target(&target)),
            unavailable: None,
        })
    }

    /// Whether two pane feeds show the same running feed.
    #[cfg(test)]
    pub fn same_feed(&self, other: &PaneFeed) -> bool {
        Arc::ptr_eq(&self.store, &other.store)
    }
}

/// `NQ`, `ESH27`, or the saved symbol of an unknown product.
fn symbol_of(t: &TargetDoc) -> String {
    match t.to_target() {
        Ok(Target::Product(id)) => catalog::get(id).map_or_else(String::new, |p| p.symbol.into()),
        Ok(Target::Contract(k)) => {
            catalog::get(k.product).map_or_else(String::new, |p| k.label(&p).as_str().to_owned())
        }
        Err(_) => t
            .symbol
            .clone()
            .unwrap_or_else(|| format!("product {}", t.product)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::Source;
    use lines_feed::availability::availability;
    use lines_feed::cqg::mock::MockConfig;

    #[test]
    fn unservable_targets_become_labelled_unavailable_feeds() {
        let reg = FeedRegistry::new();
        let base = FeedConfig::new(Source::CqgMock(MockConfig {
            quotes_per_sec: 100,
            history_bars: 5,
            ..Default::default()
        }));
        let avail = availability(&base.source).unwrap();
        let nq = TargetDoc::product(catalog::NQ);
        let f = PaneFeed::open(&reg, &base, &avail, Some(&nq));
        assert!(f.sub.is_none());
        assert_eq!(
            f.unavailable,
            Some(Unavailable {
                symbol: "NQ".into(),
                reason: "mock serves ES only".into()
            })
        );
        assert_eq!(f.target_doc.as_ref(), Some(&nq), "kept for saving");
        assert_eq!(f.store().read().product.symbol, "NQ");
        assert_eq!(reg.running(), 0, "nothing started");

        let unknown = TargetDoc {
            product: 4242,
            symbol: Some("XYZ".into()),
            contract: None,
        };
        let f = PaneFeed::open(&reg, &base, &avail, Some(&unknown));
        assert_eq!(f.unavailable.unwrap().symbol, "XYZ");

        // ES is served; switching the unavailable pane to it works.
        let es = PaneFeed::open(&reg, &base, &avail, Some(&TargetDoc::product(catalog::ES)));
        assert!(es.unavailable.is_none() && es.sub.is_some());
        let nq_pane = PaneFeed::open(&reg, &base, &avail, Some(&nq));
        let switched = nq_pane
            .retarget(&reg, Target::Product(catalog::ES))
            .unwrap();
        assert!(switched.same_feed(&es), "shares the running ES feed");
        assert_eq!(reg.running(), 1);
    }
}
