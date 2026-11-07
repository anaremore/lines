//! The strategy host in the app (decision 0025, phase c): `--strategy NAME` with its settings
//! file runs one instance on the paper core, on the app's feed (the source's default contract,
//! or `--instrument`). It starts Disarmed: only the strategy panel's Start (a person) arms it.
//!
//! The panel reads the host through [`StrategySource`]; the host runs off the UI thread and
//! its snapshot is a short lock. Adding the instance waits for the feed's contract on a
//! helper thread, so the UI never waits for it; a failure shows in the panel.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use lines_feed::FeedConfig;
use lines_feed::registry::{FeedRegistry, Subscription};
use lines_strategy::StrategyExtension;
use lines_strategy::backtest::Settings;
use lines_strategy::host::{Instance, LevelFeed, StrategyHost};
use lines_trading::{StrategyConfig, StrategyId};

use crate::strategy_panel::{StrategyInfo, StrategySource};
use crate::trading::Trading;

/// How long a strategy's feed may be silent before the host pauses it.
const FEED_STALE: Duration = Duration::from_secs(60);
/// How long to wait for the feed to say which contract it carries.
const CONTRACT_WAIT: Duration = Duration::from_secs(120);

impl StrategySource for StrategyHost {
    fn revision(&self) -> u64 {
        StrategyHost::revision(self)
    }

    fn snapshot(&self) -> Vec<StrategyInfo> {
        StrategyHost::snapshot(self)
            .into_iter()
            .map(|v| StrategyInfo {
                id: v.id,
                label: v.label,
                strategy: v.strategy,
                version: v.version,
                account: v.account,
                contract: v.contract,
                limits: v.limits,
                state: v.state,
                reason: v.reason,
                fault: v.fault,
                rolled: v.rolled,
                decisions: v.decisions,
            })
            .collect()
    }
}

/// The host as the app runs it: the host, the feed it listens to (kept open) and why adding
/// the instance failed, if it did.
pub struct AppStrategies {
    host: Arc<StrategyHost>,
    _feed: Subscription,
    problem: Arc<Problem>,
}

/// Why adding the instance failed, with a generation the panel's revision includes, so the
/// panel redraws when it is set (the host's own revision doesn't move on a refused add).
#[derive(Default)]
struct Problem {
    text: Mutex<Option<String>>,
    generation: std::sync::atomic::AtomicU64,
}

