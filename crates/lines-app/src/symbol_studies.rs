//! Studies per symbol (decision 0039): a chart's studies (VWAPs, the volume profile, Volume,
//! level sources, external series and their settings, the study label placement) belong to its
//! **product** and are shared by every chart showing it, in every window and tab.
//!
//! - The sets live app-wide in `settings.json` (`Settings::symbol_studies`, keyed by the
//!   catalog product id, so a roll keeps them; optional, no format bump). A product without one
//!   starts from what the chart shows: a new chart's defaults (`chart_defaults`), else a switch
//!   to it starts from `chart_defaults`.
//! - A chart joins its product's set when restored or switched, and at render compares what it
//!   shows with what it last synced: a change made on it is published to the product's set
//!   (every chart of the product picks it up: [`generation`] moves, each chart's poll sees it),
//!   and a published change from another chart is taken. The engine shares identical (feed,
//!   spec) nodes, so two charts of one product with the same VWAP still use one node.
//! - **Separate studies** (`ChartState::separate_studies`): one chart keeps its own set, copied
//!   from the product's when turned on, saved on the chart pane; turned off, the chart takes its
//!   product's set again. A switch to another symbol ends it (the chart shows the new symbol's
//!   set, decision 0039 §3).
//! - Visibility (hide studies, hide study labels, Clean chart) stays the chart's own.
//! - Migration (workspace format 9, [`adopt_legacy`]): charts saved before carried their own
//!   sets; the first chart of each product (window, tab and layout order) gives the product its
//!   set, a later chart of it with a different set keeps it as separate studies, so nothing is
//!   lost.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::App;

use crate::layout::{Layout, PaneKind};
use crate::study_menu::StudySet;
use crate::wsdoc::{ChartState, PaneDoc, WorkspaceDoc};

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Moves whenever any product's set is published: every shared chart compares its own then.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Sets changed outside a chart (a migration): every shared chart syncs again.
pub fn bump() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// The product's set, if it has one.
pub fn get(cx: &App, product: u32) -> Option<StudySet> {
    crate::picker_view::settings(cx)
        .symbol_studies
        .get(&product)
        .cloned()
}

/// A change made on a chart of `product`: its set (saved; every chart of it follows).
pub fn publish(cx: &mut App, product: u32, set: StudySet) {
    let mut changed = false;
    crate::picker_view::update_settings(cx, |s| {
        if s.symbol_studies.get(&product) != Some(&set) {
            s.symbol_studies.insert(product, set);
            changed = true;
        }
    });
    // Screenshots and tests keep their settings in memory, unsaved: the charts follow too.
    if changed {
        GENERATION.fetch_add(1, Ordering::AcqRel);
    }
}

/// A chart pane's study set as saved on it.
pub fn set_of(state: &ChartState) -> StudySet {
    StudySet {
        indicators: state.indicators.clone(),
        profile: state.profile,
        levels: state.levels.clone(),
        footprint: state.footprint,
        styles: state.styles.clone(),
        trades_off: false,
    }
}

/// A chart pane that shares its product's set saves none of it.
pub fn clear(state: &mut ChartState) {
    let empty = StudySet::default();
    state.indicators = empty.indicators;
    state.profile = empty.profile;
    state.levels = empty.levels;
    state.footprint = empty.footprint;
    state.styles = empty.styles;
}

/// Charts saved before studies were per symbol (marked `legacy_studies` by the format 9
/// migration), in window, tab and layout order: the first chart of each product gives it its
/// set (unless `sets` has one already: another workspace's, migrated before); a later one with
/// the same set shares it; one with a different set, or with no product, keeps its own as
/// separate studies. Nothing is lost. Returns whether `sets` changed.
pub fn adopt_legacy(doc: &mut WorkspaceDoc, sets: &mut BTreeMap<u32, StudySet>) -> bool {
    let mut changed = false;
    for w in &mut doc.windows {
        for t in &mut w.tabs {
            each_pane(&mut t.layout, &mut |p| changed |= adopt(p, sets));
        }
    }
    changed
}

