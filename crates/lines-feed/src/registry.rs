//! Shared feed subscriptions: one feed (thread, connection, store) per source and contract,
//! however many panes show it.
//!
//! Panes call [`FeedRegistry::subscribe`] with a [`FeedConfig`] and hold the returned
//! [`Subscription`]. Subscriptions that resolve to the same [`SubKey`] (source, product,
//! contract, front-month following) share one running feed and one [`ChartStore`]; the feed
//! stops when the last subscription is dropped. Stopping joins the feed thread, so it happens
//! on a short-lived background thread, never on the caller's (the UI's) thread (decision
//! 0001). Nothing here depends on GPUI.
//!
//! Each store has its own atomic revision, so a pane redraws only when *its* market changed.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use anyhow::Result;
use lines_core::{ContractKey, ProductId};
use parking_lot::Mutex;

use crate::store::ChartStore;
use crate::{FeedConfig, FeedHandle, Source};

/// What makes two subscriptions the same feed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubKey {
    /// The source and its connection parameters (e.g. `coinbase wss://…`, `replay <path> …`).
    pub source: String,
    pub product: ProductId,
    /// `None` only for replays of recordings that don't name their contract.
    pub contract: Option<ContractKey>,
    /// Follows the product's front month (rolls), rather than a fixed contract.
    pub follow_front: bool,
}

impl SubKey {
    pub fn for_config(config: &FeedConfig) -> Result<SubKey> {
        let (product, contract, follow_front) = crate::resolve(config)?;
        Ok(SubKey {
            source: source_key(&config.source),
            product,
            contract,
            follow_front,
        })
    }
}

fn source_key(source: &Source) -> String {
    match source {
        // One mock exchange per subscription key: its parameters are part of the identity.
        Source::CqgMock(m) => format!(
            "mock rate={} batch={} disconnect={:?} seed={}",
            m.quotes_per_sec, m.quotes_per_frame, m.disconnect_after, m.seed
        ),
        #[cfg(feature = "vendor-cqg")]
        Source::Cqg { url, .. } => format!("cqg {url}"),
        Source::Coinbase { url } => format!("coinbase {url}"),
        Source::Kraken {
            spot_url,
            futures_url,
        } => format!("kraken {spot_url} {futures_url}"),
        Source::Extension(e) => format!("extension {}", e.name()),
        Source::Replay {
            path,
            speed,
            stop_at,
        } => format!("replay {} speed={speed} stop={stop_at:?}", path.display()),
    }
}

#[derive(Default)]
struct Inner {
    feeds: HashMap<SubKey, Weak<Shared>>,
}

/// One running feed and everyone watching it.
struct Shared {
    key: SubKey,
    store: Arc<ChartStore>,
    handle: Option<FeedHandle>,
    registry: Weak<Mutex<Inner>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(reg) = self.registry.upgrade() {
            let mut inner = reg.lock();
            // Only remove our own (now dead) entry: a new subscriber may already have started
            // a fresh feed under the same key.
            if inner
                .feeds
                .get(&self.key)
                .is_some_and(|w| w.strong_count() == 0)
            {
                inner.feeds.remove(&self.key);
            }
        }
        if let Some(handle) = self.handle.take() {
            // Dropping the handle joins the feed thread: do it off the caller's thread.
            let spawned = std::thread::Builder::new()
                .name("lines-feed-stop".into())
                .spawn(move || drop(handle));
            if let Err(e) = spawned {
                tracing::warn!("couldn't stop the feed in the background: {e}");
            }
        }
    }
}

/// A pane's handle on a running feed. Cloning shares it; the feed stops when the last clone
/// (across all panes) is dropped.
#[derive(Clone)]
pub struct Subscription(Arc<Shared>);

impl Subscription {
    pub fn store(&self) -> &Arc<ChartStore> {
        &self.0.store
    }

    pub fn key(&self) -> &SubKey {
        &self.0.key
    }

    /// Whether two subscriptions share one feed.
    pub fn same_feed(&self, other: &Subscription) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Subscription").field(&self.0.key).finish()
    }
}

/// The app-wide set of running feeds.
#[derive(Clone, Default)]
pub struct FeedRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl FeedRegistry {
    pub fn new() -> FeedRegistry {
        FeedRegistry::default()
    }

    /// Join the running feed for this config's source and contract, or start it. A config's
    /// `record` applies only when this call starts the feed.
    pub fn subscribe(&self, config: FeedConfig) -> Result<Subscription> {
        let key = SubKey::for_config(&config)?;
        let mut inner = self.inner.lock();
        if let Some(shared) = inner.feeds.get(&key).and_then(Weak::upgrade) {
            if config.record.is_some() {
                tracing::warn!("{key:?} is already running; not recording a second copy");
            }
            return Ok(Subscription(shared));
        }
        let handle = crate::spawn(config)?;
        let shared = Arc::new(Shared {
            key: key.clone(),
            store: handle.store.clone(),
            handle: Some(handle),
            registry: Arc::downgrade(&self.inner),
        });
        inner.feeds.insert(key, Arc::downgrade(&shared));
        Ok(Subscription(shared))
    }

