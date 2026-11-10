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
//!
//! What the run's source can always show is decided once at startup ([`Fallback`]):
//! - A source that serves exactly one contract (a replay) and no `--instrument`: panes it
//!   can't serve *adopt* that contract for this run. Their saved target is untouched (autosave
//!   writes it back as it was) and their header says so ("replay: NQU25 (workspace: ES)").
//! - Otherwise an unavailable pane offers a one-click switch to the source's symbol (the
//!   replay's contract, else its default product): a deliberate switch, saved like any other.

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
    /// For this run only, showing the source's single contract instead of the workspace's
    /// symbol (which `target_doc` keeps).
    pub adopted: Option<Adopted>,
    /// What a one-click switch offers when this pane can't be served.
    pub suggest: Option<Suggestion>,
    /// The source the pane saves (decision 0030): the one it opened on in a run with several,
    /// the workspace's own in a run `--source` limits (so a mock run never rewrites it).
    pub source_doc: Option<String>,
}

/// A pane showing the replay's contract in place of its own symbol, for this run.
#[derive(Clone, Debug, PartialEq)]
pub struct Adopted {
    /// What it shows (e.g. `NQU25`).
    pub shown: String,
    /// What the workspace asked for (e.g. `ES`).
    pub workspace: String,
}

/// The source's own symbol, offered to panes that can't be served.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub target: Target,
    /// `NQU25`, `ES`, `BTC/USD`.
    pub label: String,
}

/// What the run's source can show whatever a pane asks for (decided once at startup).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fallback {
    /// The source serves exactly this contract (a replay): unservable panes adopt it.
    pub single: Option<Suggestion>,
    /// The one-click switch: the single contract, else the source's default product.
    pub suggest: Option<Suggestion>,
}

impl Fallback {
    /// For `base`'s source. `adopt` is false when `--instrument` was given (the trader chose
    /// what every pane shows; nothing is substituted).
    pub fn for_source(base: &FeedConfig, adopt: bool) -> Fallback {
        let single = match &base.source {
            lines_feed::Source::Replay { path, .. } => {
                lines_feed::transport::read_header(path).ok().and_then(|h| {
                    let product = catalog::by_symbol(&h.product)?;
                    Some(match h.contract {
                        Some(k) => Suggestion {
                            label: k.label(&product).as_str().to_owned(),
                            target: Target::Contract(k),
                        },
                        None => Suggestion {
                            label: product.symbol.to_owned(),
                            target: Target::Product(product.id),
                        },
                    })
                })
            }
            _ => None,
        };
        let suggest = single.clone().or_else(|| {
            let mut c = base.clone();
            c.target = None;
            c.record = None;
            lines_feed::new_store(&c).ok().map(|store| {
                let p = store.read().product.clone();
                Suggestion {
                    label: p.symbol.to_owned(),
                    target: Target::Product(p.id),
                }
            })
        });
        Fallback {
            single: single.filter(|_| adopt),
            suggest,
        }
    }
}

/// What a pane will show.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    /// Its own target (`None`: the source's default).
    Use(Option<Target>),
    /// The source's single contract, for this run.
    Adopt(Target),
    /// Nothing: why.
    Unavailable(String),
}

