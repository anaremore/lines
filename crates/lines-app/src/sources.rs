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

impl std::fmt::Debug for SourceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceEntry")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
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

    /// The source a pane opens on: the one it saved (if the run has it); else, limited, the
    /// run's; else the first that serves its target; a pane with no target takes the primary.
    /// `Err`: a saved source the run doesn't have, or no source serving the target, with why.
    pub fn for_pane(
        &self,
        saved: Option<&str>,
        target: Option<&TargetDoc>,
    ) -> Result<&SourceEntry, NoSource> {
        // A pane that shows no market (journal, positions, strategies) needs no source.
        if self.limited || target.is_none() {
            return Ok(self.primary());
        }
        if let Some(name) = saved {
            return self.get(name).ok_or_else(|| {
                NoSource(format!(
                    "{} is off or not in this build",
                    source_label(name)
                ))
            });
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
            // Says what's missing and the way out (decision 0030): the source, or the symbol.
            NoSource(format!(
                "No source has {}: pick another symbol",
                lines_feed::catalog::get(match &t {
                    Target::Product(p) => *p,
                    Target::Contract(k) => k.product,
                })
                .map_or_else(
                    || "it".to_owned(),
                    |p| match &t {
                        Target::Contract(k) => k.label(&p).as_str().to_owned(),
                        Target::Product(_) => p.symbol.to_owned(),
                    }
                )
            ))
        })
    }

    /// The source a pick goes to: the pane's own if it serves the target, else the first that
    /// does (`None`: none).
    pub fn for_pick(&self, current: &str, target: &Target) -> Option<&SourceEntry> {
        self.get(current)
            .filter(|e| e.availability.check(target).is_ok())
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.availability.check(target).is_ok())
            })
    }

    /// The pane's own source in this run: its saved one if the run has it, else the primary.
    pub fn own<'a>(&'a self, saved: Option<&'a str>) -> &'a str {
        saved
            .filter(|n| !self.limited && self.get(n).is_some())
            .unwrap_or(&self.primary().name)
    }

    /// Where a pane's switch to `target` goes (decision 0030), given the source it saves.
    pub fn plan_move(&self, saved: Option<&str>, target: &Target) -> Move<'_> {
        if self.limited {
            // The run's one source shows it; the saved source is worked out again next time
            // (a pane saved on Coinbase switched to ES under `--source mock` mustn't keep it).
            return Move::Same { forget: true };
        }
        let own = self.own(saved);
        match self.for_pick(own, target) {
            // Another source serves it, or the pane's saved source isn't in this run (off):
            // it moves to the serving source and saves that one.
            Some(e) if e.name != own || saved != Some(own) => Move::On(e),
            _ => Move::Same { forget: false },
        }
    }
}

