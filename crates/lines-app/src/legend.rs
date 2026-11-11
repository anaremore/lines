//! The chart legend's hover controls (decision 0039's scope in their notices): which row is
//! under the pointer, and the Undo a removal offers in the footer. Pure, so the rules are
//! tested without a window.
//!
//! - A row's controls (hide or show, settings, remove) are built only for the row under the
//!   pointer, found from the rows' bounds as last laid out ([`hit`]): no per-frame chrome for
//!   the others.
//! - A removal offers Undo in the footer for `notice::NOTICE_TTL`; the notice names the scope
//!   ([`removed_text`]): a symbol's set changes every chart of that symbol.
//! - Undo restores the studies exactly as they were, and only if nothing changed them since
//!   ([`Undo::restore`]).

use std::time::Instant;

use crate::study_menu::{StudyRef, StudyScope, StudySet};

/// A legend row's bounds (window px): left, top, width, height.
pub type RowBox = (f32, f32, f32, f32);

/// The row under (`x`, `y`), if any (the first that holds it: rows never overlap).
pub fn hit(rows: &[(StudyRef, RowBox)], x: f32, y: f32) -> Option<&StudyRef> {
    rows.iter()
        .find(|(_, (l, t, w, h))| x >= *l && x < l + w && y >= *t && y < t + h)
        .map(|(r, _)| r)
}

/// The footer's words for a removal: "Removed VWAP from ES charts", or "… from this chart"
/// for separate studies.
pub fn removed_text(name: &str, scope: &StudyScope) -> String {
    match scope {
        StudyScope::Symbol(s) => format!("Removed {name} from {s} charts"),
        StudyScope::Chart(_) => format!("Removed {name} from this chart"),
    }
}

/// A study named on the command line (screenshots: `--legend-hover`, `--context-menu
/// study:SPEC`): `vwap:N`, `volume:N`, `delta:N`, `external:N` (its index in the chart's
/// list), `profile`, `footprint` or `levels:ID`.
pub fn study_spec(s: &str) -> Result<StudyRef, String> {
    let n = |v: &str| {
        v.parse::<usize>()
            .map_err(|_| format!("{s}: expected an index after the kind"))
    };
    Ok(match s.split_once(':') {
        Some(("vwap", i)) => StudyRef::Vwap(n(i)?),
        Some(("volume", i)) => StudyRef::Volume(n(i)?),
        Some(("delta", i)) => StudyRef::Delta(n(i)?),
        Some(("external", i)) => StudyRef::External(n(i)?),
        Some(("levels", id)) if !id.is_empty() => StudyRef::Levels(id.to_owned()),
        None if s == "profile" => StudyRef::Profile,
        None if s == "footprint" => StudyRef::Footprint,
        _ => {
            return Err(format!(
                "{s}: expected vwap:N, volume:N, delta:N, external:N, profile, footprint or levels:ID"
            ));
        }
    })
}

/// [`study_spec`] for the command line: the spec itself once it names a study.
pub fn study_spec_arg(s: &str) -> Result<String, String> {
    study_spec(s).map(|_| s.to_owned())
}

/// A removal that can be undone: the studies before it and right after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Undo {
    pub text: String,
    pub before: StudySet,
    pub after: StudySet,
    /// When the footer drops it.
    pub until: Instant,
    /// The product whose shared set it was removed from (decision 0039); `None`: a chart's
    /// own studies. Undo goes to that set if the chart is gone or shows another since.
    pub product: Option<u32>,
    /// The product of the chart it was removed on.
    pub on: u32,
}

impl Undo {
    /// The set to restore, given the studies now: `before` exactly, only while they are still
    /// what the removal left (a later change, here or on another chart of the symbol, is
    /// never thrown away).
    pub fn restore(&self, now: &StudySet) -> Option<&StudySet> {
        (*now == self.after).then_some(&self.before)
    }