fn each_pane(l: &mut Layout<PaneDoc>, f: &mut impl FnMut(&mut PaneDoc)) {
    match l {
        Layout::Pane(p) => f(p),
        Layout::Split { first, second, .. } => {
            each_pane(first, f);
            each_pane(second, f);
        }
    }
}

fn adopt(p: &mut PaneDoc, sets: &mut BTreeMap<u32, StudySet>) -> bool {
    if p.kind != PaneKind::Chart {
        return false;
    }
    let product = p.target.as_ref().map(|t| t.product);
    let Some(state) = p.chart.as_mut().filter(|s| s.legacy_studies) else {
        return false;
    };
    state.legacy_studies = false;
    let own = set_of(state);
    let Some(product) = product else {
        // No product to key it by: its own, kept.
        state.separate_studies = true;
        return false;
    };
    match sets.get(&product) {
        None => {
            sets.insert(product, own);
            clear(state);
            true
        }
        Some(s) if *s == own => {
            clear(state);
            false
        }
        Some(_) => {
            state.separate_studies = true;
            false
        }
    }
}

/// The format 9 migration: every chart pane saved before carried its own studies; mark them,
/// for [`adopt_legacy`] (which needs the app's settings) to sort out after loading.
pub fn mark_legacy(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            if o.get("kind").and_then(|k| k.as_str()) == Some("chart")
                && let Some(serde_json::Value::Object(c)) = o.get_mut("chart")
            {
                c.insert("legacy_studies".into(), serde_json::Value::Bool(true));
            }
            for (_, child) in o.iter_mut() {
                mark_legacy(child);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(mark_legacy),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::versioned::{Decoded, decode};
    use crate::wsdoc::{FORMAT, TargetDoc};
    use lines_indicators::IndicatorConfig;

    fn vwap(reset: &str) -> IndicatorConfig {
        serde_json::from_str(&format!(
            r#"{{"kind": "vwap", "reset": "{reset}", "bands": 0}}"#
        ))
        .unwrap_or_else(|e| panic!("{e}"))
    }

    fn chart(product: Option<u32>, indicators: Vec<IndicatorConfig>) -> PaneDoc {
        let mut p = PaneDoc::new(
            PaneKind::Chart,
            None,
            product.map(|product| TargetDoc {
                product,
                symbol: None,
                contract: None,
            }),
        );
        let mut s: ChartState = serde_json::from_str(r#"{"bar_px": 6.0}"#).unwrap();
        s.indicators = indicators;
        s.legacy_studies = true;
        p.chart = Some(s);
        p
    }

    fn doc(panes: Vec<PaneDoc>) -> WorkspaceDoc {
        let mut d = WorkspaceDoc::first_run(None);
        let mut it = panes.into_iter();
        let mut l = Layout::Pane(it.next().unwrap());
        for p in it {
            l = Layout::Split {
                axis: crate::layout::Axis::Row,
                ratio: 0.5,
                first: Box::new(l),
                second: Box::new(Layout::Pane(p)),
            };
        }
        d.windows[0].tabs.truncate(1);
        d.windows[0].tabs[0].layout = l;
        d
    }

    fn states(d: &WorkspaceDoc) -> Vec<ChartState> {
        d.windows[0].tabs[0]
            .layout
            .leaves()
            .iter()
            .filter_map(|p| p.chart.clone())
            .collect()
    }

    /// The first chart of each product gives it its set; a later one with the same set
    /// shares it; one with a different set, or no product, keeps its own; nothing is lost.
    #[test]
    fn migration_gives_each_product_its_first_charts_set() {
        let (es, nq) = (1, 2);
        let mut d = doc(vec![
            chart(Some(es), vec![vwap("eth")]),
            chart(Some(es), vec![vwap("eth")]),
            chart(Some(es), vec![vwap("rth")]),
            chart(Some(nq), vec![]),
            chart(None, vec![vwap("weekly")]),
        ]);
        let mut sets = BTreeMap::new();
        assert!(adopt_legacy(&mut d, &mut sets));
        assert_eq!(
            sets[&es].indicators,
            vec![vwap("eth")],
            "the first ES chart's"
        );
        assert_eq!(sets[&nq].indicators, vec![], "NQ's, empty as its chart was");
        let s = states(&d);
        assert!(s.iter().all(|s| !s.legacy_studies));
        // Identical: shared, nothing of it saved on the chart.
        assert!(!s[0].separate_studies && s[0].indicators.is_empty());
        assert!(!s[1].separate_studies && s[1].indicators.is_empty());
        // Different: its own, kept.
        assert!(s[2].separate_studies);
        assert_eq!(s[2].indicators, vec![vwap("rth")]);
        // No product: its own, kept.
        assert!(s[4].separate_studies);
        assert_eq!(s[4].indicators, vec![vwap("weekly")]);
        // Again (an unsaved migration loaded twice): nothing changes.
        assert!(!adopt_legacy(&mut d, &mut sets));
    }

    /// A product that already has a set (another workspace migrated before) keeps it: this
    /// workspace's differing chart keeps its own.
    #[test]
    fn a_set_from_another_workspace_wins() {
        let mut sets = BTreeMap::from([(1, StudySet::default())]);
        let mut d = doc(vec![chart(Some(1), vec![vwap("eth")])]);
        assert!(!adopt_legacy(&mut d, &mut sets));
        assert!(states(&d)[0].separate_studies);
        assert_eq!(sets[&1], StudySet::default());
    }

    /// A version 8 file reads as version 9 with its chart panes (only those) marked as
    /// carrying their own studies; a version 9 file's are not.
    #[test]
    fn version_8_charts_are_marked_for_adoption() {
        let v8 = br#"{
          "version": 8,
          "name": "desk",
          "windows": [{
            "tabs": [{
              "focused": 0,
              "layout": {"split": {"axis": "row", "ratio": 0.6,
                "first": {"pane": {"kind": "chart", "link": 0, "state": "shown",
                  "target": {"product": 1},
                  "chart": {"bar_px": 6.0, "indicators": [{"kind": "vwap", "reset": "eth", "bands": 0}]}}},
                "second": {"pane": {"kind": "dom", "link": 0, "state": "shown"}}}}
            }]
          }]
        }"#;
        let Decoded::Ok {
            value: mut d,
            migrated_from: Some(8),
        } = decode::<WorkspaceDoc>(&FORMAT, v8)
        else {
            panic!("v8 no longer reads")
        };
        let leaves = d.windows[0].tabs[0].layout.leaves();
        assert!(leaves[0].chart.as_ref().unwrap().legacy_studies);
        assert!(leaves[1].chart.is_none());
        let mut sets = BTreeMap::new();
        assert!(adopt_legacy(&mut d, &mut sets));
        assert_eq!(sets[&1].indicators, vec![vwap("eth")]);
        // Saved again: version 9, the mark gone, the shared chart's studies with the product.
        let bytes = crate::versioned::encode(&FORMAT, &d).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains(r#""version": 9"#) && !text.contains("legacy"),
            "{text}"
        );
        assert!(!text.contains("vwap"), "{text}");
    }

    /// The per-symbol sets round-trip through the settings file, and are absent until used.
    #[test]
    fn settings_hold_each_products_set() {
        let mut s = crate::picker::Settings::default();
        assert!(
            !String::from_utf8(s.encode())
                .unwrap()
                .contains("symbol_studies")
        );
        s.symbol_studies.insert(
            2,
            StudySet {
                indicators: vec![vwap("rth")],
                ..Default::default()
            },
        );
        let text = String::from_utf8(s.encode()).unwrap();
        let Decoded::Ok { value, .. } =
            decode::<crate::picker::Settings>(&crate::picker::SETTINGS_FORMAT, text.as_bytes())
        else {
            panic!("{text}")
        };
        assert_eq!(value.symbol_studies, s.symbol_studies);
    }
}