/// Where a pane's switch goes (`Sources::plan_move`).
#[derive(Debug)]
pub enum Move<'a> {
    /// On the source the pane's feed already uses; `forget`: save no source (a limited run).
    Same { forget: bool },
    /// Onto this source, which the pane then saves.
    On(&'a SourceEntry),
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

    /// `Ok` if some source can show `target`; else why not: the pane's own source's reason
    /// when it is the only one, else that no source has it.
    pub fn check(&self, target: &Target) -> Result<(), String> {
        match self.own.check(target) {
            Ok(()) => Ok(()),
            Err(why) => {
                if self.others.is_empty() {
                    Err(why)
                } else if self.others.iter().any(|(_, a)| a.check(target).is_ok()) {
                    Ok(())
                } else {
                    Err("no source has it".to_owned())
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
    sources.own(feed.source_doc.as_deref())
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
    match env.sources.plan_move(feed.source_doc.as_deref(), &target) {
        Move::On(entry) => feed.retarget_on(&registry, entry, target),
        Move::Same { forget } => {
            let mut f = feed.retarget(&registry, target)?;
            if forget {
                f.source_doc = None;
            }
            Ok(f)
        }
    }
}

/// The one-click switch on an unavailable pane to its source's own symbol, through
/// [`retarget`] (so a pane whose saved source is off moves to, and saves, the one serving it).
pub fn to_suggested(
    feed: &crate::feeds::PaneFeed,
    cx: &gpui::App,
) -> anyhow::Result<crate::feeds::PaneFeed> {
    let s = feed
        .suggest
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("this source offers no symbol"))?;
    retarget(feed, s.target.clone(), cx)
}

/// [`pick`] while the picker is open; its own source only otherwise (the overlay draws
/// nothing then, so no list is built on every frame).
pub fn pick_if(open: bool, feed: &crate::feeds::PaneFeed, cx: &gpui::App) -> Pick {
    if open {
        pick(feed, cx)
    } else {
        Pick::one(&feed.availability)
    }
}

/// The pane's source as its header names it after the contract ("ESZ26 (Replay)"), in a run
/// with several sources (decision 0030); nothing when one source serves the run.
pub fn header_tag(feed: &crate::feeds::PaneFeed, cx: &gpui::App) -> Option<String> {
    let env = crate::workspace::try_env(cx)?;
    let sources = &env.sources;
    if sources.limited() || sources.all().len() < 2 {
        return None;
    }
    Some(source_label(current(sources, feed)))
}

/// Append the header tag to a header's contract text.
pub fn write_header_tag(out: &mut String, feed: &crate::feeds::PaneFeed, cx: &gpui::App) {
    if let Some(tag) = header_tag(feed, cx) {
        out.push_str(" (");
        out.push_str(&tag);
        out.push(')');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::catalog;

    fn entry(name: &str, source: lines_feed::Source) -> SourceEntry {
        let base = FeedConfig::new(source);
        SourceEntry {
            name: name.to_owned(),
            availability: lines_feed::availability::availability(&base.source).unwrap(),
            fallback: Fallback::default(),
            base,
        }
    }

    /// The mock (ES only) and Coinbase (spot crypto): what each pane opens on.
    fn two(limited: bool) -> Sources {
        Sources::new(
            vec![
                entry(
                    "mock",
                    lines_feed::Source::CqgMock(lines_feed::cqg::mock::MockConfig::default()),
                ),
                entry(
                    "coinbase",
                    lines_feed::Source::Coinbase {
                        url: lines_feed::coinbase::WS_URL.into(),
                    },
                ),
            ],
            limited,
        )
    }

    fn doc(product: lines_core::ProductId) -> TargetDoc {
        TargetDoc::from_target(&Target::Product(product))
    }

    fn on(m: Move<'_>) -> Option<String> {
        match m {
            Move::On(e) => Some(e.name.clone()),
            Move::Same { .. } => None,
        }
    }

    #[test]
    fn a_switch_moves_the_pane_to_the_source_serving_it_and_saves_that_one() {
        let s = two(false);
        let es = Target::Product(catalog::ES);
        let btc = Target::Product(catalog::BTC_USD);
        // Its own source serves it: it stays.
        assert!(matches!(
            s.plan_move(Some("mock"), &es),
            Move::Same { forget: false }
        ));
        // Another serves it: it moves there.
        assert_eq!(
            on(s.plan_move(Some("mock"), &btc)).as_deref(),
            Some("coinbase")
        );
        // Its saved source is off (not in the run): its feed is on the primary, and even a
        // pick the primary serves moves it onto (and saves) the primary, so it can't stay
        // stuck on the source that's off.
        assert_eq!(
            on(s.plan_move(Some("kraken"), &es)).as_deref(),
            Some("mock")
        );
        assert_eq!(on(s.plan_move(None, &es)).as_deref(), Some("mock"));
        // A run `--source` limits never moves a pane, and the saved source is worked out
        // again next time (a pane saved on Coinbase switched to ES there mustn't keep it).
        let limited = two(true);
        assert!(matches!(
            limited.plan_move(Some("coinbase"), &es),
            Move::Same { forget: true }
        ));
        // `--instrument` replaces every pane's symbol, and its saved source with it.
        let mut w = crate::wsdoc::builtin("pair").unwrap();
        w.windows[0].tabs[0].layout = w.windows[0].tabs[0].layout.map(&mut |p| {
            let mut p = p.clone();
            p.source = Some("kraken".into());
            p
        });
        w.retarget_all(Some(doc(catalog::BTC_USD)));
        let leaves = w.windows[0].tabs[0].layout.leaves();
        assert!(!leaves.is_empty() && leaves.iter().all(|p| p.source.is_none()));
    }

    #[test]
    fn a_pane_opens_on_its_saved_source_else_the_one_serving_its_symbol() {
        let s = two(false);
        let es = doc(catalog::ES);
        let btc = doc(catalog::BTC_USD);
        assert_eq!(s.for_pane(None, Some(&es)).unwrap().name, "mock");
        assert_eq!(s.for_pane(None, Some(&btc)).unwrap().name, "coinbase");
        assert_eq!(
            s.for_pane(Some("coinbase"), Some(&btc)).unwrap().name,
            "coinbase"
        );
        // No target: the preferred source.
        assert_eq!(s.for_pane(None, None).unwrap().name, "mock");
    }

    #[test]
    fn a_saved_source_that_is_off_makes_only_that_pane_unavailable() {
        let s = two(false);
        let err = s
            .for_pane(Some("kraken"), Some(&doc(catalog::BTC_USD)))
            .unwrap_err();
        assert_eq!(err.0, "Kraken is off or not in this build");
        // A pane that shows no market (a journal) is never unavailable for its source.
        assert_eq!(s.for_pane(Some("kraken"), None).unwrap().name, "mock");
        // Nothing serves NQ here (the mock is ES only).
        let err = s.for_pane(None, Some(&doc(catalog::NQ))).unwrap_err();
        assert_eq!(err.0, "No source has NQ: pick another symbol");
    }

    #[test]
    fn a_run_limited_by_source_puts_every_pane_on_it_as_before() {
        let s = two(true);
        let btc = doc(catalog::BTC_USD);
        assert_eq!(
            s.for_pane(Some("coinbase"), Some(&btc)).unwrap().name,
            "mock"
        );
        assert_eq!(s.for_pane(None, Some(&btc)).unwrap().name, "mock");
    }

    #[test]
    fn the_picker_offers_every_sources_symbols_and_tags_them() {
        let s = two(false);
        let mock = s.get("mock").unwrap();
        let p = Pick {
            own: mock.availability.clone(),
            others: vec![(
                "coinbase".into(),
                s.get("coinbase").unwrap().availability.clone(),
            )],
            own_name: Some("mock".into()),
        };
        let btc = Target::Product(catalog::BTC_USD);
        let es = Target::Product(catalog::ES);
        assert!(p.check(&btc).is_ok(), "another source serves it");
        assert_eq!(p.tag(&btc).as_deref(), Some("Coinbase"));
        assert_eq!(p.tag(&es).as_deref(), Some("Mock"));
        assert_eq!(
            p.check(&Target::Product(catalog::NQ)),
            Err("no source has it".to_owned()),
            "with several, why names no one source"
        );
        assert_eq!(s.for_pick("mock", &btc).unwrap().name, "coinbase");
        assert_eq!(s.for_pick("mock", &es).unwrap().name, "mock");
        // One source: no tags, its own rules only.
        let one = Pick::one(&mock.availability);
        assert!(one.check(&btc).is_err());
        assert_eq!(one.tag(&es), None);
    }

    #[test]
    fn a_panes_source_and_the_source_switches_are_optional_saved_fields() {
        let mut d = crate::wsdoc::PaneDoc::new(crate::layout::PaneKind::Chart, None, None);
        let plain = serde_json::to_string(&d).unwrap();
        assert!(!plain.contains("source"), "{plain}");
        d.source = Some("coinbase".into());
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(r#""source":"coinbase""#), "{json}");
        let back: crate::wsdoc::PaneDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source.as_deref(), Some("coinbase"));
        // Settings: absent is on; off is saved.
        let s: crate::picker::Settings = serde_json::from_str("{}").unwrap();
        assert!(s.sources.is_empty());
        let mut s = s;
        s.sources.insert(
            "kraken".into(),
            SourcePrefs {
                enabled: Some(false),
            },
        );
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(r#""sources":{"kraken":{"enabled":false}}"#),
            "{json}"
        );
        assert!(!s.sources["kraken"].on());
    }
}
