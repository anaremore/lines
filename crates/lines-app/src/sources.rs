//! The data sources this run can show (decision 0030, step 1): several at once.
//!
//! One entry per enabled source, built once at startup: its feed config (its own store),
//! what it can serve ([`Availability`]) and what it offers an unservable pane ([`Fallback`]).
//! Nothing connects here: a source's feed starts when a pane first subscribes to one of its
//! contracts (the registry, refcounted) and stops when none does, so a source no pane shows
//! costs nothing, and a source failing or going stale reaches only its own panes.
//!
//! `--source X` limits the run to X (tests, screenshots, headless checks: as before this
//! table). Without it the run has every enabled source: Coinbase and Kraken (on unless turned
//! off in settings) and every registered extension (decision 0020). The mock and replays stay
//! behind their flags.

use lines_feed::FeedConfig;
use lines_feed::availability::Availability;
use lines_feed::catalog::Target;

use crate::feeds::Fallback;
use crate::wsdoc::TargetDoc;

/// One source the run can show.
#[derive(Clone)]
pub struct SourceEntry {
    /// Its name: `coinbase`, `kraken`, an extension's name, `mock`, `replay` (what a pane saves).
    pub name: String,
    /// Its feed config, without a target (a pane sets it): its own store and options.
    pub base: FeedConfig,
    pub availability: Availability,
    pub fallback: Fallback,
}

/// The run's sources, in preference order (extensions first, then Coinbase, then Kraken).
#[derive(Clone)]
pub struct Sources {
    entries: Vec<SourceEntry>,
    /// `--source` limited the run to one source: every pane uses it (as before).
    limited: bool,
}

/// Why a pane can't open on any source (its target and the reason, for the pane).
#[derive(Clone, Debug, PartialEq)]
pub struct NoSource(pub String);

impl Sources {
    /// The run's sources. `entries` is never empty (startup checks).
    pub fn new(entries: Vec<SourceEntry>, limited: bool) -> Sources {
        assert!(!entries.is_empty(), "a run has at least one source");
        Sources { entries, limited }
    }

    /// The first source: the run's when limited, else the preferred one. Single-source uses
    /// (screenshots, the benchmark, the paper core's feed bridge) take it.
    pub fn primary(&self) -> &SourceEntry {
        &self.entries[0]
    }

