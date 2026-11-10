//! The data sources this run can show (decision 0030, step 1): several at once.
//!
//! One entry per enabled source, built once at startup: its feed config (its own store),
//! what it can serve ([`Availability`]) and what it offers an unservable pane ([`Fallback`]).
//! Nothing connects here: a source's feed starts when a pane first subscribes to one of its
//! contracts (the registry, refcounted) and stops when none does, so a source no pane shows
//! costs nothing, and a source failing or going stale reaches only its own panes.
//!
//! A source whose login is refused (a `LogonRejected` feed: credentials or permissions)
//! loses the choice for this run: panes on it move to the next source serving their symbol,
//! new panes and picks skip it, and the picker's tag says why ("Databento (Rithmic: login
//! refused)"), as Settings › Connections does (the provider's reason). Not set up is the
//! source's own `check` (an extension) or its absence (Databento without a key). A restart
//! tries the login again for panes still saved on it; a pane that moved saves the source it
//! moved to.
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
    /// Sources whose login was refused this run, with the provider's reason (shared by the
    /// clones panes hold).
    refused: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

/// Why a pane can't open on any source (its target and the reason, for the pane).
#[derive(Clone, Debug, PartialEq)]
pub struct NoSource(pub String);

impl Sources {
    /// The run's sources. `entries` is never empty (startup checks).
    pub fn new(entries: Vec<SourceEntry>, limited: bool) -> Sources {
        assert!(!entries.is_empty(), "a run has at least one source");
        Sources {
            entries,
            limited,
            refused: Default::default(),
        }
    }

    /// The source refused its login (`why`: the provider's reason): it loses every choice this
    /// run. `true` the first time.
    pub fn refuse(&self, name: &str, why: &str) -> bool {
        let mut r = self.refused_list();
        if r.iter().any(|(n, _)| n == name) {
            return false;
        }
        r.push((name.to_owned(), why.to_owned()));
        true
    }