    /// Still offered at `now`.
    pub fn live(&self, now: Instant) -> bool {
        now < self.until
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::study_menu::StudyAction;
    use lines_indicators::{IndicatorConfig, Reset};

    fn set() -> StudySet {
        let mut s = StudySet::default();
        s.apply(&StudyAction::AddVwap(Reset::Eth));
        s.apply(&StudyAction::AddVwap(Reset::Rth));
        s.apply(&StudyAction::AddVolume);
        s
    }

    /// Each row answers only inside its own box, and none between or outside them.
    #[test]
    fn the_row_under_the_pointer() {
        let rows = vec![
            (StudyRef::Vwap(0), (8.0, 30.0, 120.0, 16.0)),
            (StudyRef::Vwap(1), (8.0, 50.0, 140.0, 16.0)),
            (StudyRef::Levels("rs".into()), (8.0, 80.0, 200.0, 16.0)),
        ];
        assert_eq!(hit(&rows, 10.0, 31.0), Some(&StudyRef::Vwap(0)));
        assert_eq!(hit(&rows, 127.9, 45.9), Some(&StudyRef::Vwap(0)));
        assert_eq!(hit(&rows, 128.0, 31.0), None, "right of a short row");
        assert_eq!(hit(&rows, 140.0, 51.0), Some(&StudyRef::Vwap(1)));
        assert_eq!(hit(&rows, 20.0, 47.0), None, "the gap between rows");
        assert_eq!(hit(&rows, 20.0, 90.0), Some(&StudyRef::Levels("rs".into())));
        assert_eq!(hit(&rows, 7.9, 31.0), None);
        assert_eq!(hit(&[], 10.0, 31.0), None);
    }

    #[test]
    fn study_specs() {
        assert_eq!(study_spec("vwap:1"), Ok(StudyRef::Vwap(1)));
        assert_eq!(study_spec("volume:0"), Ok(StudyRef::Volume(0)));
        assert_eq!(study_spec("profile"), Ok(StudyRef::Profile));
        assert_eq!(
            study_spec("levels:demo"),
            Ok(StudyRef::Levels("demo".into()))
        );
        assert!(study_spec("vwap").is_err());
        assert!(study_spec("vwap:x").is_err());
        assert!(study_spec("levels:").is_err());
        assert!(study_spec("nope").is_err());
    }

    #[test]
    fn the_notice_names_the_scope() {
        assert_eq!(
            removed_text("VWAP", &StudyScope::Symbol("ES".into())),
            "Removed VWAP from ES charts"
        );
        assert_eq!(
            removed_text("Volume profile", &StudyScope::Chart("ES".into())),
            "Removed Volume profile from this chart"
        );
    }

    /// Undo gives back exactly the set before the removal (the study at its place, with its
    /// style), and nothing once the studies changed again.
    #[test]
    fn undo_restores_exactly_and_never_over_a_later_change() {
        let before = set();
        let mut after = before.clone();
        after.apply(&StudyAction::Remove(StudyRef::Vwap(0)));
        let u = Undo {
            text: "Removed VWAP from ES charts".into(),
            before: before.clone(),
            after: after.clone(),
            until: Instant::now() + crate::notice::NOTICE_TTL,
            product: Some(1),
            on: 1,
        };
        assert_eq!(u.restore(&after), Some(&before));
        let mut later = after.clone();
        later.apply(&StudyAction::AddVwap(Reset::Weekly));
        assert_eq!(u.restore(&later), None);
        assert!(u.live(Instant::now()));
        assert!(!u.live(u.until));
    }

    /// Hiding keeps the study's whole configuration: shown again, it is exactly as it was.
    #[test]
    fn hide_keeps_the_configuration() {
        let mut s = set();
        s.apply(&StudyAction::ToggleBand(1, 2));
        let shown = s.clone();
        s.apply(&StudyAction::ToggleHidden(StudyRef::Vwap(1)));
        assert!(s.is_hidden(&StudyRef::Vwap(1)));
        assert!(!s.is_hidden(&StudyRef::Vwap(0)));
        assert_eq!(s.indicators.len(), 3, "still listed");
        s.apply(&StudyAction::ToggleHidden(StudyRef::Vwap(1)));
        assert_eq!(s, shown);
    }

    /// A move skips studies drawn elsewhere (a lower pane's, which the legend doesn't list
    /// in between), and a reference to another kind moves nothing.
    #[test]
    fn moves_stay_among_the_studies_drawn_there() {
        let mut s = StudySet::default();
        s.apply(&StudyAction::AddVwap(Reset::Eth));
        s.apply(&StudyAction::AddDelta(lines_indicators::DeltaReset::Eth));
        s.apply(&StudyAction::AddVwap(Reset::Rth));
        assert!(s.can_move(&StudyRef::Vwap(2), true));
        assert!(
            !s.can_move(&StudyRef::Delta(1), true),
            "titled in its pane, not listed"
        );
        assert!(
            !s.can_move(&StudyRef::Vwap(1), true),
            "index 1 is the delta"
        );
        let before = s.clone();
        s.apply(&StudyAction::Move(StudyRef::Vwap(1), true));
        assert_eq!(s, before);
        s.apply(&StudyAction::Move(StudyRef::Vwap(2), true));
        // Before ETH's VWAP; the delta in between keeps its order among the lower panes.
        assert!(
            matches!(&s.indicators[0], IndicatorConfig::Vwap { spec, .. } if spec.reset == Reset::Rth)
        );
        assert!(
            matches!(&s.indicators[1], IndicatorConfig::Vwap { spec, .. } if spec.reset == Reset::Eth)
        );
        assert!(matches!(s.indicators[2], IndicatorConfig::CumDelta { .. }));
        // Back down: the legend's order as it was, the delta still last among the lower panes.
        s.apply(&StudyAction::Move(StudyRef::Vwap(0), false));
        assert!(
            matches!(&s.indicators[0], IndicatorConfig::Vwap { spec, .. } if spec.reset == Reset::Eth)
        );
        assert!(
            matches!(&s.indicators[1], IndicatorConfig::Vwap { spec, .. } if spec.reset == Reset::Rth)
        );
        assert!(matches!(s.indicators[2], IndicatorConfig::CumDelta { .. }));
        // A row whose study leaves the legend (a hidden delta shown again) is no longer one.
        s.apply(&StudyAction::ToggleHidden(StudyRef::Delta(2)));
        assert!(s.lists(&StudyRef::Delta(2)));
        s.apply(&StudyAction::ToggleHidden(StudyRef::Delta(2)));
        assert!(!s.lists(&StudyRef::Delta(2)) && s.names(&StudyRef::Delta(2)));
    }

    /// Move up and down swap neighbours in the drawing order; the ends don't move.
    #[test]
    fn move_up_and_down() {
        let mut s = set();
        let names = |s: &StudySet| -> Vec<&'static str> {
            s.indicators
                .iter()
                .map(|c| match c {
                    IndicatorConfig::Vwap { spec, .. } if spec.reset == Reset::Eth => "eth",
                    IndicatorConfig::Vwap { .. } => "rth",
                    _ => "vol",
                })
                .collect()
        };
        assert!(!s.can_move(&StudyRef::Vwap(0), true));
        assert!(s.can_move(&StudyRef::Vwap(0), false));
        assert!(!s.can_move(&StudyRef::Volume(2), false));
        assert!(!s.can_move(&StudyRef::Profile, true));
        s.apply(&StudyAction::Move(StudyRef::Volume(2), true));
        assert_eq!(names(&s), ["eth", "vol", "rth"]);
        s.apply(&StudyAction::Move(StudyRef::Vwap(0), false));
        assert_eq!(names(&s), ["vol", "eth", "rth"]);
        let ends = s.clone();
        s.apply(&StudyAction::Move(StudyRef::Volume(0), true));
        s.apply(&StudyAction::Move(StudyRef::Vwap(2), false));
        assert_eq!(s, ends);
    }