/// The adopt rule (pure): a pane's own target if the source serves it; else the source's
/// single contract if there is one; else unavailable with the reason.
pub fn resolve(
    target: Option<&TargetDoc>,
    check: impl Fn(&Target) -> Result<(), String>,
    single: Option<&Target>,
) -> Resolved {
    let reason = match target.map(TargetDoc::to_target) {
        None => return Resolved::Use(None),
        Some(Ok(t)) => match check(&t) {
            Ok(()) => return Resolved::Use(Some(t)),
            Err(r) => r,
        },
        Some(Err(e)) => e,
    };
    match single {
        Some(s) => Resolved::Adopt(s.clone()),
        None => Resolved::Unavailable(reason),
    }
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
    /// serve adopts the source's single contract (`fallback.single`) or becomes an unavailable
    /// feed with the reason; a subscription that fails is unavailable too.
    pub fn open(
        registry: &FeedRegistry,
        base: &FeedConfig,
        availability: &Availability,
        target: Option<&TargetDoc>,
        fallback: &Fallback,
    ) -> PaneFeed {
        let mut config = base.clone();
        let unavailable = |reason: String, mut config: FeedConfig| {
            config.record = None;
            let symbol = target.map_or_else(|| "default".to_owned(), symbol_of);
            let mut f = PaneFeed::unavailable(
                config,
                availability.clone(),
                target.cloned(),
                symbol,
                reason,
            );
            f.suggest = fallback.suggest.clone();
            f
        };
        let single = fallback.single.as_ref().map(|s| &s.target);
        let (t, adopted) = match resolve(target, |t| availability.check(t), single) {
            Resolved::Unavailable(reason) => return unavailable(reason, config),
            Resolved::Use(t) => (t, None),
            Resolved::Adopt(t) => (
                Some(t),
                Some(Adopted {
                    shown: fallback
                        .single
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_default(),
                    workspace: target.map_or_else(String::new, symbol_of),
                }),
            ),
        };
        config.target = t;
        match registry.subscribe(config.clone()) {
            Ok(sub) => PaneFeed {
                store: sub.store().clone(),
                sub: Some(sub),
                // The saved target stays the workspace's, also when adopting for this run.
                target_doc: target.cloned(),
                config,
                availability: availability.clone(),
                unavailable: None,
                adopted,
                suggest: fallback.suggest.clone(),
                source_doc: None,
            },
            Err(e) => unavailable(format!("{e:#}"), config),
        }
    }

    /// A feed with nothing to show, for `reason` (no source for the pane: decision 0030).
    pub fn unavailable_because(
        base: &FeedConfig,
        availability: &Availability,
        target: Option<&TargetDoc>,
        reason: String,
    ) -> PaneFeed {
        let mut config = base.clone();
        config.record = None;
        let symbol = target.map_or_else(|| "default".to_owned(), symbol_of);
        PaneFeed::unavailable(
            config,
            availability.clone(),
            target.cloned(),
            symbol,
            reason,
        )
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
            adopted: None,
            suggest: None,
            source_doc: None,
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
            adopted: None,
            suggest: self.suggest.clone(),
            source_doc: self.source_doc.clone(),
        })
    }

    /// Another source's product or contract (a pick the pane's own source can't serve, in a
    /// run with several): the pane moves to that source and saves it. The caller picks the
    /// source (`Sources::for_pick`); its own source goes through [`PaneFeed::retarget`].
    pub fn retarget_on(
        &self,
        registry: &FeedRegistry,
        entry: &crate::sources::SourceEntry,
        target: Target,
    ) -> Result<PaneFeed> {
        let mut config = entry.base.clone();
        config.target = Some(target.clone());
        config.record = None;
        let sub = registry.subscribe(config.clone())?;
        Ok(PaneFeed {
            store: sub.store().clone(),
            sub: Some(sub),
            config,
            availability: entry.availability.clone(),
            target_doc: Some(TargetDoc::from_target(&target)),
            unavailable: None,
            adopted: None,
            suggest: entry.fallback.suggest.clone(),
            source_doc: Some(entry.name.clone()),
        })
    }

    /// Switch to the source's own symbol (the one-click action on an unavailable pane goes
    /// through `sources::to_suggested`, which picks the source too).
    #[cfg(test)]
    pub fn to_suggested(&self, registry: &FeedRegistry) -> Result<PaneFeed> {
        let s = self
            .suggest
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this source offers no symbol"))?;
        self.retarget(registry, s.target.clone())
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
        let f = PaneFeed::open(&reg, &base, &avail, Some(&nq), &Fallback::default());
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
        let f = PaneFeed::open(&reg, &base, &avail, Some(&unknown), &Fallback::default());
        assert_eq!(f.unavailable.unwrap().symbol, "XYZ");

        // ES is served; switching the unavailable pane to it works.
        let es = PaneFeed::open(
            &reg,
            &base,
            &avail,
            Some(&TargetDoc::product(catalog::ES)),
            &Fallback::default(),
        );
        assert!(es.unavailable.is_none() && es.sub.is_some());
        let nq_pane = PaneFeed::open(&reg, &base, &avail, Some(&nq), &Fallback::default());
        let switched = nq_pane
            .retarget(&reg, Target::Product(catalog::ES))
            .unwrap();
        assert!(switched.same_feed(&es), "shares the running ES feed");
        assert_eq!(reg.running(), 1);
    }

    fn mock() -> FeedConfig {
        FeedConfig::new(Source::CqgMock(MockConfig {
            quotes_per_sec: 100,
            history_bars: 5,
            ..Default::default()
        }))
    }

    /// ES's front month, as a replay of it would offer.
    fn es_front() -> Suggestion {
        let es = catalog::get(catalog::ES).unwrap();
        let k = lines_feed::front_month(&es, None).unwrap();
        Suggestion {
            label: k.label(&es).as_str().to_owned(),
            target: Target::Contract(k),
        }
    }

    #[test]
    fn the_adopt_rule() {
        let only_nq = |t: &Target| match t {
            Target::Product(p) if *p == catalog::NQ => Ok(()),
            _ => Err("recording is NQ only".to_owned()),
        };
        let nq_u25 = Target::Product(catalog::NQ);
        let es = TargetDoc::product(catalog::ES);
        let nq = TargetDoc::product(catalog::NQ);
        // A single-contract source: what it can't serve adopts its contract.
        assert_eq!(
            resolve(Some(&es), only_nq, Some(&nq_u25)),
            Resolved::Adopt(nq_u25.clone())
        );
        // ...while panes it serves keep their own (a mixed workspace).
        assert_eq!(
            resolve(Some(&nq), only_nq, Some(&nq_u25)),
            Resolved::Use(Some(Target::Product(catalog::NQ)))
        );
        assert_eq!(resolve(None, only_nq, Some(&nq_u25)), Resolved::Use(None));
        // Not a single-contract source (or --instrument given): unavailable, with the reason.
        assert_eq!(
            resolve(Some(&es), only_nq, None),
            Resolved::Unavailable("recording is NQ only".into())
        );
        // A product this build doesn't know adopts too (nothing else could show it).
        let unknown = TargetDoc {
            product: 4242,
            symbol: Some("XYZ".into()),
            contract: None,
        };
        assert_eq!(
            resolve(Some(&unknown), only_nq, Some(&nq_u25)),
            Resolved::Adopt(nq_u25)
        );
    }

    #[test]
    fn adopting_is_for_this_run_only() {
        let reg = FeedRegistry::new();
        let base = mock();
        let avail = availability(&base.source).unwrap();
        let fallback = Fallback {
            single: Some(es_front()),
            suggest: Some(es_front()),
        };
        // An NQ pane on a source that only serves ES's front month: it shows ES, says so,
        // and still saves NQ.
        let nq = TargetDoc::product(catalog::NQ);
        let f = PaneFeed::open(&reg, &base, &avail, Some(&nq), &fallback);
        assert!(f.sub.is_some() && f.unavailable.is_none());
        assert_eq!(
            f.adopted,
            Some(Adopted {
                shown: es_front().label,
                workspace: "NQ".into()
            })
        );
        assert_eq!(f.target_doc.as_ref(), Some(&nq), "the workspace keeps NQ");
        assert_eq!(f.store().read().product.symbol, "ES");
        // A pane the source serves is untouched (mixed workspace).
        let es = PaneFeed::open(
            &reg,
            &base,
            &avail,
            Some(&TargetDoc::product(catalog::ES)),
            &fallback,
        );
        assert_eq!(es.adopted, None);
    }

    #[test]
    fn the_one_click_switch_shows_the_sources_symbol_and_is_saved() {
        let reg = FeedRegistry::new();
        let base = mock();
        let avail = availability(&base.source).unwrap();
        // The mock serves more than one contract: nothing is adopted, ES is offered.
        let fallback = Fallback::for_source(&base, true);
        assert_eq!(fallback.single, None);
        assert_eq!(
            fallback.suggest,
            Some(Suggestion {
                target: Target::Product(catalog::ES),
                label: "ES".into()
            })
        );
        let nq = TargetDoc::product(catalog::NQ);
        let f = PaneFeed::open(&reg, &base, &avail, Some(&nq), &fallback);
        assert!(f.unavailable.is_some());
        assert_eq!(f.suggest.as_ref().map(|s| s.label.as_str()), Some("ES"));
        let switched = f.to_suggested(&reg).unwrap();
        assert!(switched.unavailable.is_none() && switched.sub.is_some());
        // A deliberate switch: saved like the picker's.
        assert_eq!(switched.target_doc, Some(TargetDoc::product(catalog::ES)));
        assert_eq!(switched.suggest, f.suggest, "still offered after switching");
        // No suggestion: nothing to switch to.
        let none = PaneFeed::open(&reg, &base, &avail, Some(&nq), &Fallback::default());
        assert!(none.to_suggested(&reg).is_err());
    }
}
