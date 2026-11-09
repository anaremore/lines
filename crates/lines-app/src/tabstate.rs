//! What a tab shows: each pane's state and the pure decisions behind hiding, "chart only" and
//! detaching (roadmap 5b).
//!
//! A pane is never removed from its tab's layout when it is hidden or detached: its leaf
//! stays, with every split ratio around it, and only *shown* leaves take space
//! (`Layout::place_where`). Showing or re-attaching it therefore restores exactly the old size
//! and position. Hidden and detached panes keep their link group, settings and feed.
//!
//! The decisions here take the leaves in layout order plus each leaf's kind, link and state,
//! and return the state changes to apply; the workspace applies them. Everything is tested
//! below without GPUI.

use std::collections::HashMap;
use std::hash::Hash;

use serde::{Deserialize, Serialize};

use lines_core::ProductId;

use crate::layout::{LinkGroup, PaneKind};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    #[default]
    Shown,
    /// Hidden by the trader (hotkey, header control, menu).
    Hidden,
    /// Hidden by its chart's "chart only" toggle; turning that off shows it again.
    ChartOnly,
    /// In its own window; its leaf keeps its place here for re-attaching.
    Detached,
}

impl PaneState {
    pub fn is_shown(self) -> bool {
        self == PaneState::Shown
    }

    pub fn is_hidden(self) -> bool {
        matches!(self, PaneState::Hidden | PaneState::ChartOnly)
    }
}

/// What the decisions need to know about a pane.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Leaf {
    pub kind: PaneKind,
    pub link: LinkGroup,
    pub state: PaneState,
}

/// State changes to apply, in layout order.
pub type Changes<Id> = Vec<(Id, PaneState)>;

fn shown_after<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    changes: &Changes<Id>,
) -> usize {
    order
        .iter()
        .filter(|id| {
            changes
                .iter()
                .find(|(c, _)| c == *id)
                .map_or_else(|| leaves[*id].state, |(_, s)| *s)
                .is_shown()
        })
        .count()
}

/// At least one pane must stay shown in a tab.
fn keep_one<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    changes: Changes<Id>,
) -> Result<Changes<Id>, &'static str> {
    if shown_after(order, leaves, &changes) == 0 {
        Err("the last visible pane stays")
    } else {
        Ok(changes)
    }
}

/// Hide pane `id` (DOM and time & sales only: charts are closed or detached instead).
pub fn hide<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    id: Id,
) -> Result<Changes<Id>, &'static str> {
    let leaf = leaves.get(&id).ok_or("no such pane")?;
    if leaf.kind == PaneKind::Chart {
        return Err("charts aren't hidden: close or pop them out");
    }
    if !leaf.state.is_shown() {
        return Err("already hidden");
    }
    keep_one(order, leaves, vec![(id, PaneState::Hidden)])
}

/// Hide or show the DOMs (or tapes) that belong with the focused pane: the focused pane itself
/// if it is one, else those of its kind in its link group in this tab (all of them when it is
/// unlinked). If any is shown they are hidden, otherwise the hidden ones are shown. Detached
/// panes are left alone.
pub fn toggle_kind<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    focused: Id,
    kind: PaneKind,
) -> Result<Changes<Id>, &'static str> {
    let f = leaves.get(&focused).ok_or("no such pane")?;
    if f.kind == kind && f.state.is_shown() {
        return hide(order, leaves, focused);
    }
    let candidates: Vec<Id> = order
        .iter()
        .copied()
        .filter(|id| {
            let l = leaves[id];
            l.kind == kind
                && l.state != PaneState::Detached
                && (f.link.is_none() || l.link == f.link)
        })
        .collect();
    if candidates.is_empty() {
        return Err(match kind {
            PaneKind::Dom => "no DOM goes with this pane",
            PaneKind::Tape => "no time & sales goes with this pane",
            PaneKind::Chart => "no chart goes with this pane",
            PaneKind::Journal => "no journal goes with this pane",
            PaneKind::Strategies => "no strategy panel goes with this pane",
            PaneKind::Chain => "no option chain goes with this pane",
        });
    }
    let any_shown = candidates.iter().any(|id| leaves[id].state.is_shown());
    let changes = candidates
        .into_iter()
        .filter(|id| leaves[id].state.is_shown() == any_shown)
        .map(|id| {
            (
                id,
                if any_shown {
                    PaneState::Hidden
                } else {
                    PaneState::Shown
                },
            )
        })
        .collect();
    keep_one(order, leaves, changes)
}