    /// A copy for another chart is shown there, never doubles a study it has (a hidden one
    /// is shown), takes a free VWAP color, and a chart with nothing to add stays unchanged.
    #[test]
    fn add_to_another_chart() {
        let mut src = set();
        src.apply(&StudyAction::ToggleHidden(StudyRef::Vwap(1)));
        let copy = src.copy_of(&StudyRef::Vwap(1)).unwrap();
        let mut other = StudySet::default();
        other.apply(&StudyAction::AddVwap(Reset::Eth));
        assert!(other.add_copy(&copy));
        assert_eq!(other.indicators.len(), 2);
        assert!(!other.is_hidden(&StudyRef::Vwap(1)));
        let colors: Vec<u8> = other
            .indicators
            .iter()
            .filter_map(|c| match c {
                IndicatorConfig::Vwap { color, .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert_ne!(colors[0], colors[1]);
        assert!(!other.add_copy(&copy), "already there");
        other.apply(&StudyAction::ToggleHidden(StudyRef::Vwap(1)));
        assert!(other.add_copy(&copy), "hidden: shown");
        assert!(!other.is_hidden(&StudyRef::Vwap(1)));
        // One Volume per chart.
        let vol = src.copy_of(&StudyRef::Volume(2)).unwrap();
        assert!(other.add_copy(&vol));
        assert!(!other.add_copy(&vol));
        assert_eq!(src.copy_of(&StudyRef::Profile), None, "not on the chart");
    }
}