    pub fn all(&self) -> &[SourceEntry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&SourceEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Whether `--source` limited the run to one source.
    pub fn limited(&self) -> bool {
        self.limited
    }

    /// The sources that can serve `target`, in preference order.
    pub fn serving<'a>(&'a self, target: &'a Target) -> impl Iterator<Item = &'a SourceEntry> {
        self.entries
            .iter()
            .filter(move |e| e.availability.check(target).is_ok())
    }

    /// The source a pane opens on: the one it saved (if the run has it); else, limited, the
    /// run's; else the first that serves its target; a pane with no target takes the primary.
    /// `Err`: a saved source the run doesn't have, or no source serving the target, with why.
    pub fn for_pane(
        &self,
        saved: Option<&str>,
        target: Option<&TargetDoc>,
    ) -> Result<&SourceEntry, NoSource> {
        if self.limited {
            return Ok(self.primary());
        }
        if let Some(name) = saved {
            return self
                .get(name)
                .ok_or_else(|| NoSource(format!("{name} is off or not in this build")));
        }
        let Some(t) = target.map(TargetDoc::to_target) else {
            return Ok(self.primary());
        };
        let t = t.map_err(NoSource)?;
        let found = self
            .entries
            .iter()
            .find(|e| e.availability.check(&t).is_ok());
        found.ok_or_else(|| {
            NoSource(format!(
                "no enabled source serves {}",
                lines_feed::catalog::get(match &t {
                    Target::Product(p) => *p,
                    Target::Contract(k) => k.product,
                })
                .map_or("it", |p| p.symbol)
            ))
        })
    }

    /// The source a pick goes to: the pane's own if it serves the target, else the first that
    /// does (`None`: none).
    pub fn for_pick<'a>(&'a self, current: &str, target: &'a Target) -> Option<&'a SourceEntry> {
        self.get(current)
            .filter(|e| e.availability.check(target).is_ok())
            .or_else(|| self.serving(target).next())
    }
}

/// Settings for the public sources (an optional settings field: absent is on).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourcePrefs {
    /// Off: the source isn't part of a run without `--source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl SourcePrefs {
    pub fn on(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// What a pane's symbol picker may pick (decision 0030): its own source's symbols, and, in a
/// run with several, every other enabled source's (the pick moves the pane there). Owned
/// (availabilities are cheap `Arc` clones), built when the picker asks.
#[derive(Clone)]
pub struct Pick {
    own: Availability,
    /// The other sources, with their names (empty when `--source` limits the run).
    others: Vec<(String, Availability)>,
    /// The pane's own source's name (for the tag), when the run has several.
    own_name: Option<String>,
}

impl Pick {
    /// Only the pane's own source (a run `--source` limits, and tests).
    pub fn one(own: &Availability) -> Pick {
        Pick {
            own: own.clone(),
            others: Vec::new(),
            own_name: None,
        }
    }

    /// `Ok` if some source can show `target`; else the pane's own source's reason.
    pub fn check(&self, target: &Target) -> Result<(), String> {
        match self.own.check(target) {
            Ok(()) => Ok(()),
            Err(why) => {
                if self.others.iter().any(|(_, a)| a.check(target).is_ok()) {
                    Ok(())
                } else {
                    Err(why)
                }
            }
        }
    }

    /// The source a row's tag names, in a run with several: the pane's own when it serves the
    /// target, else the first that does (`None`: one source, or none serves it).
    pub fn tag(&self, target: &Target) -> Option<String> {
        let own = self.own_name.as_ref()?;
        if self.own.check(target).is_ok() {
            return Some(source_label(own));
        }
        self.others
            .iter()
            .find(|(_, a)| a.check(target).is_ok())
            .map(|(n, _)| source_label(n))
    }
}

/// How a source is named on screen: "Coinbase", "Kraken", an extension's name capitalised.
pub fn source_label(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The pane's own source's name in this run (its saved one, else the primary's).
fn current<'a>(sources: &'a Sources, feed: &'a crate::feeds::PaneFeed) -> &'a str {
    feed.source_doc
        .as_deref()
        .filter(|n| sources.get(n).is_some() && !sources.limited())
        .unwrap_or(&sources.primary().name)
}

/// The picker's sources for a pane.
pub fn pick(feed: &crate::feeds::PaneFeed, cx: &gpui::App) -> Pick {
    let Some(env) = crate::workspace::try_env(cx) else {
        return Pick::one(&feed.availability);
    };
    let sources = &env.sources;
    if sources.limited() || sources.all().len() < 2 {
        return Pick::one(&feed.availability);
    }
    let own = current(sources, feed).to_owned();
    Pick {
        own: feed.availability.clone(),
        others: sources
            .all()
            .iter()
            .filter(|e| e.name != own)
            .map(|e| (e.name.clone(), e.availability.clone()))
            .collect(),
        own_name: Some(own),
    }
}

/// Switch a pane to `target`: on its own source if that serves it, else on the first source
/// that does (decision 0030; the pane then shows and saves that source).
pub fn retarget(
    feed: &crate::feeds::PaneFeed,
    target: Target,
    cx: &gpui::App,
) -> anyhow::Result<crate::feeds::PaneFeed> {
    let registry = crate::feeds::Feeds::registry(cx);
    let Some(env) = crate::workspace::try_env(cx) else {
        return feed.retarget(&registry, target);
    };
    let sources = &env.sources;
    if sources.limited() || feed.availability.check(&target).is_ok() {
        return feed.retarget(&registry, target);
    }
    let own = current(sources, feed);
    let wanted = target.clone();
    match sources.for_pick(own, &wanted) {
        Some(entry) if entry.name != own => feed.retarget_on(&registry, entry, target),
        _ => feed.retarget(&registry, target),
    }
}