    /// The stores of the feeds running now (recording starts and stops on them).
    pub fn stores(&self) -> Vec<Arc<crate::store::ChartStore>> {
        self.inner
            .lock()
            .feeds
            .values()
            .filter_map(Weak::upgrade)
            .map(|s| s.store.clone())
            .collect()
    }

    /// Feeds currently running.
    pub fn running(&self) -> usize {
        self.inner
            .lock()
            .feeds
            .values()
            .filter(|w| w.strong_count() > 0)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{self, Target};
    use crate::cqg::mock::MockConfig;
    use std::time::{Duration, Instant};

    fn mock(target: Option<Target>) -> FeedConfig {
        let mut c = FeedConfig::new(Source::CqgMock(MockConfig {
            quotes_per_sec: 200,
            history_bars: 10,
            ..Default::default()
        }));
        c.target = target;
        c
    }

    fn wait_until(mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn same_contract_shares_one_feed_and_stops_with_the_last_pane() {
        let reg = FeedRegistry::new();
        let es = catalog::get(catalog::ES).unwrap();
        let front = crate::front_month(&es, None).unwrap();
        let a = reg.subscribe(mock(None)).unwrap();
        // The product (default ES, front month) and its front contract resolve differently
        // (one follows rolls), so they are distinct feeds...
        let c = reg.subscribe(mock(Some(Target::Contract(front)))).unwrap();
        // ...while the same product again shares.
        let b = reg
            .subscribe(mock(Some(Target::Product(catalog::ES))))
            .unwrap();
        assert!(a.same_feed(&b));
        assert!(Arc::ptr_eq(a.store(), b.store()), "one store");
        assert!(!a.same_feed(&c));
        assert_eq!(reg.running(), 2);
        wait_until(|| a.store().read().stats.trades_in > 0);

        drop(a);
        assert_eq!(reg.running(), 2, "b still watches it");
        let store = b.store().clone();
        drop(b);
        assert_eq!(reg.running(), 1);
        // The feed thread stops in the background; the store stops changing.
        wait_until(|| {
            let r = store.revision();
            std::thread::sleep(Duration::from_millis(100));
            store.revision() == r
        });
        drop(c);
        assert_eq!(reg.running(), 0);
        // Subscribing again starts a fresh feed.
        let d = reg.subscribe(mock(None)).unwrap();
        assert!(!Arc::ptr_eq(d.store(), &store));
    }

    /// "Start recording" on a running feed: it reconnects once with a recorder, so the file
    /// (named for the source and contract, the source in its header: decisions 0010, 0030)
    /// holds a whole session and replays; Stop finishes it while the feed goes on. A replay is
    /// never recorded again.
    #[test]
    fn a_running_feed_starts_and_stops_recording() {
        use crate::live_record::RecState;
        let dir = std::env::temp_dir().join(format!("lines-live-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = FeedRegistry::new();
        let a = reg.subscribe(mock(None)).unwrap();
        let store = a.store().clone();
        wait_until(|| store.read().stats.trades_in > 0);
        assert!(store.recording.recordable());
        assert!(store.recording.start(dir.clone()));
        assert!(!store.recording.start(dir.clone()), "once");
        wait_until(|| matches!(store.recording.state(), RecState::On { .. }));
        let RecState::On { path, .. } = store.recording.state() else {
            unreachable!()
        };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let es = catalog::get(catalog::ES).unwrap();
        let front = crate::front_month(&es, None).unwrap();
        assert!(
            name.starts_with(&format!("cqg-{}-", front.label(&es))) && name.ends_with("Z.lfr"),
            "{name}"
        );
        let n = store.read().stats.trades_in;
        wait_until(|| store.read().stats.trades_in > n + 20);
        assert_eq!(
            store.recording.stop(),
            crate::live_record::Stopped::Saved(path.clone())
        );
        assert_eq!(store.recording.state(), RecState::Off);
        let n = store.read().stats.trades_in;
        wait_until(|| store.read().stats.trades_in > n);

        // The file: the source and contract in its header, a whole session that replays.
        let h = crate::transport::read_header(&path).unwrap();
        assert_eq!((h.provider.as_str(), h.contract), ("cqg", Some(front)));
        let replay = reg
            .subscribe(FeedConfig::new(Source::Replay {
                path: path.clone(),
                speed: 0.0,
                stop_at: None,
            }))
            .unwrap();
        let rs = replay.store().clone();
        wait_until(|| rs.read().status == crate::store::FeedStatus::ReplayFinished);
        assert!(
            rs.read().stats.trades_in > 20,
            "{}",
            rs.read().stats.trades_in
        );
        assert!(!rs.recording.recordable());
        assert!(
            !rs.recording.start(dir.clone()),
            "a replay is never recorded again"
        );
        drop((a, replay));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinct_sources_never_share() {
        let a = SubKey::for_config(&mock(None)).unwrap();
        let mut other = mock(None);
        if let Source::CqgMock(m) = &mut other.source {
            m.quotes_per_sec = 999;
        }
        assert_ne!(a, SubKey::for_config(&other).unwrap());
        assert_eq!(a, SubKey::for_config(&mock(None)).unwrap());
    }
}