    /// Why the source's login was refused this run (`None`: it wasn't).
    pub fn refusal(&self, name: &str) -> Option<String> {
        self.refused_list()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, why)| why.clone())
    }

    /// The run's source a feed config is on (by the registry's key).
    pub fn name_of(&self, source: &lines_feed::Source) -> Option<&str> {
        let key = lines_feed::registry::source_key(source);
        self.entries
            .iter()
            .find(|e| lines_feed::registry::source_key(&e.base.source) == key)
            .map(|e| e.name.as_str())
    }

    fn refused_list(&self) -> std::sync::MutexGuard<'_, Vec<(String, String)>> {
        self.refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the source refused its login this run.
    pub fn refused(&self, name: &str) -> bool {
        self.refused_list().iter().any(|(n, _)| n == name)
    }

    /// The source serving `target`: the first whose login wasn't refused, else (only refused
    /// ones serve it) the first; `None`: none serves it.
    fn serving(&self, target: &Target) -> Option<&SourceEntry> {
        let mut serving = self
            .entries
            .iter()
            .filter(|e| e.availability.check(target).is_ok());
        let first = serving.next()?;
        if !self.refused(&first.name) {
            return Some(first);
        }
        serving.find(|e| !self.refused(&e.name)).or(Some(first))
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

    /// The source a pane opens on: the one it saved (if the run has it), unless its login was
    /// refused and another serves the target; else, limited, the run's; else the first that
    /// serves its target (a refused one last); a pane with no target takes the primary.
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
        let saved = match saved {
            Some(name) => {
                let e = self
                    .get(name)
                    .ok_or_else(|| NoSource(missing_reason(name)))?;
                if !self.refused(name) {
                    return Ok(e);
                }
                Some(e)
            }
            None => None,
        };
        let Some(t) = target.map(TargetDoc::to_target) else {
            return Ok(self.primary());
        };
        let t = t.map_err(NoSource)?;
        // A saved source whose login was refused: the next serving it, else still the saved.
        let found = match saved {
            Some(e) => Some(
                self.serving(&t)
                    .filter(|f| !self.refused(&f.name))
                    .unwrap_or(e),
            ),
            None => self.serving(&t),
        };
        found.ok_or_else(|| {
            // What's missing; the pane under it and the tab's banner give the way out
            // (Ctrl+K, a one-click switch to a symbol a source has).
            NoSource(format!(
                "no source has {}",
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

    /// What a first run opens on (decision 0030): `None` for `pair` (NQ | ES) when a source
    /// in the run serves ES; else a product one serves (BTC, then ETH; Coinbase's before
    /// Kraken's), else the primary source's own symbol, else BTC/USD (a source serving none
    /// of these and offering no symbol: its panes then say so).
    pub fn first_run_product(&self) -> Option<lines_core::ProductId> {
        use lines_feed::catalog;
        let serves = |p| {
            self.entries
                .iter()
                .any(|e| e.availability.check(&Target::Product(p)).is_ok())
        };
        if serves(catalog::ES) {
            return None;
        }
        [
            catalog::BTC_USD,
            catalog::BTC_USD_KR,
            catalog::BTC_PERP_KR,
            catalog::ETH_USD,
            catalog::ETH_USD_KR,
        ]
        .into_iter()
        .find(|&p| serves(p))
        .or_else(|| {
            self.primary()
                .fallback
                .suggest
                .as_ref()
                .map(|s| match &s.target {
                    Target::Product(p) => *p,
                    Target::Contract(k) => k.product,
                })
        })
        .or(Some(catalog::BTC_USD))
    }

    /// The source a pick goes to: the pane's own if it serves the target and its login wasn't
    /// refused, else the first that does and wasn't, else (only refused ones serve it) the
    /// pane's own if it serves it, else the first (`None`: none serves it).
    pub fn for_pick(&self, current: &str, target: &Target) -> Option<&SourceEntry> {
        let own = self
            .get(current)
            .filter(|e| e.availability.check(target).is_ok());
        own.filter(|e| !self.refused(&e.name))
            .or_else(|| self.serving(target).filter(|e| !self.refused(&e.name)))
            .or(own)
            .or_else(|| self.serving(target))
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
    /// Sources whose login was refused this run (they lose the tag to another serving).
    refused: Vec<String>,
}

impl Pick {
    /// Only the pane's own source (a run `--source` limits, and tests).
    pub fn one(own: &Availability) -> Pick {
        Pick {
            own: own.clone(),
            others: Vec::new(),
            own_name: None,
            refused: Vec::new(),
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

    /// The source a row's tag names, in a run with several, as [`Sources::for_pick`] picks:
    /// the pane's own when it serves the target, else the first that does, a source whose
    /// login was refused last. When a refused source also serves it, the tag says so: the
    /// fallthrough "Databento (Rithmic: login refused)", or only "Rithmic: login refused" when
    /// no other does. `None`: one source, or none serves it.
    pub fn tag(&self, target: &Target) -> Option<String> {
        let own = self.own_name.as_ref()?;
        let serving: Vec<&String> = std::iter::once((own, &self.own))
            .chain(self.others.iter().map(|(n, a)| (n, a)))
            .filter(|(_, a)| a.check(target).is_ok())
            .map(|(n, _)| n)
            .collect();
        let refused = serving.iter().find(|n| self.refused.contains(n));
        match (serving.iter().find(|n| !self.refused.contains(n)), refused) {
            (Some(n), None) => Some(source_label(n)),
            (Some(n), Some(r)) => Some(format!("{} ({})", source_label(n), refused_label(r))),
            (None, r) => r.map(|r| refused_label(r)),
        }
    }
}

/// Why a saved source isn't in this run. Databento without a key is "not set up" (decision
/// 0033): it makes no requests until the key exists. Reads the startup lookup, never starts one.
fn missing_reason(name: &str) -> String {
    missing_reason_with(name, crate::databento_known())
}

fn missing_reason_with(name: &str, databento: Option<bool>) -> String {
    // On, but missing a setting it describes (decision 0030 §6): say so and where.
    if let Some(needs) = crate::connections::not_set_up(name) {
        return format!(
            "{} isn't set up: needs {needs}. Set it up in Settings › Connections",
            source_label(name)
        );
    }
    if name == crate::DATABENTO && databento == Some(false) {
        return "Databento is not set up: no API key. Set it up in Settings › Connections"
            .to_owned();
    }
    format!("{} is off or not in this build", source_label(name))
}

/// A source whose login was refused, as the picker's tag and a pane's header name it.
pub fn refused_label(name: &str) -> String {
    format!("{}: login refused", source_label(name))
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
        refused: sources
            .refused_list()
            .iter()
            .map(|(n, _)| n.clone())
            .collect(),
    }
}

/// The pane's feed stopped because its source refused the login: the run stops choosing that
/// source, and the pane moves to the next source serving its symbol (saving that one, as a
/// pick would). `None`: it stays (a limited run, no symbol, or no other source has it; its
/// header then says the login was refused).
pub fn off_refused(
    feed: &crate::feeds::PaneFeed,
    cx: &gpui::App,
) -> Option<crate::feeds::PaneFeed> {
    let env = crate::workspace::try_env(cx)?;
    let sources = &env.sources;
    if sources.limited() {
        return None;
    }
    // The source the refused store's feed is on (not the saved one: a pane with no market
    // keeps its saved source while it runs on the primary).
    let name = sources.name_of(&feed.config.source)?.to_owned();
    let why = match &feed.store().read().status {
        lines_feed::store::FeedStatus::Failed(why) => why.clone(),
        _ => "login refused".to_owned(),
    };
    if sources.refuse(&name, &why) {
        tracing::warn!(
            "{}: login refused; panes move to the next source that has their symbol",
            source_label(&name)
        );
    }
    let target = feed.target_doc.as_ref()?.to_target().ok()?;
    let entry = sources.for_pick(&name, &target)?;
    if entry.name == name {
        return None;
    }
    let registry = crate::feeds::Feeds::registry(cx);
    feed.retarget_on(&registry, entry, target)
        .inspect_err(|e| tracing::warn!("moving a pane off {name}: {e:#}"))
        .ok()
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
    let name = current(sources, feed);
    // This pane's own feed was refused (never another pane's: a vendor may refuse one
    // connection only).
    Some(if feed.store().logon_refused() {
        refused_label(name)
    } else {
        source_label(name)
    })
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

    /// Decision 0030: a first run opens on a symbol the run's sources serve: `pair` with a
    /// futures source, else BTC/USD on Coinbase, Kraken's BTC/USD with Kraken alone.
    #[test]
    fn a_first_run_opens_on_a_symbol_the_runs_sources_serve() {
        assert_eq!(two(false).first_run_product(), None, "the mock serves ES");
        let coinbase = || {
            entry(
                "coinbase",
                lines_feed::Source::Coinbase {
                    url: lines_feed::coinbase::WS_URL.into(),
                },
            )
        };
        let kraken = || {
            entry(
                "kraken",
                lines_feed::Source::Kraken {
                    spot_url: lines_feed::kraken::SPOT_WS_URL.into(),
                    futures_url: lines_feed::kraken::FUTURES_WS_URL.into(),
                },
            )
        };
        let plain = Sources::new(vec![coinbase(), kraken()], false);
        assert_eq!(plain.first_run_product(), Some(catalog::BTC_USD));
        let kraken_only = Sources::new(vec![kraken()], false);
        assert_eq!(kraken_only.first_run_product(), Some(catalog::BTC_USD_KR));
        let limited = Sources::new(vec![kraken()], true);
        assert_eq!(limited.first_run_product(), Some(catalog::BTC_USD_KR));
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
        // Databento without a key says so; switched off (never looked up) it is "off".
        assert_eq!(
            missing_reason_with("databento", Some(false)),
            "Databento is not set up: no API key. Set it up in Settings › Connections"
        );
        assert_eq!(
            missing_reason_with("databento", None),
            "Databento is off or not in this build"
        );
        assert_eq!(
            missing_reason_with("databento", Some(true)),
            "Databento is off or not in this build"
        );
        // On but not set up: the pane says so, and where (decision 0030 §6).
        crate::connections::mark_not_set_up("acme-not-set-up", "API key");
        let err = s
            .for_pane(Some("acme-not-set-up"), Some(&doc(catalog::BTC_USD)))
            .unwrap_err();
        assert_eq!(
            err.0,
            "Acme-not-set-up isn't set up: needs API key. Set it up in Settings › Connections"
        );
        // A pane that shows no market (a journal) is never unavailable for its source.
        assert_eq!(s.for_pane(Some("kraken"), None).unwrap().name, "mock");
        // Nothing serves NQ here (the mock is ES only).
        let err = s.for_pane(None, Some(&doc(catalog::NQ))).unwrap_err();
        assert_eq!(err.0, "no source has NQ");
        // A contract is named as a contract.
        let nqz = TargetDoc::from_target(&Target::Contract(lines_core::ContractKey::monthly(
            catalog::NQ,
            2026,
            lines_core::Month::new(12).unwrap(),
        )));
        assert_eq!(
            s.for_pane(None, Some(&nqz)).unwrap_err().0,
            "no source has NQZ26"
        );
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

    /// Two sources serving ES (an extension first, as Rithmic in the official build, then
    /// Databento) and Coinbase.
    fn three() -> Sources {
        let mock = |name: &str| {
            entry(
                name,
                lines_feed::Source::CqgMock(lines_feed::cqg::mock::MockConfig::default()),
            )
        };
        Sources::new(
            vec![
                mock("rithmic"),
                mock("databento"),
                entry(
                    "coinbase",
                    lines_feed::Source::Coinbase {
                        url: lines_feed::coinbase::WS_URL.into(),
                    },
                ),
            ],
            false,
        )
    }

    /// The owner's run (2026-11-10): Rithmic first, its test login refused ("logon rejected
    /// (code 13) permission denied"), so ES panes showed nothing though Databento has ES. A
    /// refused source loses every choice to the next serving the symbol, saved or not.
    #[test]
    fn a_source_whose_login_was_refused_loses_every_choice() {
        let s = three();
        let es = doc(catalog::ES);
        let es_t = Target::Product(catalog::ES);
        assert_eq!(s.for_pane(None, Some(&es)).unwrap().name, "rithmic");
        assert!(s.refuse("rithmic", "logon rejected (code 13): permission denied"));
        assert!(!s.refuse("rithmic", "again"), "once");
        assert_eq!(
            s.refusal("rithmic").as_deref(),
            Some("logon rejected (code 13): permission denied"),
            "the first reason, for Settings › Connections"
        );
        assert_eq!(s.refusal("databento"), None);
        assert!(
            s.clone().refused("rithmic"),
            "every clone (the panes') knows"
        );
        // New panes, saved panes, picks and switches go to Databento.
        assert_eq!(s.for_pane(None, Some(&es)).unwrap().name, "databento");
        assert_eq!(
            s.for_pane(Some("rithmic"), Some(&es)).unwrap().name,
            "databento"
        );
        assert_eq!(s.for_pick("rithmic", &es_t).unwrap().name, "databento");
        assert_eq!(
            on(s.plan_move(Some("rithmic"), &es_t)).as_deref(),
            Some("databento")
        );
        // Others are untouched: BTC stays on Coinbase, a pane with no market on the primary.
        assert_eq!(
            s.for_pane(None, Some(&doc(catalog::BTC_USD))).unwrap().name,
            "coinbase"
        );
        assert_eq!(s.for_pane(Some("rithmic"), None).unwrap().name, "rithmic");
        // The picker's tag names where a pick goes, and why a refused source would be it.
        let avail = |n: &str| s.get(n).unwrap().availability.clone();
        let pick = |own: &str, refused: &[&str]| Pick {
            own: avail(own),
            others: ["rithmic", "databento", "coinbase"]
                .into_iter()
                .filter(|n| *n != own)
                .map(|n| (n.to_owned(), avail(n)))
                .collect(),
            own_name: Some(own.to_owned()),
            refused: refused.iter().map(|n| (*n).to_owned()).collect(),
        };
        let fell = Some("Databento (Rithmic: login refused)");
        assert_eq!(pick("rithmic", &["rithmic"]).tag(&es_t).as_deref(), fell);
        assert_eq!(pick("coinbase", &["rithmic"]).tag(&es_t).as_deref(), fell);
        assert_eq!(pick("databento", &["rithmic"]).tag(&es_t).as_deref(), fell);
        assert_eq!(
            pick("rithmic", &["rithmic"])
                .tag(&Target::Product(catalog::BTC_USD))
                .as_deref(),
            Some("Coinbase"),
            "a symbol the refused source doesn't serve says nothing of it"
        );
        assert_eq!(pick("coinbase", &[]).tag(&es_t).as_deref(), Some("Rithmic"));
        // Only refused sources serve it: still the first, saying why.
        assert!(s.refuse("databento", "bad key"));
        assert_eq!(s.for_pane(None, Some(&es)).unwrap().name, "rithmic");
        assert_eq!(
            s.for_pane(Some("databento"), Some(&es)).unwrap().name,
            "databento",
            "a refused saved source stays when no other serves it"
        );
        assert_eq!(s.for_pick("databento", &es_t).unwrap().name, "databento");
        assert_eq!(
            pick("coinbase", &["rithmic", "databento"])
                .tag(&es_t)
                .as_deref(),
            Some("Rithmic: login refused")
        );
        assert!(
            pick("coinbase", &["rithmic", "databento"])
                .check(&es_t)
                .is_ok()
        );
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
            refused: Vec::new(),
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