impl Problem {
    fn set(&self, why: String) {
        *self.text.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl StrategySource for AppStrategies {
    fn revision(&self) -> u64 {
        let problems = self
            .problem
            .generation
            .load(std::sync::atomic::Ordering::Acquire);
        self.host.revision().wrapping_add(problems << 48)
    }

    fn snapshot(&self) -> Vec<StrategyInfo> {
        StrategySource::snapshot(&*self.host)
    }

    fn problem(&self) -> Option<String> {
        self.problem
            .text
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Start the host on the paper core and add `strategy` (Disarmed) on the default paper
/// account and the feed's contract.
pub fn start(
    strategy: Arc<dyn StrategyExtension>,
    settings: Settings,
    registry: &FeedRegistry,
    config: FeedConfig,
    trading: &Trading,
) -> Result<AppStrategies> {
    let account = trading
        .accounts()
        .into_iter()
        .find(|a| a.paper)
        .map(|a| a.id)
        .ok_or_else(|| anyhow!("--strategy: no paper account"))?;
    let feed = registry.subscribe(config)?;
    let store = feed.store().clone();
    // Listen before anything else, and let the paper broker see the same trades.
    let market = lines_feed::listen::listen(store.feed_id());
    trading.follow(&store);
    let host = Arc::new(StrategyHost::start(
        trading.handle().clone(),
        trading.clock(),
    ));
    let problem = Arc::new(Problem::default());
    let (h, p) = (host.clone(), problem.clone());
    std::thread::Builder::new()
        .name("strategy-add".into())
        .spawn(move || {
            let deadline = Instant::now() + CONTRACT_WAIT;
            let (product, contract) = loop {
                // One read guard at a time: a second read while a writer waits deadlocks.
                let found = {
                    let d = store.read();
                    d.contract.map(|k| (d.product.clone(), k))
                };
                if let Some(found) = found {
                    break found;
                }
                if Instant::now() > deadline {
                    p.set("the feed never said which contract it carries".into());
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let mut levels: Vec<Box<dyn LevelFeed>> = Vec::new();
            for id in strategy.level_sources() {
                let Some(source) = lines_feed::levels::by_id(id) else {
                    p.set(format!(
                        "level source {id:?} is not registered in this build"
                    ));
                    return;
                };
                let ctx = lines_feed::levels::LevelContext {
                    product: product.clone(),
                    contract: Some(contract),
                    settings: lines_feed::levels::runtime::settings_for(id),
                };
                let prices = Arc::new(lines_feed::levels::StorePrices(store.clone()));
                levels.push(Box::new(lines_feed::levels::runtime::acquire(
                    source, ctx, prices,
                )));
            }
            let r = h.add(Instance {
                ext: strategy.clone(),
                params: settings.params.clone(),
                config: StrategyConfig {
                    id: StrategyId(1),
                    label: format!("{} #1", strategy.label()),
                    account,
                    contract,
                    limits: settings.limits.clone(),
                },
                product,
                market,
                levels,
                stale_after: FEED_STALE,
            });
            if let Err(e) = r {
                tracing::error!("strategy {}: {e}", strategy.name());
                p.set(format!("{}: {e}", strategy.label()));
            }
        })?;
    Ok(AppStrategies {
        host,
        _feed: feed,
        problem,
    })
}

#[cfg(test)]
mod tests {
    use lines_core::Product;
    use lines_feed::Source as FeedSource;
    use lines_strategy::{Ctx, Decision, Input, Strategy};
    use lines_trading::Source;
    use lines_trading::strategy::StrategyState;

    use super::*;
    use crate::strategy_panel;
    use crate::trading::TradingSetup;

    /// Never trades: only the controls are under test.
    struct Idle;

    impl Strategy for Idle {
        fn on_input(&mut self, _: &Ctx<'_>, _: Input<'_>, _: &mut Vec<Decision>) {}
    }

    /// `true`: refuses to create the strategy (bad parameters).
    struct IdleExt(bool);

    impl StrategyExtension for IdleExt {
        fn name(&self) -> &str {
            "idle"
        }
        fn label(&self) -> &str {
            "Idle"
        }
        fn version(&self) -> &str {
            "1"
        }
        fn serves(&self, _: &Product) -> bool {
            true
        }
        fn create(&self, _: &serde_json::Value) -> Result<Box<dyn Strategy>, String> {
            if self.0 {
                return Err("bad parameters".into());
            }
            Ok(Box::new(Idle))
        }
    }

    /// Wait (generously: other builds load the machine) until `f` holds.
    fn until(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn settings() -> Settings {
        serde_json::from_value(serde_json::json!({
            "params": {},
            "limits": {"max_position": 1, "max_daily_loss": 500, "max_orders_per_min": 5,
                       "hours": {"tz": "UTC", "windows": [{"start": "00:00", "end": "23:59"}]}}
        }))
        .unwrap()
    }

    /// A strategy the host refuses to add shows why in the panel, and the panel's revision
    /// moves so it redraws (the host's own doesn't on a refused add).
    #[test]
    fn a_refused_strategy_shows_why() {
        let source = FeedSource::CqgMock(lines_feed::cqg::mock::MockConfig::default());
        let config = FeedConfig::new(source.clone());
        let registry = FeedRegistry::new();
        let trading = Trading::start(
            TradingSetup::for_live(None, &source),
            registry.clone(),
            config.clone(),
        );
        let s = start(
            Arc::new(IdleExt(true)),
            settings(),
            &registry,
            config,
            &trading,
        )
        .unwrap();
        until("the refusal", || s.problem().is_some());
        assert!(s.problem().unwrap().contains("bad parameters"));
        // The refusal is part of the revision (its generation, in the top bits).
        assert_eq!(s.revision() >> 48, 1);
        assert!(s.snapshot().is_empty());
    }

    /// The panel's path end to end on the mock source: the host adds the instance Disarmed,
    /// the panel's Start (a manual command to the core) arms it, Stop all stops it, and the
    /// core's state reaches the UI's cache each time.
    #[test]
    fn the_panel_starts_and_stops_a_hosted_strategy() {
        let source = FeedSource::CqgMock(lines_feed::cqg::mock::MockConfig::default());
        let config = FeedConfig::new(source.clone());
        let registry = FeedRegistry::new();
        let trading = Trading::start(
            TradingSetup::for_live(None, &source),
            registry.clone(),
            config.clone(),
        );
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "params": {},
            "limits": {"max_position": 1, "max_daily_loss": 500, "max_orders_per_min": 5,
                       "hours": {"tz": "UTC", "windows": [{"start": "00:00", "end": "23:59"}]}}
        }))
        .unwrap();
        let s = start(
            Arc::new(IdleExt(false)),
            settings,
            &registry,
            config,
            &trading,
        )
        .unwrap();
        until("the instance", || {
            assert_eq!(s.problem(), None);
            s.snapshot().len() == 1
        });
        let info = s.snapshot().remove(0);
        assert_eq!(info.state, StrategyState::Disarmed);
        assert_eq!(info.label, "Idle #1");
        assert!(s.problem().is_none());
        let core_state = |t: &Trading| {
            t.cache
                .try_read()
                .and_then(|c| c.strategy(info.id).map(|(st, _)| st))
        };
        trading.submit(
            Source::Palette,
            Instant::now(),
            strategy_panel::start(info.id),
        );
        until("armed", || {
            core_state(&trading) == Some(StrategyState::Armed)
        });
        strategy_panel::submit_stop_all(&trading, true, Source::Hotkey);
        until("stopped", || {
            core_state(&trading) == Some(StrategyState::Stopped)
        });
        until("the host's view", || {
            s.snapshot()[0].state == StrategyState::Stopped
        });
    }
}