/// What `toggle_kind` would act on for `kind`, from `focused`: shown (it would hide them),
/// hidden (it would show them), or nothing (`None`). For a menu's checkmark.
pub fn kind_shown<Id: Copy + Eq + Hash>(
    leaves: &HashMap<Id, Leaf>,
    focused: Id,
    kind: PaneKind,
) -> Option<bool> {
    let f = leaves.get(&focused)?;
    if f.kind == kind && f.state.is_shown() {
        return Some(true);
    }
    let mut candidates = leaves.values().filter(|l| {
        l.kind == kind && l.state != PaneState::Detached && (f.link.is_none() || l.link == f.link)
    });
    let first = candidates.next()?;
    Some(first.state.is_shown() || candidates.any(|l| l.state.is_shown()))
}

/// Whether `chart`'s "chart only" is on: some DOM or tape in its link group is hidden by it.
pub fn is_chart_only<Id: Copy + Eq + Hash>(leaves: &HashMap<Id, Leaf>, chart: Id) -> bool {
    let Some(c) = leaves.get(&chart).filter(|c| c.kind == PaneKind::Chart) else {
        return false;
    };
    c.link.is_some()
        && leaves
            .values()
            .any(|l| l.link == c.link && l.state == PaneState::ChartOnly)
}

/// Toggle "chart only" on `chart`: hide the DOMs and tapes in its link group in this tab
/// (marked as hidden by chart only), or show exactly those again. Panes the trader hid
/// themselves stay hidden; detached panes are left alone.
///
/// A refusal says what is actually there: no linked DOM/T&S in this tab, or linked ones that
/// are already hidden or popped out (chart only leaves pop-outs alone), never "none shown"
/// while the trader can see them in their own windows.
pub fn chart_only<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    chart: Id,
) -> Result<Changes<Id>, String> {
    let c = leaves.get(&chart).ok_or("no such pane")?;
    if c.kind != PaneKind::Chart {
        return Err("chart only acts on a chart".into());
    }
    if c.link.is_none() {
        return Err("this chart isn't linked to a DOM or time & sales".into());
    }
    // Its DOMs and time & sales only: a linked journal isn't the chart's.
    let mine = |id: &Id| {
        let l = leaves[id];
        matches!(l.kind, PaneKind::Dom | PaneKind::Tape) && l.link == c.link
    };
    if is_chart_only(leaves, chart) {
        return Ok(order
            .iter()
            .copied()
            .filter(|id| mine(id) && leaves[id].state == PaneState::ChartOnly)
            .map(|id| (id, PaneState::Shown))
            .collect());
    }
    let changes: Changes<Id> = order
        .iter()
        .copied()
        .filter(|id| mine(id) && leaves[id].state.is_shown())
        .map(|id| (id, PaneState::ChartOnly))
        .collect();
    if changes.is_empty() {
        let linked: Vec<PaneState> = order
            .iter()
            .filter(|id| mine(id))
            .map(|id| leaves[id].state)
            .collect();
        return Err(if linked.is_empty() {
            format!(
                "chart only: no DOM or time & sales in link group {} in this tab",
                crate::layout::link_letter(c.link)
            )
        } else if linked.iter().all(|s| *s == PaneState::Detached) {
            "chart only: the linked DOM and time & sales are popped out (left as they are)".into()
        } else {
            "chart only: the linked DOM and time & sales are already hidden or popped out".into()
        });
    }
    Ok(keep_one(order, leaves, changes)?)
}

/// Whether pane `id` can go into its own window (some other pane stays shown).
pub fn can_detach<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    id: Id,
) -> Result<(), &'static str> {
    if !leaves.get(&id).ok_or("no such pane")?.state.is_shown() {
        return Err("only a shown pane can pop out");
    }
    keep_one(order, leaves, vec![(id, PaneState::Detached)]).map(|_| ())
}

