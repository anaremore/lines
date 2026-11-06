//! What a pane is given: a shared feed subscription for one contract, plus what switching it
//! needs. The app-wide [`FeedRegistry`] (lines-feed, no GPUI) is a GPUI global, so every
//! window and pane subscribes through the same registry: two panes on one contract share one
//! connection and one store.

use anyhow::Result;
use gpui::{App, Global};
use lines_feed::FeedConfig;
use lines_feed::availability::{Availability, availability};
use lines_feed::catalog::Target;
use lines_feed::registry::{FeedRegistry, Subscription};

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

/// A pane's feed: the shared subscription, the config it came from (switching keeps the
/// source and changes the target) and what the source can serve (the picker's disabled rows).
#[derive(Clone)]
pub struct PaneFeed {
    pub sub: Subscription,
    pub config: FeedConfig,
    pub availability: Availability,
}

impl PaneFeed {
    pub fn subscribe(registry: &FeedRegistry, config: FeedConfig) -> Result<PaneFeed> {
        let availability = availability(&config.source)?;
        Ok(PaneFeed {
            sub: registry.subscribe(config.clone())?,
            config,
            availability,
        })
    }

    /// The same source, another product or contract.
    pub fn retarget(&self, registry: &FeedRegistry, target: Target) -> Result<PaneFeed> {
        let mut config = self.config.clone();
        config.target = Some(target);
        if config.record.take().is_some() {
            // A recording holds exactly one contract (format v3); don't overwrite it.
            tracing::warn!("recording stopped: switching symbol does not continue the recording");
        }
        Ok(PaneFeed {
            sub: registry.subscribe(config.clone())?,
            config,
            availability: self.availability.clone(),
        })
    }
}