/// Where focus goes when the focused pane stops being shown: the next shown pane in layout
/// order, else the previous one.
pub fn refocus<Id: Copy + Eq + Hash>(
    order: &[Id],
    leaves: &HashMap<Id, Leaf>,
    focused: Id,
) -> Option<Id> {
    if leaves.get(&focused).is_some_and(|l| l.state.is_shown()) {
        return Some(focused);
    }
    let i = order.iter().position(|id| *id == focused).unwrap_or(0);
    let shown = |id: &&Id| leaves[*id].state.is_shown();
    order[i..]
        .iter()
        .find(shown)
        .or_else(|| order[..i].iter().rev().find(shown))
        .copied()
}

// ---- unavailable panes ----------------------------------------------------------------

/// Where a tab offers the one-click switch to the source's symbol.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Every pane is served: nothing to offer.
    None,
    /// On each unavailable pane.
    PerPane,
    /// Once for the whole tab (every one of its two or more panes is unavailable): no
    /// identical button repeated in each pane.
    Tab,
}

/// `unavailable[i]`: whether pane `i` of a tab can't be served.
pub fn switch_offer(unavailable: &[bool]) -> Offer {
    match unavailable.iter().filter(|u| **u).count() {
        0 => Offer::None,
        n if n == unavailable.len() && n > 1 => Offer::Tab,
        _ => Offer::PerPane,
    }
}

// ---- tabs -------------------------------------------------------------------------------

/// A window shows its tab bar only with two tabs or more (one tab needs no bar; Ctrl+T, the
/// pane menu and the palette still make a second one, and then the bar appears).
pub fn shows_tab_bar(tabs: usize) -> bool {
    tabs > 1
}

/// What closing a tab does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseTab {
    /// Remove it; this tab index is active afterwards.
    Remove { active: usize },
    /// It was the only tab of a window that isn't the last: the window closes.
    CloseWindow,
    /// It was the only tab of the last window: a fresh tab replaces it (closing always does
    /// something visible).
    ReplaceWithFresh,
}

/// Close tab `i` of `tabs` (with `active` active) in one of `windows` main windows.
pub fn close_tab(tabs: usize, windows: usize, active: usize, i: usize) -> CloseTab {
    if tabs <= 1 {
        return if windows > 1 {
            CloseTab::CloseWindow
        } else {
            CloseTab::ReplaceWithFresh
        };
    }
    let left = tabs - 1;
    // The tab to the right slides into a closed active tab's place; past the end, the left one.
    let active = if active > i { active - 1 } else { active };
    CloseTab::Remove {
        active: active.min(left - 1),
    }
}

/// Recently closed tabs (newest last), each with the index it had, for "Reopen closed tab".
/// Bounded: the oldest are forgotten.
#[derive(Debug)]
pub struct ClosedTabs<T> {
    stack: Vec<(T, usize)>,
    cap: usize,
}

impl<T> ClosedTabs<T> {
    pub fn new(cap: usize) -> ClosedTabs<T> {
        ClosedTabs {
            stack: Vec::new(),
            cap,
        }
    }

    pub fn push(&mut self, tab: T, index: usize) {
        self.stack.push((tab, index));
        if self.stack.len() > self.cap {
            let extra = self.stack.len() - self.cap;
            self.stack.drain(..extra);
        }
    }

    /// The newest closed tab and the index to reopen it at among `len` open tabs.
    pub fn pop(&mut self, len: usize) -> Option<(T, usize)> {
        self.stack.pop().map(|(t, i)| (t, i.min(len)))
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

/// Crosshair sync is keyed by link group (as uPlot's sync key): a chart's crosshair is shown
/// on the charts of its own group, and unlinked charts sync with each other.
pub fn crosshair_peers(from: LinkGroup, to: LinkGroup) -> bool {
    from == to
}

/// The synced crosshair carries its price only with price sync on and between charts of the
/// same product (the same price scale and meaning; another product's price would be a
/// different number on a different axis).
pub fn syncs_price(price_sync: bool, from: ProductId, to: ProductId) -> bool {
    price_sync && from == to
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crosshair_sync_is_keyed_by_link_group() {
        assert!(crosshair_peers(Some(0), Some(0)));
        assert!(!crosshair_peers(Some(0), Some(1)));
        assert!(!crosshair_peers(Some(0), None));
        assert!(!crosshair_peers(None, Some(2)));
        assert!(crosshair_peers(None, None), "unlinked charts share one key");
        let (es, nq) = (ProductId(1), ProductId(2));
        assert!(syncs_price(true, es, es));
        assert!(!syncs_price(true, es, nq));
        assert!(!syncs_price(false, es, es));
    }
    use crate::layout::{Axis, Layout, Preset, Rect};

    /// The pair: NQ (A) chart 1, DOM 2, tape 3 | ES (B) chart 4, DOM 5, tape 6.
    fn pair() -> (Layout<u32>, HashMap<u32, Leaf>) {
        let mut id = 0;
        let mut leaves = HashMap::new();
        let side = |link: u8, id: &mut u32, leaves: &mut HashMap<u32, Leaf>| {
            crate::layout::trading_group(Some(link), &mut |kind, link| {
                *id += 1;
                leaves.insert(
                    *id,
                    Leaf {
                        kind,
                        link,
                        state: PaneState::Shown,
                    },
                );
                *id
            })
        };
        let a = side(0, &mut id, &mut leaves);
        let b = side(1, &mut id, &mut leaves);
        (Layout::split(Axis::Row, 0.5, a, b), leaves)
    }

    fn apply(leaves: &mut HashMap<u32, Leaf>, changes: Changes<u32>) {
        for (id, s) in changes {
            leaves.get_mut(&id).unwrap().state = s;
        }
    }

    fn geometry(l: &Layout<u32>, leaves: &HashMap<u32, Leaf>) -> Vec<(u32, Rect)> {
        let order: Vec<u32> = l.leaves().into_iter().copied().collect();
        l.place_where(
            Rect::new(0.0, 0.0, 1600.0, 900.0),
            &|id| leaves[id].state.is_shown(),
            &|_| 0.0,
            &|_| 0.0,
        )
        .into_iter()
        .map(|p| (order[p.index], p.rect))
        .collect()
    }

    fn order(l: &Layout<u32>) -> Vec<u32> {
        l.leaves().into_iter().copied().collect()
    }

    /// The pane menu's "Linked DOM" check says what the toggle would do.
    #[test]
    fn kind_shown_matches_what_the_toggle_acts_on() {
        let (l, mut leaves) = pair();
        assert_eq!(kind_shown(&leaves, 1, PaneKind::Dom), Some(true));
        let changes = toggle_kind(&order(&l), &leaves, 1, PaneKind::Dom).unwrap();
        apply(&mut leaves, changes);
        assert_eq!(kind_shown(&leaves, 1, PaneKind::Dom), Some(false));
        assert_eq!(
            kind_shown(&leaves, 4, PaneKind::Dom),
            Some(true),
            "group B's DOM is still shown"
        );
        assert_eq!(
            kind_shown(&leaves, 1, PaneKind::Journal),
            None,
            "none to toggle"
        );
    }

    #[test]
    fn hide_reflows_and_show_restores_the_exact_geometry() {
        let (l, mut leaves) = pair();
        let o = order(&l);
        let before = geometry(&l, &leaves);
        // Hide NQ's DOM: its tape takes the whole column; nothing else moves.
        let c = toggle_kind(&o, &leaves, 2, PaneKind::Dom).unwrap();
        apply(&mut leaves, c);
        let hidden = geometry(&l, &leaves);
        assert!(hidden.iter().all(|(id, _)| *id != 2));
        let tape = hidden.iter().find(|(id, _)| *id == 3).unwrap().1;
        let col = before.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!((tape.y, tape.h), (col.y, col.h + before[2].1.h));
        assert_eq!(hidden[0], before[0], "the chart keeps its size");
        // Hide the tape too: the chart takes NQ's whole half.
        let c = hide(&o, &leaves, 3).unwrap();
        apply(&mut leaves, c);
        let chart = geometry(&l, &leaves)[0].1;
        assert_eq!(chart, Rect::new(0.0, 0.0, 800.0, 900.0));
        // Show both again (toggle from the chart: hidden → shown): exactly as before.
        let c = toggle_kind(&o, &leaves, 1, PaneKind::Dom).unwrap();
        apply(&mut leaves, c);
        let c = toggle_kind(&o, &leaves, 1, PaneKind::Tape).unwrap();
        apply(&mut leaves, c);
        assert_eq!(geometry(&l, &leaves), before);
        // A dragged ratio survives hiding (the split under a hidden side keeps it).
        let mut l = l;
        let d = l.dividers(Rect::new(0.0, 0.0, 1600.0, 900.0));
        assert!(l.set_ratio(&d[1].path, 0.8));
        let dragged = geometry(&l, &leaves);
        let c = hide(&o, &leaves, 2).unwrap();
        apply(&mut leaves, c);
        assert_ne!(geometry(&l, &leaves), dragged);
        apply(&mut leaves, vec![(2, PaneState::Shown)]);
        assert_eq!(geometry(&l, &leaves), dragged);
    }

    #[test]
    fn toggling_from_a_chart_acts_on_its_own_group_only() {
        let (l, mut leaves) = pair();
        let o = order(&l);
        let c = toggle_kind(&o, &leaves, 4, PaneKind::Dom).unwrap();
        apply(&mut leaves, c);
        assert_eq!(leaves[&5].state, PaneState::Hidden, "ES's DOM");
        assert_eq!(leaves[&2].state, PaneState::Shown, "NQ's untouched");
        assert_eq!(leaves[&6].state, PaneState::Shown, "tape is independent");
        // Hidden panes keep their link (and everything else).
        assert_eq!(leaves[&5].link, Some(1));
        // A chart can't be hidden; the last shown pane stays.
        assert!(hide(&o, &leaves, 1).is_err());
        let mut solo = HashMap::new();
        solo.insert(
            1u32,
            Leaf {
                kind: PaneKind::Dom,
                link: None,
                state: PaneState::Shown,
            },
        );
        assert_eq!(hide(&[1], &solo, 1), Err("the last visible pane stays"));
    }

    /// A journal in the chart's link group isn't one of the chart's panes: "chart only"
    /// leaves it shown.
    #[test]
    fn chart_only_leaves_a_linked_journal_alone() {
        let mut leaves = HashMap::new();
        let leaf = |kind| Leaf {
            kind,
            link: Some(0),
            state: PaneState::Shown,
        };
        leaves.insert(1u32, leaf(PaneKind::Chart));
        leaves.insert(2, leaf(PaneKind::Dom));
        leaves.insert(3, leaf(PaneKind::Journal));
        let c = chart_only(&[1, 2, 3], &leaves, 1).unwrap();
        assert_eq!(c, vec![(2, PaneState::ChartOnly)]);
    }

    #[test]
    fn chart_only_on_and_off_restores_only_what_it_hid() {
        let (l, mut leaves) = pair();
        let o = order(&l);
        let before = geometry(&l, &leaves);
        // The trader hid ES's tape first.
        let c = hide(&o, &leaves, 6).unwrap();
        apply(&mut leaves, c);
        assert!(!is_chart_only(&leaves, 4));
        let c = chart_only(&o, &leaves, 4).unwrap();
        apply(&mut leaves, c);
        assert!(is_chart_only(&leaves, 4));
        assert_eq!(leaves[&5].state, PaneState::ChartOnly);
        let g = geometry(&l, &leaves);
        assert_eq!(
            g.iter().find(|(id, _)| *id == 4).unwrap().1,
            Rect::new(800.0, 0.0, 800.0, 900.0),
            "ES chart fills its half"
        );
        assert_eq!(leaves[&2].state, PaneState::Shown, "NQ untouched");
        // Off: the DOM comes back, the tape the trader hid stays hidden.
        let c = chart_only(&o, &leaves, 4).unwrap();
        apply(&mut leaves, c);
        assert_eq!(leaves[&5].state, PaneState::Shown);
        assert_eq!(leaves[&6].state, PaneState::Hidden);
        apply(&mut leaves, vec![(6, PaneState::Shown)]);
        assert_eq!(geometry(&l, &leaves), before);
        // Nothing to hide: the refusal says why, accurately.
        apply(
            &mut leaves,
            vec![(5, PaneState::Detached), (6, PaneState::Detached)],
        );
        assert_eq!(
            chart_only(&o, &leaves, 4).unwrap_err(),
            "chart only: the linked DOM and time & sales are popped out (left as they are)"
        );
        apply(&mut leaves, vec![(5, PaneState::Hidden)]);
        assert!(
            chart_only(&o, &leaves, 4)
                .unwrap_err()
                .contains("already hidden")
        );
        leaves.get_mut(&4).unwrap().link = Some(3);
        assert_eq!(
            chart_only(&o, &leaves, 4).unwrap_err(),
            "chart only: no DOM or time & sales in link group D in this tab"
        );
        // Only on linked charts.
        leaves.get_mut(&1).unwrap().link = None;
        assert!(chart_only(&o, &leaves, 1).is_err());
        assert!(chart_only(&o, &leaves, 2).is_err(), "not a chart");
    }

    #[test]
    fn detach_and_reattach_round_trip() {
        let (l, mut leaves) = pair();
        let o = order(&l);
        let before = geometry(&l, &leaves);
        can_detach(&o, &leaves, 5).unwrap();
        apply(&mut leaves, vec![(5, PaneState::Detached)]);
        assert!(geometry(&l, &leaves).iter().all(|(id, _)| *id != 5));
        // Detached panes aren't toggled by the DOM hotkey.
        assert!(toggle_kind(&o, &leaves, 4, PaneKind::Dom).is_err());
        apply(&mut leaves, vec![(5, PaneState::Shown)]);
        assert_eq!(geometry(&l, &leaves), before, "back in its previous spot");
    }

    #[test]
    fn docking_beside_a_chart_moves_the_leaf() {
        // What `workspace::dock_beside_chart` does: take the leaf out (the rest closes up),
        // then split the chart's leaf, the chart keeping 72%.
        let (l, leaves) = pair();
        let l = l.remove_leaf(&2).unwrap().unwrap();
        let l = l.split_leaf_at(&4, Axis::Row, 2, 0.72).unwrap();
        let g = geometry(&l, &leaves);
        let chart = g.iter().find(|(id, _)| *id == 4).unwrap().1;
        let dom = g.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(dom.x, chart.right());
        assert_eq!((dom.y, dom.h), (chart.y, chart.h));
        assert_eq!(chart.w, (800.0f32 * 0.68 * 0.72).round());
        // NQ's tape closed up where the DOM was.
        let tape = g.iter().find(|(id, _)| *id == 3).unwrap().1;
        assert_eq!((tape.y, tape.h), (0.0, 900.0));
    }

    #[test]
    fn focus_moves_off_a_hidden_pane() {
        let (l, mut leaves) = pair();
        let o = order(&l);
        let c = hide(&o, &leaves, 2).unwrap();
        apply(&mut leaves, c);
        assert_eq!(refocus(&o, &leaves, 2), Some(3));
        apply(&mut leaves, vec![(3, PaneState::Hidden)]);
        assert_eq!(refocus(&o, &leaves, 3), Some(4));
        assert_eq!(refocus(&o, &leaves, 1), Some(1), "still shown");
        let g = Preset::Chart.build(|k, l| (k, l));
        assert_eq!(g.leaf_count(), 1);
    }

    /// Close tab `i` of `tabs`, as the workspace does.
    fn close(
        tabs: &mut Vec<&'static str>,
        active: &mut usize,
        closed: &mut ClosedTabs<&'static str>,
        windows: usize,
        i: usize,
    ) -> CloseTab {
        closed.push(tabs[i], i);
        let plan = close_tab(tabs.len(), windows, *active, i);
        match plan {
            CloseTab::Remove { active: a } => {
                tabs.remove(i);
                *active = a;
            }
            CloseTab::CloseWindow => tabs.clear(),
            CloseTab::ReplaceWithFresh => {
                tabs.clear();
                tabs.push("fresh");
                *active = 0;
            }
        }
        plan
    }

    fn reopen(tabs: &mut Vec<&'static str>, closed: &mut ClosedTabs<&'static str>) -> usize {
        let (t, i) = closed.pop(tabs.len()).unwrap();
        tabs.insert(i, t);
        i
    }

    #[test]
    fn close_then_reopen_restores_tabs_and_order() {
        let mut tabs = vec!["a", "b", "c", "d"];
        let mut active = 1;
        let mut closed = ClosedTabs::new(10);
        close(&mut tabs, &mut active, &mut closed, 1, 1);
        assert_eq!(
            (tabs.clone(), active),
            (vec!["a", "c", "d"], 1),
            "c slides in"
        );
        close(&mut tabs, &mut active, &mut closed, 1, 2);
        assert_eq!((tabs.clone(), active), (vec!["a", "c"], 1));
        close(&mut tabs, &mut active, &mut closed, 1, 1);
        assert_eq!(
            (tabs.clone(), active),
            (vec!["a"], 0),
            "past the end: the left one"
        );
        // Newest first, each back at its index: the original order.
        assert_eq!(reopen(&mut tabs, &mut closed), 1);
        assert_eq!(reopen(&mut tabs, &mut closed), 2);
        assert_eq!(reopen(&mut tabs, &mut closed), 1);
        assert_eq!(tabs, vec!["a", "b", "c", "d"]);
        assert!(closed.is_empty());
        // Closing a tab left of the active one keeps the same tab active.
        let mut active = 3;
        close(&mut tabs, &mut active, &mut closed, 1, 0);
        assert_eq!(tabs[active], "d");
    }

    #[test]
    fn closing_the_last_tab_of_a_window() {
        let mut closed = ClosedTabs::new(10);
        // A secondary window: it closes, and the tab can still be reopened (elsewhere).
        let (mut tabs, mut active) = (vec!["x"], 0);
        assert_eq!(
            close(&mut tabs, &mut active, &mut closed, 2, 0),
            CloseTab::CloseWindow
        );
        assert!(tabs.is_empty());
        let mut main = vec!["a"];
        reopen(&mut main, &mut closed);
        assert_eq!(main, vec!["x", "a"], "back at index 0");
        // The last tab overall: replaced by a fresh one, never nothing.
        let (mut tabs, mut active) = (vec!["only"], 0);
        assert_eq!(
            close(&mut tabs, &mut active, &mut closed, 1, 0),
            CloseTab::ReplaceWithFresh
        );
        assert_eq!(tabs, vec!["fresh"]);
        assert_eq!(reopen(&mut tabs, &mut closed), 0);
        assert_eq!(tabs, vec!["only", "fresh"]);
    }

    #[test]
    fn the_tab_bar_appears_with_a_second_tab() {
        assert!(!shows_tab_bar(1));
        assert!(shows_tab_bar(2));
        // The last tab overall is replaced, never removed: one tab, no bar.
        assert_eq!(close_tab(1, 1, 0, 0), CloseTab::ReplaceWithFresh);
        assert!(!shows_tab_bar(1));
    }

    #[test]
    fn the_switch_is_offered_once_when_every_pane_needs_it() {
        assert_eq!(switch_offer(&[false, false]), Offer::None);
        // A 2×2 of ES on an NQ-only source: once, for the tab.
        assert_eq!(switch_offer(&[true; 4]), Offer::Tab);
        // Mixed: some panes are served, so each unavailable one offers it.
        assert_eq!(switch_offer(&[true, false, true]), Offer::PerPane);
        // A single pane: its own button.
        assert_eq!(switch_offer(&[true]), Offer::PerPane);
        assert_eq!(switch_offer(&[]), Offer::None);
    }

    #[test]
    fn the_closed_stack_is_bounded() {
        let mut closed = ClosedTabs::new(10);
        for (i, t) in ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"]
            .into_iter()
            .enumerate()
        {
            closed.push(t, i);
        }
        let mut got = Vec::new();
        while let Some((t, _)) = closed.pop(0) {
            got.push(t);
        }
        assert_eq!(got.len(), 10);
        assert_eq!((got[0], got[9]), ("11", "2"), "the oldest two forgotten");
    }
}
