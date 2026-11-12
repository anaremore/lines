//! The studies menu (Ctrl+I, the chart's "Studies…", the palette): one place for everything
//! drawn on a chart: VWAPs, cumulative delta, Volume, the footprint, the volume profile and external
//! level sources. Pure (no GPUI):
//! the chart renders [`StudyMenu::items`] and feeds keys and clicks back as [`MenuCmd`]s.
//!
//! - **On this chart:** each study with a hide toggle (eye) and an editor (gear). The editor
//!   is a flat list of controls (reset, bands, colour, width, label; profile mode, session,
//!   split, grouping; level kinds and labels), each one a command, ending with "Style…": the
//!   shared style editor on the study's parts (`crate::study_style`, decision 0026).
//! - **Add:** a catalog filtered by typing, like the symbol picker.
//! - **Templates:** apply or delete saved study sets; save the chart's set as a template or as
//!   the default for new charts (decision 0021: scope is explicit).
//!
//! Every change is a [`StudyAction`] applied to the chart's [`StudySet`] by
//! [`StudySet::apply`] (pure), so menus, keys and tests share one path.

use lines_core::{Bar, Viewport};
use lines_indicators::{
    Bands, DeltaReset, DeltaSpec, IndicatorConfig, Reset, VolumeSpec, VwapSpec,
};
use serde::{Deserialize, Serialize};

use crate::footprint::{FootprintConfig, FootprintMode};

use crate::levels_layer::LevelsConfig;
use crate::profile::{ProfileConfig, ProfileMode, ProfileSession};
use crate::study_style::{self, Look, StudyStyles};

/// Every study on a chart: what a template or "default for new charts" holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudySet {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub indicators: Vec<IndicatorConfig>,
    #[serde(skip_serializing_if = "ProfileConfig::is_default")]
    pub profile: ProfileConfig,
    #[serde(skip_serializing_if = "LevelsConfig::is_default")]
    pub levels: LevelsConfig,
    #[serde(skip_serializing_if = "FootprintConfig::is_default")]
    pub footprint: FootprintConfig,
    /// User styles of the profile, the footprint and level sources (decision 0026; VWAPs and
    /// cumulative delta carry theirs in `indicators`). Optional.
    #[serde(skip_serializing_if = "StudyStyles::is_empty")]
    pub styles: StudyStyles,
    /// New charts start with their trade history off (Settings › Charts, "Save as default for
    /// new charts"). Only in the defaults for new charts, never in a template. Optional.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trades_off: bool,
}

/// A named study set (saved in the app settings).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudyTemplate {
    pub name: String,
    pub set: StudySet,
}

/// A study on the chart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StudyRef {
    Vwap(usize),
    /// Cumulative delta at this index of the indicators.
    Delta(usize),
    /// Volume at this index of the indicators.
    Volume(usize),
    /// An external series study at this index of the indicators.
    External(usize),
    Footprint,
    Profile,
    Levels(String),
}

/// A change to a chart's studies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StudyAction {
    AddVwap(Reset),
    /// Anchor VWAP `index` (or a new one) at a bar's start time (ms).
    AnchorAt {
        index: Option<usize>,
        from: i64,
    },
    AddProfile(ProfileMode),
    AddDelta(DeltaReset),
    /// Add Volume (on the chart: `VolumeSpec::added`); one per chart.
    AddVolume,
    /// Volume on the chart (`true`) or in its own pane: the chart's Volume moves, or one is
    /// added there (the command "Volume: on the chart / in a pane").
    PlaceVolume(bool),
    /// Add a level source's lower-pane series study (source id, study id); once per chart.
    AddExternal(String, String),
    /// Set Volume `index`'s options.
    SetVolume(usize, VolumeSpec),
    SetDeltaReset(usize, DeltaReset),
    AddFootprint(FootprintMode),
    SetFootprintMode(FootprintMode),
    SetImbalanceRatio(u32),
    SetImbalanceMin(i64),
    SetFootprintGrouping(Option<u32>),
    ToggleHidden(StudyRef),
    Remove(StudyRef),
    SetReset(usize, Reset),
    ToggleBand(usize, u8),
    SetColor(usize, u8),
    SetWidth(usize, u8),
    ToggleLabel(usize),
    SetProfileMode(ProfileMode),
    SetProfileSession(ProfileSession),
    ToggleProfileSplit,
    SetProfileGrouping(Option<u32>),
    ToggleLevelKind(String, String),
    /// Show a kind its parent hid, on its own (source, kind: `LevelsConfig::show_under`).
    ShowLevelKindUnder(String, String),
    ToggleLevelLabels(String),
    /// Where the chart's study labels go.
    SetLabelPlace(crate::levels_layer::LabelPlace),
    /// One part of the volume profile's label, on or off.
    ToggleProfileLabel(crate::profile::ProfileLabelPart),
    /// Restyle a study: every part's look, in its parts' order (`crate::study_style`).
    SetStyle(StudyRef, Vec<Look>),
    /// Replace every study with a template's (boxed: a study set is large).
    Replace(Box<StudySet>),
    /// Draw a study one place earlier (`true`) or later in the chart's list (the legend's
    /// "Move up / down"): only studies in the indicator list move; the others keep their place.
    Move(StudyRef, bool),
}

/// One study taken from a chart to add to others ("Add to every chart in this tab"), shown.
#[derive(Clone, Debug, PartialEq)]
pub enum StudyCopy {
    Indicator(IndicatorConfig),
    Profile(ProfileConfig),
    Footprint(FootprintConfig),
    /// A level source shown (it serves the charts it serves; nothing else to copy).
    Levels(String),
}

fn next_color(set: &StudySet) -> u8 {
    let used: Vec<u8> = set
        .indicators
        .iter()
        .filter_map(|c| match c {
            IndicatorConfig::Vwap { color, .. } => Some(*color),
            IndicatorConfig::CumDelta { .. }
            | IndicatorConfig::Volume { .. }
            | IndicatorConfig::External { .. } => None,
        })
        .collect();
    (0..4u8).find(|c| !used.contains(c)).unwrap_or(0)
}

impl StudySet {
    /// Whether it holds any study (the trade history switch aside): what Settings › Charts'
    /// "Studies" row reports.
    pub fn has_studies(&self) -> bool {
        let studies = StudySet {
            trades_off: false,
            ..self.clone()
        };
        studies != StudySet::default()
    }

    fn vwap(
        &mut self,
        i: usize,
    ) -> Option<(&mut VwapSpec, &mut u8, &mut lines_indicators::Display)> {
        match self.indicators.get_mut(i)? {
            IndicatorConfig::Vwap {
                spec,
                color,
                display,
                ..
            } => Some((spec, color, display)),
            IndicatorConfig::CumDelta { .. }
            | IndicatorConfig::Volume { .. }
            | IndicatorConfig::External { .. } => None,
        }
    }

    fn delta(&mut self, i: usize) -> Option<&mut DeltaSpec> {
        match self.indicators.get_mut(i)? {
            IndicatorConfig::CumDelta { spec, .. } => Some(spec),
            IndicatorConfig::Vwap { .. }
            | IndicatorConfig::Volume { .. }
            | IndicatorConfig::External { .. } => None,
        }
    }

    /// Apply one change. Unknown indices are ignored (the menu was stale).
    pub fn apply(&mut self, a: &StudyAction) {
        match a {
            StudyAction::AddVwap(reset) => {
                let color = next_color(self);
                self.indicators.push(IndicatorConfig::Vwap {
                    spec: VwapSpec {
                        reset: *reset,
                        bands: Bands(0),
                    },
                    color,
                    display: Default::default(),
                    style: Default::default(),
                });
            }
            StudyAction::AnchorAt { index, from } => {
                let reset = Reset::Anchored { from: *from };
                match index {
                    Some(i) => {
                        if let Some((spec, ..)) = self.vwap(*i) {
                            spec.reset = reset;
                        }
                    }
                    None => self.apply(&StudyAction::AddVwap(reset)),
                }
            }
            StudyAction::AddProfile(mode) => {
                self.profile.mode = *mode;
                self.profile.hidden = false;
            }
            StudyAction::AddDelta(reset) => {
                // One cumulative delta per chart: adding again replaces it.
                self.indicators
                    .retain(|c| !matches!(c, IndicatorConfig::CumDelta { .. }));
                self.indicators.push(IndicatorConfig::CumDelta {
                    spec: DeltaSpec { reset: *reset },
                    display: Default::default(),
                    style: Default::default(),
                });
            }
            StudyAction::AddVolume => {
                // One Volume per chart: adding again keeps the one there.
                if !self
                    .indicators
                    .iter()
                    .any(|c| matches!(c, IndicatorConfig::Volume { .. }))
                {
                    self.indicators.push(IndicatorConfig::Volume {
                        spec: VolumeSpec::added(),
                        display: Default::default(),
                        style: Default::default(),
                    });
                }
            }
            StudyAction::PlaceVolume(overlay) => {
                let at = self.indicators.iter_mut().find_map(|c| match c {
                    IndicatorConfig::Volume { spec, .. } => Some(spec),
                    _ => None,
                });
                match at {
                    Some(spec) => spec.overlay = *overlay,
                    None => self.indicators.push(IndicatorConfig::Volume {
                        spec: VolumeSpec {
                            overlay: *overlay,
                            ..VolumeSpec::added()
                        },
                        display: Default::default(),
                        style: Default::default(),
                    }),
                }
            }
            StudyAction::AddExternal(source, study) => {
                let spec = lines_indicators::ExternalSpec {
                    source: source.clone(),
                    study: study.clone(),
                };
                if !self
                    .indicators
                    .iter()
                    .any(|c| matches!(c, IndicatorConfig::External { spec: s, .. } if *s == spec))
                {
                    self.indicators.push(IndicatorConfig::External {
                        spec,
                        display: Default::default(),
                        style: Default::default(),
                    });
                }
            }
            StudyAction::SetVolume(i, v) => {
                if let Some(IndicatorConfig::Volume { spec, .. }) = self.indicators.get_mut(*i) {
                    *spec = *v;
                    spec.average_len = spec.average_len.clamp(1, VolumeSpec::MAX_LEN);
                }
            }
            StudyAction::SetDeltaReset(i, reset) => {
                if let Some(spec) = self.delta(*i) {
                    spec.reset = *reset;
                }
            }
            StudyAction::AddFootprint(mode) | StudyAction::SetFootprintMode(mode) => {
                self.footprint.mode = *mode;
                self.footprint.hidden = false;
            }
            StudyAction::SetImbalanceRatio(r) => self.footprint.imbalance_pct = (*r).max(100),
            StudyAction::SetImbalanceMin(m) => self.footprint.imbalance_min = (*m).max(1),
            StudyAction::SetFootprintGrouping(g) => self.footprint.grouping = *g,
            StudyAction::ToggleHidden(r) => match r {
                StudyRef::Vwap(i)
                | StudyRef::Delta(i)
                | StudyRef::Volume(i)
                | StudyRef::External(i) => {
                    if let Some(c) = self.indicators.get_mut(*i) {
                        let d = c.display_mut();
                        d.hidden = !d.hidden;
                    }
                }
                StudyRef::Footprint => self.footprint.hidden = !self.footprint.hidden,
                StudyRef::Profile => self.profile.hidden = !self.profile.hidden,
                StudyRef::Levels(id) => self.levels.toggle_source(id),
            },
            StudyAction::Remove(r) => match r {
                StudyRef::Vwap(i)
                | StudyRef::Delta(i)
                | StudyRef::Volume(i)
                | StudyRef::External(i) => {
                    if *i < self.indicators.len() {
                        self.indicators.remove(*i);
                    }
                }
                StudyRef::Footprint => {
                    self.footprint.mode = FootprintMode::Off;
                    self.footprint.hidden = false;
                    self.styles.footprint.clear();
                }
                StudyRef::Profile => {
                    self.profile.mode = ProfileMode::Off;
                    self.profile.hidden = false;
                    self.styles.profile.clear();
                }
                // A registered source can't be removed from a chart, only hidden.
                StudyRef::Levels(id) => {
                    if self.levels.source_shown(id) {
                        self.levels.toggle_source(id);
                    }
                }
            },
            StudyAction::SetReset(i, reset) => {
                if let Some((spec, ..)) = self.vwap(*i) {
                    spec.reset = *reset;
                }
            }
            StudyAction::ToggleBand(i, k) => {
                if let Some((spec, ..)) = self.vwap(*i)
                    && (1..=3).contains(k)
                {
                    spec.bands = Bands(spec.bands.0 ^ (1 << (k - 1)));
                }
            }
            StudyAction::SetColor(i, c) => {
                if let Some(cfg @ IndicatorConfig::Vwap { .. }) = self.indicators.get_mut(*i) {
                    // A theme color replaces a custom one (the style editor's line color).
                    let mut looks = study_style::vwap_looks(cfg);
                    if let crate::style_editor::PartStyle::Line(l) =
                        &mut looks[study_style::VWAP_LINE].style
                    {
                        l.color = lines_core::style::Color::Palette(*c % 4);
                    }
                    study_style::set_vwap(cfg, &looks);
                }
            }
            StudyAction::SetWidth(i, w) => {
                if let Some((.., d)) = self.vwap(*i) {
                    d.width = (*w).clamp(1, 4);
                }
            }
            StudyAction::ToggleLabel(i) => {
                if let Some(c) = self.indicators.get_mut(*i) {
                    let d = c.display_mut();
                    d.label = !d.label;
                }
            }
            StudyAction::SetProfileMode(m) => self.profile.mode = *m,
            StudyAction::SetProfileSession(s) => self.profile.session = *s,
            StudyAction::ToggleProfileSplit => self.profile.split = !self.profile.split,
            StudyAction::SetProfileGrouping(g) => self.profile.grouping = *g,
            StudyAction::ToggleLevelKind(id, kind) => self.levels.toggle_kind(id, kind),
            StudyAction::ShowLevelKindUnder(id, kind) => self.levels.show_under(id, kind),
            StudyAction::ToggleLevelLabels(id) => self.levels.toggle_labels(id),
            StudyAction::SetLabelPlace(p) => self.levels.label_place = *p,
            StudyAction::ToggleProfileLabel(p) => self.profile.toggle_label_part(*p),
            StudyAction::SetStyle(r, looks) => match r {
                StudyRef::Vwap(i) => {
                    if let Some(c @ IndicatorConfig::Vwap { .. }) = self.indicators.get_mut(*i) {
                        study_style::set_vwap(c, looks);
                    }
                }
                StudyRef::Delta(i) => {
                    if let Some(c @ IndicatorConfig::CumDelta { .. }) = self.indicators.get_mut(*i)
                    {
                        study_style::write(c.style_mut(), &study_style::DELTA, looks);
                    }
                }
                StudyRef::Volume(i) => {
                    if let Some(c @ IndicatorConfig::Volume { .. }) = self.indicators.get_mut(*i) {
                        study_style::write(c.style_mut(), &study_style::VOLUME, looks);
                    }
                }
                StudyRef::External(i) => {
                    if let Some(c @ IndicatorConfig::External { .. }) = self.indicators.get_mut(*i)
                        && let Some(def) = crate::studies::ext_def(c)
                    {
                        let defs = crate::series_pane::part_defs(def);
                        study_style::write(c.style_mut(), defs, looks);
                    }
                }
                StudyRef::Profile => {
                    study_style::write(&mut self.styles.profile, &study_style::PROFILE, looks)
                }
                StudyRef::Footprint => {
                    study_style::write(&mut self.styles.footprint, &study_style::FOOTPRINT, looks)
                }
                StudyRef::Levels(id) => self.styles.set_levels(id, looks),
            },
            StudyAction::Replace(set) => *self = (**set).clone(),
            StudyAction::Move(r, up) => {
                // Taken out and put back at its neighbour's place: the studies in between (drawn
                // in lower panes, not listed) keep their order.
                if let Some((i, j)) = self.neighbour(r, *up) {
                    let c = self.indicators.remove(i);
                    self.indicators.insert(j, c);
                }
            }
        }
    }

    /// Whether study `r` can move one place earlier (`up`) or later (`StudyAction::Move`).
    pub fn can_move(&self, r: &StudyRef, up: bool) -> bool {
        self.neighbour(r, up).is_some()
    }

    /// Study `r`'s index and its neighbour's that way among the studies drawn where it is
    /// (on the chart, or in lower panes): a move swaps them, so the legend's order changes
    /// with every move (`None`: an end, or not an indicator, or `r` names another kind).
    fn neighbour(&self, r: &StudyRef, up: bool) -> Option<(usize, usize)> {
        let i = match r {
            StudyRef::Vwap(i)
            | StudyRef::Delta(i)
            | StudyRef::Volume(i)
            | StudyRef::External(i) => *i,
            StudyRef::Footprint | StudyRef::Profile | StudyRef::Levels(_) => return None,
        };
        let c = self.indicators.get(i)?;
        let kind_ok = matches!(
            (r, c),
            (StudyRef::Vwap(_), IndicatorConfig::Vwap { .. })
                | (StudyRef::Delta(_), IndicatorConfig::CumDelta { .. })
                | (StudyRef::Volume(_), IndicatorConfig::Volume { .. })
                | (StudyRef::External(_), IndicatorConfig::External { .. })
        );
        if !kind_ok {
            return None;
        }
        // Among the rows the legend lists, so every move shows (a study drawn in a lower pane
        // is titled there, not listed, and keeps its place).
        if !legend_lists(c) {
            return None;
        }
        let same = |j: &usize| legend_lists(&self.indicators[*j]);
        let j = if up {
            (0..i).rev().find(same)
        } else {
            (i + 1..self.indicators.len()).find(same)
        }?;
        Some((i, j))
    }

    /// Whether `r` still names a study of its kind here (a legend row's reference after a
    /// change: the same kind at that index, the profile or footprint on, any level source).
    pub fn names(&self, r: &StudyRef) -> bool {
        match r {
            StudyRef::Vwap(i) => {
                matches!(self.indicators.get(*i), Some(IndicatorConfig::Vwap { .. }))
            }
            StudyRef::Delta(i) => {
                matches!(
                    self.indicators.get(*i),
                    Some(IndicatorConfig::CumDelta { .. })
                )
            }
            StudyRef::Volume(i) => {
                matches!(
                    self.indicators.get(*i),
                    Some(IndicatorConfig::Volume { .. })
                )
            }
            StudyRef::External(i) => {
                matches!(
                    self.indicators.get(*i),
                    Some(IndicatorConfig::External { .. })
                )
            }
            StudyRef::Footprint => self.footprint.mode != FootprintMode::Off,
            StudyRef::Profile => self.profile.mode != ProfileMode::Off,
            StudyRef::Levels(_) => true,
        }
    }

    /// Whether `r` still names a study the legend lists (its row is still there).
    pub fn lists(&self, r: &StudyRef) -> bool {
        self.names(r)
            && match r {
                StudyRef::Vwap(i)
                | StudyRef::Delta(i)
                | StudyRef::Volume(i)
                | StudyRef::External(i) => legend_lists(&self.indicators[*i]),
                StudyRef::Footprint | StudyRef::Profile | StudyRef::Levels(_) => true,
            }
    }

    /// Whether study `r` is hidden (`StudyAction::ToggleHidden`).
    pub fn is_hidden(&self, r: &StudyRef) -> bool {
        match r {
            StudyRef::Vwap(i)
            | StudyRef::Delta(i)
            | StudyRef::Volume(i)
            | StudyRef::External(i) => self.indicators.get(*i).is_some_and(|c| c.display().hidden),
            StudyRef::Footprint => self.footprint.hidden,
            StudyRef::Profile => self.profile.hidden,
            StudyRef::Levels(id) => !self.levels.source_shown(id),
        }
    }

    /// Study `r` as a copy for another chart (shown there), if it is on this one.
    pub fn copy_of(&self, r: &StudyRef) -> Option<StudyCopy> {
        Some(match r {
            StudyRef::Vwap(i)
            | StudyRef::Delta(i)
            | StudyRef::Volume(i)
            | StudyRef::External(i) => {
                let mut c = self.indicators.get(*i)?.clone();
                c.display_mut().hidden = false;
                StudyCopy::Indicator(c)
            }
            StudyRef::Footprint => (self.footprint.mode != FootprintMode::Off).then(|| {
                StudyCopy::Footprint(FootprintConfig {
                    hidden: false,
                    ..self.footprint
                })
            })?,
            StudyRef::Profile => (self.profile.mode != ProfileMode::Off).then(|| {
                StudyCopy::Profile(ProfileConfig {
                    hidden: false,
                    ..self.profile
                })
            })?,
            StudyRef::Levels(id) => StudyCopy::Levels(id.clone()),
        })
    }

    /// Add `copy` unless this set already has that study (the same VWAP, delta or series
    /// spec; any Volume, the one per chart): `true` if it changed. A hidden one is shown. A
    /// profile or footprint already on takes the copy's settings.
    pub fn add_copy(&mut self, copy: &StudyCopy) -> bool {
        let before = self.clone();
        match copy {
            StudyCopy::Indicator(c) => {
                let same = |o: &IndicatorConfig| match (o, c) {
                    (
                        IndicatorConfig::Vwap { spec: a, .. },
                        IndicatorConfig::Vwap { spec: b, .. },
                    ) => a == b,
                    (
                        IndicatorConfig::CumDelta { spec: a, .. },
                        IndicatorConfig::CumDelta { spec: b, .. },
                    ) => a == b,
                    (IndicatorConfig::Volume { .. }, IndicatorConfig::Volume { .. }) => true,
                    (
                        IndicatorConfig::External { spec: a, .. },
                        IndicatorConfig::External { spec: b, .. },
                    ) => a == b,
                    _ => false,
                };
                match self.indicators.iter_mut().find(|o| same(o)) {
                    Some(o) => o.display_mut().hidden = false,
                    None => {
                        let mut c = c.clone();
                        // A VWAP takes the next free color here (never two the same).
                        if let IndicatorConfig::Vwap { color, .. } = &mut c
                            && self.indicators.iter().any(|o| matches!(o, IndicatorConfig::Vwap { color: k, .. } if k == color))
                        {
                            *color = next_color(self);
                        }
                        self.indicators.push(c);
                    }
                }
            }
            StudyCopy::Profile(p) => self.profile = *p,
            StudyCopy::Footprint(f) => self.footprint = *f,
            // Only that source: levels turned off as a whole on a chart stay off there.
            StudyCopy::Levels(id) => self.levels.hidden_sources.retain(|s| s != id),
        }
        *self != before
    }
}

/// An indicator the chart's legend lists: drawn on the chart with its label on, or hidden
/// (listed, dimmed, one click from coming back, whatever its label setting; a hidden
/// lower-pane study has no pane title).
pub fn legend_lists(c: &IndicatorConfig) -> bool {
    let d = c.display();
    d.hidden || (d.label && !c.in_subpane())
}

/// A study's name in a sentence ("Removed VWAP from ES charts"): its kind, or a series' or
/// a level source's own name (`named`: from the chart's sources).
pub fn short_name(
    set: &StudySet,
    r: &StudyRef,
    named: &dyn Fn(&IndicatorConfig) -> Option<String>,
) -> String {
    match r {
        StudyRef::Vwap(_) => "VWAP".into(),
        StudyRef::Delta(_) => "Cumulative delta".into(),
        StudyRef::Volume(_) => "Volume".into(),
        StudyRef::External(i) => set
            .indicators
            .get(*i)
            .and_then(named)
            .unwrap_or_else(|| "the series".into()),
        StudyRef::Footprint => "Footprint".into(),
        StudyRef::Profile => "Volume profile".into(),
        StudyRef::Levels(id) => id.clone(),
    }
}

/// A command from the menu: a change to the studies, or a menu or template operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCmd {
    Set(StudyAction),
    /// Open (or close) the inline editor of a study.
    Edit(StudyRef),
    /// Open the style editor on a study's parts ("Style…").
    Style(StudyRef),
    /// Wait for a click on a bar to anchor VWAP `index` (or a new one there).
    BeginAnchor(Option<usize>),
    ApplyTemplate(String),
    DeleteTemplate(String),
    /// Start typing a template name (Enter saves).
    StartSaveTemplate,
    SaveTemplate(String),
    /// Save the chart's studies as the default for new charts.
    SaveDefault,
    /// This chart's own studies (a copy of its symbol's) on or off (decision 0039).
    ToggleSeparate,
    /// A level source's own setting ("Source settings"; app-wide, `crate::plugin_settings`).
    Plugin(PluginCmd),
    Close,
}

/// A change to a plugin's settings from the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginCmd {
    /// Set `key` to `value` (a switch asking first asks: [`StudyMenu::asking`]).
    Set {
        source: String,
        key: &'static str,
        value: String,
    },
    /// Start typing `key`, from `text`.
    Edit {
        source: String,
        key: &'static str,
        text: String,
    },
    /// The question answered: turn the switch on (`true`), or leave it.
    Answer(bool),
    /// Turn the switch on, asked and answered (never asks again).
    Confirmed { source: String, key: &'static str },
    /// Esc on a field: drop its refused value (the value in force shows again).
    Discard { source: String, key: &'static str },
}

/// A plugin setting being typed in the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Typing {
    pub source: String,
    pub key: &'static str,
    pub buf: String,
}

/// A level source on the chart, as the menu shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    pub id: String,
    pub label: String,
    pub kinds: Vec<String>,
    /// The lower-pane series studies it offers: (id, name).
    pub series: Vec<(String, String)>,
    /// Each kind's parent kind (`LevelType::parent`), in `kinds` order (empty: none).
    pub parents: Vec<Option<&'static str>>,
    /// Kinds whose type is turned off in the style editor (Show), in `kinds` order (empty:
    /// none).
    pub styled_off: Vec<bool>,
    /// Its own settings ("Source settings", `crate::plugin_settings`; empty: none). App-wide.
    pub plugin: Vec<crate::plugin_settings::Row>,
}

/// What the menu is built from.
pub struct MenuInput<'a> {
    pub set: &'a StudySet,
    /// Whose studies these are: the symbol's (every chart of it) or this chart's own.
    pub scope: StudyScope,
    pub sources: &'a [SourceInfo],
    pub templates: &'a [String],
    /// The chart's product has RTH (else RTH choices are not offered).
    pub has_rth: bool,
    /// When the product's ETH and RTH sessions open, from its sessions calendar
    /// ([`session_opens`]): "5:00 pm CT". `None`: no calendar, the choice names no time.
    pub opens: SessionOpens,
}

/// Whose studies a chart's menu edits (decision 0039).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StudyScope {
    /// The product's set, shared by every chart of it: its symbol ("ES").
    Symbol(String),
    /// This chart's own (separate studies); its symbol, whose set it can take again.
    Chart(String),
}

impl StudyScope {
    /// The menu's header: "ES charts", or "This chart (separate)".
    pub fn header(&self) -> String {
        match self {
            StudyScope::Symbol(s) => format!("{s} charts"),
            StudyScope::Chart(_) => "This chart (separate)".into(),
        }
    }
}

/// The regular ETH and RTH opens a VWAP's session choice names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionOpens {
    pub eth: Option<String>,
    pub rth: Option<String>,
}

/// The product's session opens, read from its sessions calendar (the exchange's hours, never
/// hard-coded) and named on the chart's clock (`zone`): "5:00 pm CT" for CME's equity futures
/// on a Chicago clock, "6:00 pm ET" on New York's. The open named is the next session's (the
/// one trading at `now`, else the next), so a DST change or a holiday's own open is the
/// calendar's.
/// The time the session choice names its opens for: the feed's clock, else the trading
/// core's (a replay's or a screenshot's), else a replay's own clock or first recorded frame
/// (`replay`); the wall clock (`wall`, `lines_feed::now_utc` in the app) only when none is
/// known (live).
pub fn opens_clock(
    feed: Option<lines_core::UtcMillis>,
    core: Option<lines_core::UtcMillis>,
    replay: Option<Option<lines_core::UtcMillis>>,
    wall: impl FnOnce() -> lines_core::UtcMillis,
) -> lines_core::UtcMillis {
    feed.or(core).or(replay.flatten()).unwrap_or_else(wall)
}

pub fn session_opens(
    cal: Option<&lines_core::sessions::SessionCalendar>,
    zone: &lines_core::display_tz::DisplayTz,
    now: lines_core::UtcMillis,
) -> SessionOpens {
    use lines_core::sessions::SessionKind;
    let Some(cal) = cal else {
        return SessionOpens::default();
    };
    let say = |kind| {
        let open = cal.next_session(now, kind)?.open;
        let (_, _, _, h, m, _) = zone.civil(open);
        let h12 = match h % 12 {
            0 => 12,
            x => x,
        };
        let mut s = format!("{h12}:{m:02} {} ", if h < 12 { "am" } else { "pm" });
        let _ = zone.write_label(open, &mut s);
        Some(s)
    };
    SessionOpens {
        eth: say(SessionKind::Eth),
        rth: if cal.has_rth() {
            say(SessionKind::Rth)
        } else {
            None
        },
    }
}

/// A session choice's label: "ETH (5:00 pm CT open)", or just "ETH" without a calendar.
fn session_choice(name: &str, open: Option<&String>) -> String {
    match open {
        Some(o) => format!("{name} ({o} open)"),
        None => name.to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Swatch {
    /// A theme study colour index.
    Study(u8),
    /// A color the user chose (a VWAP line's custom color).
    Custom(lines_core::style::Rgb),
    /// No swatch.
    None,
}

/// One line of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Header(std::borrow::Cow<'static, str>),
    /// Non-focusable text (scope, naming prompt).
    Note(String),
    /// A study on the chart: the eye and gear act on it.
    Study {
        r: StudyRef,
        label: String,
        swatch: Swatch,
        hidden: bool,
        editing: bool,
    },
    /// An inline editor control: `group` names the setting, `label` the choice.
    Control {
        group: &'static str,
        label: String,
        on: bool,
        swatch: Swatch,
        cmd: MenuCmd,
    },
    /// A setting of several options in one field (owner-approved controls, 2026-11-11): the
    /// value in force (one pick) or a summary (several, `multi`), its options in a floating
    /// list (`crate::drop_list`).
    Drop {
        group: &'static str,
        label: String,
        multi: bool,
        options: Vec<Opt>,
        /// A last option "Custom…" for a typed value.
        custom: Option<CustomTo>,
    },
    /// A number with − / +: its value's text and each step's command (`None` at its end).
    /// `typed`: Enter (or a click on the value) types it (a plugin's numbers); `dim`: the
    /// label is a placeholder; `typing`: being typed (the label is the text so far).
    Step {
        group: &'static str,
        label: String,
        less: Option<MenuCmd>,
        more: Option<MenuCmd>,
        typed: Option<MenuCmd>,
        dim: bool,
        typing: bool,
        /// Its value was refused (the reason is the next item): the destructive edge.
        refused: bool,
    },
    /// A sub-heading inside an editor ("Source settings", a plugin's group).
    Section(String),
    /// A text field (a plugin's text and URL settings): Enter or a click types it.
    Field {
        group: &'static str,
        text: String,
        placeholder: &'static str,
        typing: bool,
        /// Its value was refused (the reason is the next item): the destructive edge.
        refused: bool,
        cmd: MenuCmd,
    },
    /// Why the value above was refused (a plugin's field): its text stays, nothing applied.
    Reason(String),
    /// A quiet line in the editor's value column (a plugin's "For every chart: …", a switch's
    /// question).
    Hint(String),
    /// An "Add" catalog entry or a template action.
    Action {
        label: String,
        cmd: MenuCmd,
        /// Delete key / "delete" affordance (templates).
        delete: Option<MenuCmd>,
    },
}

/// One option of an [`Item::Drop`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opt {
    pub label: String,
    pub on: bool,
    pub cmd: MenuCmd,
}

/// What a typed "Custom…" value sets.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CustomTo {
    ProfileGrouping,
    FootprintGrouping,
}

/// A footprint's imbalance ratio's most (%), and its least volume's most: − / + stop there.
pub const MAX_IMBALANCE_PCT: u32 = 1_000;
pub const MAX_IMBALANCE_MIN: i64 = 10_000;

/// The most ticks per row a typed grouping takes (as a DOM's).
pub const MAX_GROUPING: u32 = 10_000;

impl CustomTo {
    /// The command for typed `text`, if it is a whole number of ticks in range.
    pub fn cmd(self, text: &str) -> Option<MenuCmd> {
        let g = text
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|g| (1..=MAX_GROUPING).contains(g))?;
        Some(MenuCmd::Set(match self {
            CustomTo::ProfileGrouping => StudyAction::SetProfileGrouping(Some(g)),
            CustomTo::FootprintGrouping => StudyAction::SetFootprintGrouping(Some(g)),
        }))
    }
}

/// The open [`Item::Drop`]'s list: the item it belongs to (an index into the menu's items,
/// and its group, checked when the items change), its rows and keys, its scroll.
#[derive(Clone, Debug)]
pub struct MenuDrop {
    pub at: usize,
    pub group: &'static str,
    pub list: crate::drop_list::DropList,
    pub scroll: gpui::ScrollHandle,
}

impl PartialEq for MenuDrop {
    fn eq(&self, o: &MenuDrop) -> bool {
        (self.at, self.group, &self.list) == (o.at, o.group, &o.list)
    }
}
impl Eq for MenuDrop {}

/// Which of a study's control groups are one field (`multi`: several on at once; `custom`: a
/// typed value too): the audited rows. The rest stay chips.
fn drop_kind(r: &StudyRef, group: &str) -> Option<(bool, Option<CustomTo>)> {
    Some(match (r, group) {
        (StudyRef::Vwap(_), "Anchor") | (StudyRef::Profile, "Mode") => (false, None),
        (StudyRef::Vwap(_), "Bands")
        | (StudyRef::Profile, "Label")
        | (StudyRef::Levels(_), "Kinds") => (true, None),
        (StudyRef::Profile, "Grouping") => (false, Some(CustomTo::ProfileGrouping)),
        (StudyRef::Footprint, "Grouping") => (false, Some(CustomTo::FootprintGrouping)),
        _ => return None,
    })
}

/// A dropdown's field: the option in force (one pick); "None", "All", one or two names,
/// "All but X" (one off), or "N of M" (several).
pub fn drop_summary(options: &[Opt], multi: bool) -> String {
    let on: Vec<&str> = options
        .iter()
        .filter(|o| o.on)
        .map(|o| o.label.as_str())
        .collect();
    match (multi, on.as_slice()) {
        (false, [first, ..]) => (*first).to_owned(),
        (false, []) => "—".to_owned(),
        (true, []) => "None".to_owned(),
        (true, on) if on.len() == options.len() && on.len() > 2 => "All".to_owned(),
        (true, [a]) => (*a).to_owned(),
        (true, [a, b]) => format!("{a}, {b}"),
        (true, on) if on.len() + 1 == options.len() => match options.iter().find(|o| !o.on) {
            Some(off) => format!("All but {}", off.label),
            None => format!("{} of {}", on.len(), options.len()),
        },
        (true, on) => format!("{} of {}", on.len(), options.len()),
    }
}

/// Fold each audited group of chips in `out[start..]` into one [`Item::Drop`].
fn fold_drops(r: &StudyRef, out: &mut Vec<Item>, start: usize) {
    let tail = out.split_off(start);
    let mut it = tail.into_iter().peekable();
    while let Some(item) = it.next() {
        let Item::Control {
            group,
            label,
            on,
            swatch,
            cmd,
        } = item
        else {
            out.push(item);
            continue;
        };
        let Some((multi, custom)) = drop_kind(r, group) else {
            out.push(Item::Control {
                group,
                label,
                on,
                swatch,
                cmd,
            });
            continue;
        };
        let mut options = vec![Opt { label, on, cmd }];
        while let Some(Item::Control { group: g, .. }) = it.peek() {
            if *g != group {
                break;
            }
            if let Some(Item::Control { label, on, cmd, .. }) = it.next() {
                options.push(Opt { label, on, cmd });
            }
        }
        out.push(Item::Drop {
            group,
            label: drop_summary(&options, multi),
            multi,
            options,
            custom,
        });
    }
}

impl Item {
    pub fn focusable(&self) -> bool {
        !matches!(
            self,
            Item::Header(_) | Item::Note(_) | Item::Section(_) | Item::Reason(_) | Item::Hint(_)
        )
    }
}

/// The open menu's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StudyMenu {
    /// Filters the Add catalog.
    pub query: String,
    /// Index into [`StudyMenu::items`] (always a focusable item when any exists).
    pub cursor: usize,
    pub editing: Option<StudyRef>,
    /// A template name being typed.
    pub naming: Option<String>,
    /// A dropdown's list, open.
    pub drop: Option<MenuDrop>,
    /// A plugin setting being typed (Enter applies, Esc goes back).
    pub typing: Option<Typing>,
    /// A plugin switch's question, asked before it turns on: (source, key).
    pub asking: Option<(String, &'static str)>,
}

pub fn reset_label(r: Reset) -> String {
    match r {
        Reset::Anchored { from } => format!("anchored {}", lines_core::UtcMillis(from)),
        r => r.label().to_owned(),
    }
}

pub fn vwap_label(spec: &VwapSpec) -> String {
    let mut s = format!("VWAP {}", reset_label(spec.reset));
    for k in 1..=3u8 {
        if spec.bands.has(k) {
            s.push_str(&format!(" ±{k}σ"));
        }
    }
    s
}

/// Volume's row: its name and the options that are on.
pub fn volume_label(v: &VolumeSpec) -> String {
    let mut s = "Volume".to_owned();
    if v.color_by_candle && !v.split {
        s.push_str(" · by candle");
    }
    if v.split {
        s.push_str(" · buy/sell");
    }
    if v.average {
        s.push_str(&format!(" · average {}", v.window()));
    }
    if v.overlay {
        s.push_str(" · overlay");
    }
    s
}

/// The Add catalog: `(label, command)`.
fn catalog(input: &MenuInput) -> Vec<(String, MenuCmd)> {
    let set = |a| MenuCmd::Set(a);
    let mut v = vec![(
        format!("VWAP · {}", session_choice("ETH", input.opens.eth.as_ref())),
        set(StudyAction::AddVwap(Reset::Eth)),
    )];
    if input.has_rth {
        v.push((
            format!("VWAP · {}", session_choice("RTH", input.opens.rth.as_ref())),
            set(StudyAction::AddVwap(Reset::Rth)),
        ));
    }
    v.push((
        "VWAP · week".into(),
        set(StudyAction::AddVwap(Reset::Weekly)),
    ));
    v.push((
        "VWAP · month".into(),
        set(StudyAction::AddVwap(Reset::Monthly)),
    ));
    v.push(("VWAP · from a bar…".into(), MenuCmd::BeginAnchor(None)));
    if !input
        .set
        .indicators
        .iter()
        .any(|c| matches!(c, IndicatorConfig::Volume { .. }))
    {
        v.push(("Volume".into(), set(StudyAction::AddVolume)));
    }
    if !input
        .set
        .indicators
        .iter()
        .any(|c| matches!(c, IndicatorConfig::CumDelta { .. }))
    {
        // The same session resets as VWAP's, named the same way.
        v.push((
            format!(
                "Cumulative delta · {}",
                session_choice("ETH", input.opens.eth.as_ref())
            ),
            set(StudyAction::AddDelta(DeltaReset::Eth)),
        ));
        if input.has_rth {
            v.push((
                format!(
                    "Cumulative delta · {}",
                    session_choice("RTH", input.opens.rth.as_ref())
                ),
                set(StudyAction::AddDelta(DeltaReset::Rth)),
            ));
        }
        v.push((
            "Cumulative delta · no reset".into(),
            set(StudyAction::AddDelta(DeltaReset::None)),
        ));
    }
    if input.set.footprint.mode == FootprintMode::Off {
        for m in [
            FootprintMode::BidAsk,
            FootprintMode::Delta,
            FootprintMode::Volume,
        ] {
            v.push((
                format!("Footprint · {}", m.label()),
                set(StudyAction::AddFootprint(m)),
            ));
        }
    }
    if input.set.profile.mode == ProfileMode::Off {
        for (m, name) in [
            (ProfileMode::Session, "session"),
            (ProfileMode::VisibleRange, "visible range"),
            (ProfileMode::Composite, "composite"),
        ] {
            v.push((
                format!("Volume profile · {name}"),
                set(StudyAction::AddProfile(m)),
            ));
        }
    }
    // A source's lower-pane series studies, under its name; each once per chart.
    for s in input.sources {
        for (id, name) in &s.series {
            let on = input.set.indicators.iter().any(|c| {
                matches!(c, IndicatorConfig::External { spec, .. }
                    if spec.source == s.id && spec.study == *id)
            });
            if !on {
                v.push((
                    format!("{name} · {}", s.label),
                    set(StudyAction::AddExternal(s.id.clone(), id.clone())),
                ));
            }
        }
    }
    for s in input.sources {
        if !input.set.levels.source_shown(&s.id) {
            v.push((
                format!("Levels · {}", s.label),
                set(StudyAction::ToggleHidden(StudyRef::Levels(s.id.clone()))),
            ));
        }
    }
    v
}

/// Every word of `query` appears in `label` (case-insensitive), as the picker filters.
fn matches(query: &str, label: &str) -> bool {
    let l = label.to_lowercase();
    query
        .split_whitespace()
        .all(|w| l.contains(&w.to_lowercase()))
}

/// A VWAP's swatch: its line's color.
fn vwap_swatch(c: &IndicatorConfig) -> Swatch {
    match study_style::vwap_looks(c)[study_style::VWAP_LINE].color() {
        lines_core::style::Color::Custom(rgb) => Swatch::Custom(rgb),
        lines_core::style::Color::Palette(i) => Swatch::Study(i),
        _ => Swatch::None,
    }
}

fn controls(input: &MenuInput, r: &StudyRef, out: &mut Vec<Item>) {
    let start = out.len();
    controls_of(input, r, out);
    fold_drops(r, out, start);
    // Every study ends with its styles (one entry: the parts are the style editor's tabs).
    if out.last().is_some_and(|i| {
        matches!(
            i,
            Item::Control { .. } | Item::Drop { .. } | Item::Step { .. }
        )
    }) {
        out.push(Item::Control {
            group: "Style",
            label: "Style…".into(),
            on: false,
            swatch: Swatch::None,
            cmd: MenuCmd::Style(r.clone()),
        });
    }
}

fn controls_of(input: &MenuInput, r: &StudyRef, out: &mut Vec<Item>) {
    let set = |a| MenuCmd::Set(a);
    macro_rules! c {
        ($group:expr, $label:expr, $on:expr, $swatch:expr, $cmd:expr $(,)?) => {
            out.push(Item::Control {
                group: $group,
                label: $label,
                on: $on,
                swatch: $swatch,
                cmd: $cmd,
            })
        };
    }
    // A number with − / + (`drop_list::step`).
    macro_rules! step {
        ($group:expr, $label:expr, $less:expr, $more:expr $(,)?) => {
            out.push(Item::Step {
                group: $group,
                label: $label,
                less: $less,
                more: $more,
                typed: None,
                dim: false,
                typing: false,
                refused: false,
            })
        };
    }
    match r {
        StudyRef::Delta(i) => {
            let Some(IndicatorConfig::CumDelta { spec, display, .. }) =
                input.set.indicators.get(*i)
            else {
                return;
            };
            let i = *i;
            let mut resets = vec![(DeltaReset::Eth, "ETH")];
            if input.has_rth {
                resets.push((DeltaReset::Rth, "RTH"));
            }
            resets.push((DeltaReset::None, "None"));
            for (r, name) in resets {
                c!(
                    "Reset",
                    name.into(),
                    spec.reset == r,
                    Swatch::None,
                    set(StudyAction::SetDeltaReset(i, r)),
                );
            }
            c!(
                "Label",
                "Show title".into(),
                display.label,
                Swatch::None,
                set(StudyAction::ToggleLabel(i)),
            );
        }
        StudyRef::Volume(i) => {
            let Some(IndicatorConfig::Volume { spec, display, .. }) = input.set.indicators.get(*i)
            else {
                return;
            };
            let (i, v) = (*i, *spec);
            let with = |f: &dyn Fn(&mut VolumeSpec)| {
                let mut n = v;
                f(&mut n);
                set(StudyAction::SetVolume(i, n))
            };
            // Split draws each side in its direction's shape and color: no candle colors then.
            if !v.split {
                c!(
                    "Color",
                    "By candle".into(),
                    v.color_by_candle,
                    Swatch::None,
                    with(&|n| n.color_by_candle = !n.color_by_candle),
                );
            }
            c!(
                "Average",
                "Show".into(),
                v.average,
                Swatch::None,
                with(&|n| n.average = !n.average),
            );
            if v.average {
                let len = i64::from(v.average_len);
                let to = |up| {
                    crate::drop_list::step(len, 10, 1, i64::from(VolumeSpec::MAX_LEN), up)
                        .map(|n| with(&|s| s.average_len = n as u16))
                };
                step!("Length", format!("{len} bars"), to(false), to(true));
            }
            c!(
                "Split",
                "Buy / sell".into(),
                v.split,
                Swatch::None,
                with(&|n| n.split = !n.split),
            );
            for (overlay, name) in [(false, "Pane"), (true, "Overlay")] {
                c!(
                    "Display",
                    name.into(),
                    v.overlay == overlay,
                    Swatch::None,
                    with(&|n| n.overlay = overlay),
                );
            }
            c!(
                "Label",
                if v.overlay {
                    "Show in legend"
                } else {
                    "Show title"
                }
                .into(),
                display.label,
                Swatch::None,
                set(StudyAction::ToggleLabel(i)),
            );
        }
        StudyRef::External(i) => {
            let Some(IndicatorConfig::External { display, .. }) = input.set.indicators.get(*i)
            else {
                return;
            };
            c!(
                "Label",
                "Show title".into(),
                display.label,
                Swatch::None,
                set(StudyAction::ToggleLabel(*i)),
            );
        }
        StudyRef::Footprint => {
            let f = &input.set.footprint;
            for m in [
                FootprintMode::BidAsk,
                FootprintMode::Delta,
                FootprintMode::Volume,
            ] {
                c!(
                    "Show",
                    m.label().into(),
                    f.mode == m,
                    Swatch::None,
                    set(StudyAction::SetFootprintMode(m)),
                );
            }
            let pct = i64::from(f.imbalance_pct);
            let ratio = |up| {
                crate::drop_list::step(pct, 25, 100, i64::from(MAX_IMBALANCE_PCT), up)
                    .map(|r| set(StudyAction::SetImbalanceRatio(r as u32)))
            };
            step!("Imbalance", format!("{pct}%"), ratio(false), ratio(true));
            let min = f.imbalance_min;
            let least = |up| {
                crate::drop_list::step(min, 1, 1, MAX_IMBALANCE_MIN, up)
                    .map(|m| set(StudyAction::SetImbalanceMin(m)))
            };
            step!("Min volume", format!("{min}"), least(false), least(true));
            c!(
                "Grouping",
                "Auto".into(),
                f.grouping.is_none(),
                Swatch::None,
                set(StudyAction::SetFootprintGrouping(None)),
            );
            for g in lines_core::ladder::GROUPING_PRESETS {
                c!(
                    "Grouping",
                    format!("{g}"),
                    f.grouping == Some(g),
                    Swatch::None,
                    set(StudyAction::SetFootprintGrouping(Some(g))),
                );
            }
            // A typed one ("Custom…"): listed, in force.
            if let Some(g) = f
                .grouping
                .filter(|g| !lines_core::ladder::GROUPING_PRESETS.contains(g))
            {
                c!(
                    "Grouping",
                    format!("{g}"),
                    true,
                    Swatch::None,
                    set(StudyAction::SetFootprintGrouping(Some(g))),
                );
            }
        }
        StudyRef::Vwap(i) => {
            let Some(IndicatorConfig::Vwap {
                spec,
                color,
                display,
                ..
            }) = input.set.indicators.get(*i)
            else {
                return;
            };
            let i = *i;
            let mut resets = vec![(Reset::Eth, session_choice("ETH", input.opens.eth.as_ref()))];
            if input.has_rth {
                resets.push((Reset::Rth, session_choice("RTH", input.opens.rth.as_ref())));
            }
            resets.extend([
                (Reset::Weekly, "Week".to_owned()),
                (Reset::Monthly, "Month".to_owned()),
            ]);
            // "Anchor": where the VWAP starts (the Add list's terms).
            for (r, name) in resets {
                c!(
                    "Anchor",
                    name,
                    spec.reset == r,
                    Swatch::None,
                    set(StudyAction::SetReset(i, r)),
                );
            }
            c!(
                "Anchor",
                "From a bar…".into(),
                matches!(spec.reset, Reset::Anchored { .. }),
                Swatch::None,
                MenuCmd::BeginAnchor(Some(i)),
            );
            for k in 1..=3u8 {
                c!(
                    "Bands",
                    format!("±{k}σ"),
                    spec.bands.has(k),
                    Swatch::None,
                    set(StudyAction::ToggleBand(i, k)),
                );
            }
            // A custom line color (the style editor's) selects none of the theme colors.
            let custom = matches!(
                input.set.indicators.get(i).map(vwap_swatch),
                Some(Swatch::Custom(_))
            );
            for k in 0..4u8 {
                c!(
                    "Color",
                    format!("{}", k + 1),
                    *color == k && !custom,
                    Swatch::Study(k),
                    set(StudyAction::SetColor(i, k)),
                );
            }
            for (w, name) in [(1u8, "Thin"), (2, "Normal"), (3, "Thick"), (4, "Heavy")] {
                c!(
                    "Width",
                    name.into(),
                    display.width == w,
                    Swatch::None,
                    set(StudyAction::SetWidth(i, w)),
                );
            }
            c!(
                "Label",
                "Show in legend".into(),
                display.label,
                Swatch::None,
                set(StudyAction::ToggleLabel(i)),
            );
        }
        StudyRef::Profile => {
            let p = &input.set.profile;
            for (m, name) in [
                (ProfileMode::Session, "Session"),
                (ProfileMode::VisibleRange, "Visible range"),
                (ProfileMode::Composite, "Composite"),
            ] {
                c!(
                    "Mode",
                    name.into(),
                    p.mode == m,
                    Swatch::None,
                    set(StudyAction::SetProfileMode(m)),
                );
            }
            let mut sessions = vec![(ProfileSession::Eth, "ETH")];
            if input.has_rth {
                sessions.push((ProfileSession::Rth, "RTH"));
            }
            for (s, name) in sessions {
                c!(
                    "Session",
                    name.into(),
                    p.session == s,
                    Swatch::None,
                    set(StudyAction::SetProfileSession(s)),
                );
            }
            c!(
                "Split",
                "Buy / sell".into(),
                p.split,
                Swatch::None,
                set(StudyAction::ToggleProfileSplit),
            );
            c!(
                "Grouping",
                "Auto".into(),
                p.grouping.is_none(),
                Swatch::None,
                set(StudyAction::SetProfileGrouping(None)),
            );
            for g in lines_core::ladder::GROUPING_PRESETS {
                c!(
                    "Grouping",
                    format!("{g}"),
                    p.grouping == Some(g),
                    Swatch::None,
                    set(StudyAction::SetProfileGrouping(Some(g))),
                );
            }
            // A typed one ("Custom…"): listed, in force.
            if let Some(g) = p
                .grouping
                .filter(|g| !lines_core::ladder::GROUPING_PRESETS.contains(g))
            {
                c!(
                    "Grouping",
                    format!("{g}"),
                    true,
                    Swatch::None,
                    set(StudyAction::SetProfileGrouping(Some(g))),
                );
            }
            for part in crate::profile::ProfileLabelPart::ALL {
                c!(
                    "Label",
                    part.label().into(),
                    p.label_part(part),
                    Swatch::None,
                    set(StudyAction::ToggleProfileLabel(part)),
                );
            }
        }
        StudyRef::Levels(id) => {
            let Some(s) = input.sources.iter().find(|s| s.id == *id) else {
                return;
            };
            for (i, k) in s.kinds.iter().enumerate() {
                let parent = s.parents.get(i).copied().flatten();
                let shown = input.set.levels.kind_shown_under(id, k, parent);
                // Hidden through its parent (a kind the source has split): showing it hides
                // its siblings and the parent itself in the parent's place.
                let action = match parent {
                    Some(_) if !shown && input.set.levels.kind_shown(id, k) => {
                        StudyAction::ShowLevelKindUnder(id.clone(), k.clone())
                    }
                    _ => StudyAction::ToggleLevelKind(id.clone(), k.clone()),
                };
                // A type turned off in its style editor reads unchecked and says where.
                let off = s.styled_off.get(i).copied().unwrap_or(false);
                let label = if off {
                    format!("{k} (off in Style…)")
                } else {
                    k.clone()
                };
                c!("Kinds", label, shown && !off, Swatch::None, set(action));
            }
            c!(
                "Labels",
                "Show labels".into(),
                input.set.levels.labels_shown(id),
                Swatch::None,
                set(StudyAction::ToggleLevelLabels(id.clone())),
            );
            // Where every study label on the chart goes (not only this source's).
            for p in crate::levels_layer::LabelPlace::ALL {
                c!(
                    "Level labels",
                    p.label().into(),
                    input.set.levels.label_place == p,
                    Swatch::None,
                    set(StudyAction::SetLabelPlace(p)),
                );
            }
        }
    }
}

impl StudyMenu {
    /// A level source's own settings under its controls ("Source settings": app-wide, one
    /// feed serves every chart), drawn from its specs (`crate::plugin_settings`): a group's
    /// switches as chips on one line (a lone switch, or one that asks, on its own), a choice as
    /// a dropdown, seconds and numbers with − / + (Enter types one), text and addresses as
    /// fields; a refused value keeps its text with the reason under it; a switch's question
    /// under it until answered.
    fn plugin_items(&self, s: &SourceInfo, out: &mut Vec<Item>) {
        use lines_feed::levels::{SettingKind, setting};
        if s.plugin.is_empty() {
            return;
        }
        out.push(Item::Section("Source settings".into()));
        out.push(Item::Hint(format!(
            "For every chart: one {} feed serves them all.",
            s.label
        )));
        let set = |key: &'static str, value: String| {
            MenuCmd::Plugin(PluginCmd::Set {
                source: s.id.clone(),
                key,
                value,
            })
        };
        let edit = |key: &'static str, text: String| {
            MenuCmd::Plugin(PluginCmd::Edit {
                source: s.id.clone(),
                key,
                text,
            })
        };
        let flip = |v: &str| if setting::is_on(v) { "false" } else { "true" }.to_owned();
        let rows = &s.plugin;
        let mut group = "";
        let mut i = 0;
        while i < rows.len() {
            let r = &rows[i];
            if r.spec.group != group {
                group = r.spec.group;
                if !group.is_empty() {
                    out.push(Item::Section(group.to_owned()));
                }
            }
            let key = r.spec.key;
            let typing = self
                .typing
                .as_ref()
                .filter(|t| t.source == s.id && t.key == key);
            match r.spec.kind {
                SettingKind::Bool => {
                    let simple = |x: &&crate::plugin_settings::Row| {
                        x.spec.kind == SettingKind::Bool
                            && x.spec.group == group
                            && x.spec.confirm.is_empty()
                    };
                    let run = rows[i..].iter().take_while(simple).count();
                    if run >= 2 && !group.is_empty() {
                        // Under the group's own heading: no name of their own in the label
                        // column (never "Display" under "DISPLAY").
                        for x in &rows[i..i + run] {
                            out.push(Item::Control {
                                group: "",
                                label: x.spec.label.into(),
                                on: setting::is_on(&x.value),
                                swatch: Swatch::None,
                                cmd: set(x.spec.key, flip(&x.value)),
                            });
                        }
                        i += run;
                        continue;
                    }
                    let on = setting::is_on(&r.value);
                    out.push(Item::Control {
                        group: r.spec.label,
                        label: if on { "On" } else { "Off" }.into(),
                        on,
                        swatch: Swatch::None,
                        cmd: set(key, flip(&r.value)),
                    });
                    if self
                        .asking
                        .as_ref()
                        .is_some_and(|(src, k)| *src == s.id && *k == key)
                    {
                        out.push(Item::Hint(r.spec.confirm.to_owned()));
                        out.push(Item::Action {
                            label: format!("Turn on {}", r.spec.label.to_lowercase()),
                            cmd: MenuCmd::Plugin(PluginCmd::Answer(true)),
                            delete: None,
                        });
                        out.push(Item::Action {
                            label: "Cancel".into(),
                            cmd: MenuCmd::Plugin(PluginCmd::Answer(false)),
                            delete: None,
                        });
                    }
                }
                SettingKind::Choice(_) => out.push(Item::Drop {
                    group: r.spec.label,
                    label: r.choice_label(),
                    multi: false,
                    options: r
                        .options
                        .iter()
                        .map(|(v, l)| Opt {
                            label: l.clone(),
                            on: v.eq_ignore_ascii_case(&r.value),
                            cmd: set(key, v.clone()),
                        })
                        .collect(),
                    custom: None,
                }),
                SettingKind::Seconds { .. } | SettingKind::Number { .. } => {
                    // Typed from the value in its stored form (no unit), or the refused text.
                    let start = match (&r.refused, r.spec.kind) {
                        (Some(f), _) => f.text.clone(),
                        (None, SettingKind::Seconds { .. }) => r.shown.clone(),
                        (None, _) => r.value.clone(),
                    };
                    let (label, dim) = match typing {
                        Some(t) => (format!("{}\u{258f}", t.buf), false),
                        None if r.text().is_empty() => (r.spec.placeholder.to_owned(), true),
                        None => (r.text().to_owned(), false),
                    };
                    out.push(Item::Step {
                        group: r.spec.label,
                        label,
                        less: r.step(false).map(|v| set(key, v)),
                        more: r.step(true).map(|v| set(key, v)),
                        typed: Some(edit(key, start)),
                        dim,
                        typing: typing.is_some(),
                        refused: r.refused.is_some(),
                    });
                }
                SettingKind::Text | SettingKind::Url => out.push(Item::Field {
                    group: r.spec.label,
                    text: typing
                        .map_or_else(|| r.text().to_owned(), |t| format!("{}\u{258f}", t.buf)),
                    placeholder: r.spec.placeholder,
                    typing: typing.is_some(),
                    refused: r.refused.is_some(),
                    cmd: edit(key, r.text().to_owned()),
                }),
                SettingKind::Secret => {}
            }
            if let Some(line) = r.refused_line()
                && typing.is_none()
            {
                out.push(Item::Reason(line));
            }
            i += 1;
        }
    }

    /// The menu's lines, top to bottom.
    pub fn items(&self, input: &MenuInput) -> Vec<Item> {
        let mut v = vec![Item::Header(input.scope.header().into())];
        let mut studies: Vec<(StudyRef, String, Swatch, bool)> = Vec::new();
        for (i, c) in input.set.indicators.iter().enumerate() {
            match c {
                IndicatorConfig::Vwap { spec, display, .. } => studies.push((
                    StudyRef::Vwap(i),
                    vwap_label(spec),
                    vwap_swatch(c),
                    display.hidden,
                )),
                IndicatorConfig::CumDelta { spec, display, .. } => studies.push((
                    StudyRef::Delta(i),
                    format!("Cumulative delta · {}", spec.reset.label()),
                    Swatch::None,
                    display.hidden,
                )),
                IndicatorConfig::Volume { spec, display, .. } => studies.push((
                    StudyRef::Volume(i),
                    volume_label(spec),
                    Swatch::None,
                    display.hidden,
                )),
                IndicatorConfig::External { spec, display, .. } => {
                    // Its name and source as the source offers it; the ids if the source
                    // isn't here (it stays, and shows when the source comes back).
                    let src = input.sources.iter().find(|s| s.id == spec.source);
                    let name = src
                        .and_then(|s| s.series.iter().find(|(id, _)| *id == spec.study))
                        .map_or(spec.study.as_str(), |(_, n)| n.as_str());
                    let label = src.map_or(spec.source.as_str(), |s| s.label.as_str());
                    studies.push((
                        StudyRef::External(i),
                        format!("{name} · {label}"),
                        Swatch::None,
                        display.hidden,
                    ));
                }
            }
        }
        let f = &input.set.footprint;
        if f.mode != FootprintMode::Off {
            studies.push((
                StudyRef::Footprint,
                format!("Footprint · {}", f.mode.label()),
                Swatch::None,
                f.hidden,
            ));
        }
        let p = &input.set.profile;
        if p.mode != ProfileMode::Off {
            let mode = match p.mode {
                ProfileMode::Session => "session",
                ProfileMode::VisibleRange => "visible range",
                ProfileMode::Composite => "composite",
                ProfileMode::Off => "",
            };
            let session = match p.session {
                ProfileSession::Eth => "ETH",
                ProfileSession::Rth => "RTH",
            };
            studies.push((
                StudyRef::Profile,
                format!("Volume profile · {mode} · {session}"),
                Swatch::None,
                p.hidden,
            ));
        }
        for s in input.sources {
            if input.set.levels.source_shown(&s.id) {
                studies.push((
                    StudyRef::Levels(s.id.clone()),
                    format!("Levels · {}", s.label),
                    Swatch::None,
                    false,
                ));
            }
        }
        if studies.is_empty() {
            v.push(Item::Note("Nothing yet: add from the list below.".into()));
        }
        for (r, label, swatch, hidden) in studies {
            let editing = self.editing.as_ref() == Some(&r);
            v.push(Item::Study {
                r: r.clone(),
                label,
                swatch,
                hidden,
                editing,
            });
            if editing {
                controls(input, &r, &mut v);
                if let StudyRef::Levels(id) = &r
                    && let Some(s) = input.sources.iter().find(|s| s.id == *id)
                {
                    self.plugin_items(s, &mut v);
                }
            }
        }
        // Whose studies these are, under them: this chart's own instead, or the symbol's again.
        v.push(Item::Action {
            label: match &input.scope {
                StudyScope::Symbol(_) => "Use separate studies on this chart".into(),
                StudyScope::Chart(s) => format!("Use {s}'s studies on this chart"),
            },
            cmd: MenuCmd::ToggleSeparate,
            delete: None,
        });
        v.push(Item::Header("Add".into()));
        let mut any = false;
        for (label, cmd) in catalog(input) {
            if matches(&self.query, &label) {
                any = true;
                v.push(Item::Action {
                    label,
                    cmd,
                    delete: None,
                });
            }
        }
        if !any {
            v.push(Item::Note(format!("No study matches \"{}\"", self.query)));
        }
        v.push(Item::Header("Templates".into()));
        for t in input.templates {
            v.push(Item::Action {
                label: format!("Apply template \"{t}\""),
                cmd: MenuCmd::ApplyTemplate(t.clone()),
                delete: Some(MenuCmd::DeleteTemplate(t.clone())),
            });
        }
        match &self.naming {
            Some(name) => v.push(Item::Note(format!(
                "Template name: {name}▏ (Enter saves, Esc cancels)"
            ))),
            None => v.push(Item::Action {
                label: "Save these studies as a template…".into(),
                cmd: MenuCmd::StartSaveTemplate,
                delete: None,
            }),
        }
        v.push(Item::Action {
            label: "Save as default for new charts".into(),
            cmd: MenuCmd::SaveDefault,
            delete: None,
        });
        v.push(Item::Note(match &input.scope {
            StudyScope::Symbol(s) => format!("Changes apply to every {s} chart."),
            StudyScope::Chart(_) => "Changes apply to this chart only.".into(),
        }));
        v
    }

    fn focusables(items: &[Item]) -> Vec<usize> {
        items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.focusable())
            .map(|(i, _)| i)
            .collect()
    }

    /// Keep the cursor on a focusable item after the items changed (an open list whose item
    /// went closes).
    pub fn clamp(&mut self, items: &[Item]) {
        // An open list follows its item when the lines above it change (a level source learned
        // a kind); one whose item went, or whose options changed under it: closed, never a row
        // toggling what another row now is.
        if let Some(d) = &self.drop {
            let same = |it: &Item| {
                matches!(it, Item::Drop { group, options, .. }
                    if *group == d.group
                        && options.len() == d.list.entries.len()
                        && options.iter().zip(&d.list.entries).all(|(o, e)| o.label == e.label))
            };
            if !items.get(d.at).is_some_and(same) {
                match items.iter().position(same) {
                    Some(at) => {
                        if self.cursor == d.at {
                            self.cursor = at;
                        }
                        if let Some(d) = self.drop.as_mut() {
                            d.at = at;
                        }
                    }
                    None => self.drop = None,
                }
            }
        }
        let f = Self::focusables(items);
        if f.is_empty() {
            self.cursor = 0;
        } else if !f.contains(&self.cursor) {
            self.cursor = *f
                .iter()
                .find(|&&i| i >= self.cursor)
                .unwrap_or(f.last().unwrap());
        }
    }

    fn step(&mut self, items: &[Item], d: isize) {
        let f = Self::focusables(items);
        if f.is_empty() {
            return;
        }
        let at = f.iter().position(|&i| i == self.cursor).unwrap_or(0) as isize;
        let n = f.len() as isize;
        self.cursor = f[((at + d).rem_euclid(n)) as usize];
    }

    /// The first catalog entry (typing moves the cursor there, as in the picker).
    fn first_add(items: &[Item]) -> Option<usize> {
        let add = items
            .iter()
            .position(|i| *i == Item::Header("Add".into()))?;
        items[add..]
            .iter()
            .position(|i| matches!(i, Item::Action { .. }))
            .map(|p| add + p)
    }

    /// Open item `i`'s list (a [`Item::Drop`]), lit at its option in force; again: close it.
    pub fn open_drop(&mut self, items: &[Item], i: usize) {
        let Some(Item::Drop {
            group,
            multi,
            options,
            custom,
            ..
        }) = items.get(i)
        else {
            return;
        };
        if self.drop.as_ref().is_some_and(|d| d.at == i) {
            self.drop = None;
            return;
        }
        let rows = options
            .iter()
            .map(|o| crate::drop_list::Entry::new(o.label.clone()))
            .collect();
        let at = if *multi {
            None
        } else {
            options.iter().position(|o| o.on)
        };
        self.cursor = i;
        self.drop = Some(MenuDrop {
            at: i,
            group,
            list: crate::drop_list::DropList::new(rows, *multi, custom.is_some(), at),
            scroll: gpui::ScrollHandle::new(),
        });
    }

    /// Row `k` of the open list clicked.
    pub fn drop_choose(&mut self, items: &[Item], k: usize) -> Option<MenuCmd> {
        self.typing = None;
        let o = self.drop.as_mut()?.list.choose(k);
        self.drop_outcome(items, o)
    }

    /// What a key or a click in the open list does: a pick runs its option and closes, a
    /// toggle runs it and stays open, a typed value runs if it is one (else the field stays).
    fn drop_outcome(&mut self, items: &[Item], o: crate::drop_list::Outcome) -> Option<MenuCmd> {
        use crate::drop_list::Outcome;
        let d = self.drop.as_ref()?;
        let Some(Item::Drop {
            options, custom, ..
        }) = items.get(d.at)
        else {
            self.drop = None;
            return None;
        };
        match o {
            Outcome::None => None,
            Outcome::Close => {
                self.drop = None;
                None
            }
            Outcome::Pick(k) => {
                self.drop = None;
                options.get(k).map(|o| o.cmd.clone())
            }
            Outcome::Toggle(k) => {
                if let Some(d) = self.drop.as_mut() {
                    d.list.lit = k;
                }
                options.get(k).map(|o| o.cmd.clone())
            }
            Outcome::Submit(text) => {
                let cmd = custom.and_then(|c| c.cmd(&text));
                if cmd.is_some() {
                    self.drop = None;
                }
                cmd
            }
        }
    }

    /// − (`up` false) or + on item `i` (a [`Item::Step`]).
    pub fn nudge(&mut self, items: &[Item], i: usize, up: bool) -> Option<MenuCmd> {
        self.cursor = i;
        self.typing = None;
        match items.get(i)? {
            Item::Step { less, more, .. } => if up { more } else { less }.clone(),
            _ => None,
        }
    }

    /// The primary command of item `i` (a click on it, or Enter).
    pub fn activate(&mut self, items: &[Item], i: usize) -> Option<MenuCmd> {
        self.cursor = i;
        // A press elsewhere ends typing (on the field itself, typing goes on).
        let own = |cmd: &MenuCmd| {
            matches!(cmd, MenuCmd::Plugin(PluginCmd::Edit { source, key, .. })
                if self.typing.as_ref().is_some_and(|t| t.source == *source && t.key == *key))
        };
        let keep = match items.get(i) {
            Some(Item::Field { cmd, .. }) => own(cmd),
            Some(Item::Step {
                typed: Some(cmd), ..
            }) => own(cmd),
            _ => false,
        };
        if !keep {
            self.typing = None;
        }
        match items.get(i)? {
            Item::Study { r, .. } => Some(MenuCmd::Edit(r.clone())),
            Item::Drop { .. } => {
                self.open_drop(items, i);
                None
            }
            Item::Control { cmd, .. } | Item::Action { cmd, .. } | Item::Field { cmd, .. } => {
                Some(cmd.clone())
            }
            Item::Step {
                typed: Some(cmd), ..
            } => Some(cmd.clone()),
            _ => None,
        }
    }

    /// The eye of a study row (Space).
    pub fn eye(&mut self, items: &[Item], i: usize) -> Option<MenuCmd> {
        self.cursor = i;
        self.typing = None;
        match items.get(i)? {
            Item::Study { r, .. } => Some(MenuCmd::Set(StudyAction::ToggleHidden(r.clone()))),
            Item::Control { cmd, .. } => Some(cmd.clone()),
            Item::Drop { .. } => {
                self.open_drop(items, i);
                None
            }
            _ => None,
        }
    }

    /// Delete on an item: remove a study, delete a template.
    pub fn delete(&mut self, items: &[Item], i: usize) -> Option<MenuCmd> {
        self.cursor = i;
        self.typing = None;
        match items.get(i)? {
            Item::Study { r, .. } => Some(MenuCmd::Set(StudyAction::Remove(r.clone()))),
            Item::Action {
                delete: Some(d), ..
            } => Some(d.clone()),
            _ => None,
        }
    }

    /// A key while the menu is open (`text`: the character typed, if printable and without
    /// Ctrl/Alt). Returns the command to run, if any; the menu state is updated in place.
    pub fn key(&mut self, items: &[Item], key: &str, text: Option<&str>) -> Option<MenuCmd> {
        self.key_at(items, key, text, std::time::Instant::now())
    }

    /// [`StudyMenu::key`] at `now` (an open list's typing jumps by time).
    pub fn key_at(
        &mut self,
        items: &[Item],
        key: &str,
        text: Option<&str>,
        now: std::time::Instant,
    ) -> Option<MenuCmd> {
        // An open list takes every key (Esc closes it, not the menu).
        if let Some(d) = self.drop.as_mut() {
            let o = d.list.key(key, text, now);
            return self.drop_outcome(items, o);
        }
        if let Some(t) = self.typing.as_mut() {
            match key {
                "escape" => {
                    let t = self.typing.take()?;
                    return Some(MenuCmd::Plugin(PluginCmd::Discard {
                        source: t.source,
                        key: t.key,
                    }));
                }
                "enter" => {
                    let t = self.typing.take()?;
                    return Some(MenuCmd::Plugin(PluginCmd::Set {
                        source: t.source,
                        key: t.key,
                        value: t.buf,
                    }));
                }
                "backspace" => {
                    t.buf.pop();
                }
                _ => {
                    if let Some(c) = text.filter(|c| !c.chars().any(char::is_control))
                        && t.buf.chars().count() < 200
                    {
                        t.buf.push_str(c);
                    }
                }
            }
            return None;
        }
        // A question asked: Enter or Esc leaves it (the safe answer); the buttons answer.
        if self.asking.is_some() && matches!(key, "escape") {
            self.asking = None;
            return None;
        }
        if let Some(name) = self.naming.as_mut() {
            match key {
                "escape" => self.naming = None,
                "enter" => {
                    let name = name.trim().to_owned();
                    self.naming = None;
                    return (!name.is_empty()).then_some(MenuCmd::SaveTemplate(name));
                }
                "backspace" => {
                    name.pop();
                }
                _ => {
                    if let Some(t) = text
                        && name.chars().count() < 40
                    {
                        name.push_str(t);
                    }
                }
            }
            return None;
        }
        match key {
            "escape" => {
                if self.editing.is_some() {
                    self.editing = None;
                } else if !self.query.is_empty() {
                    self.query.clear();
                } else {
                    return Some(MenuCmd::Close);
                }
            }
            "up" => self.step(items, -1),
            "down" | "tab" => self.step(items, 1),
            // A number's − / +.
            "left" | "right" if matches!(items.get(self.cursor), Some(Item::Step { .. })) => {
                return self.nudge(items, self.cursor, key == "right");
            }
            "enter" => return self.activate(items, self.cursor),
            "space" => return self.eye(items, self.cursor),
            "delete" => return self.delete(items, self.cursor),
            "backspace" => {
                self.query.pop();
            }
            _ => {
                if let Some(t) = text.filter(|t| !t.chars().any(char::is_control)) {
                    // As in the picker: typing searches the catalog (the caller moves the
                    // cursor to the first match with `follow_query`).
                    self.query.push_str(t);
                }
            }
        }
        None
    }

    /// After the query changed: move the cursor to the first matching catalog entry.
    pub fn follow_query(&mut self, items: &[Item]) {
        if let Some(i) = Self::first_add(items) {
            self.cursor = i;
        } else {
            self.clamp(items);
        }
    }

    /// Run the menu's own part of a command (editor and naming state). Returns the command
    /// for the chart to finish (study changes, anchoring, templates), or `None`.
    pub fn local(&mut self, cmd: MenuCmd) -> Option<MenuCmd> {
        match cmd {
            MenuCmd::Edit(r) => {
                // Another editor (or none): a value being typed or a question goes with the
                // old one, so no key lands in a field no longer shown.
                self.typing = None;
                self.asking = None;
                self.editing = if self.editing.as_ref() == Some(&r) {
                    None
                } else {
                    Some(r)
                };
                None
            }
            MenuCmd::StartSaveTemplate => {
                self.naming = Some(String::new());
                None
            }
            MenuCmd::Plugin(PluginCmd::Edit { source, key, text }) => {
                self.asking = None;
                // A press on the field being typed keeps what was typed.
                if self
                    .typing
                    .as_ref()
                    .is_some_and(|t| t.source == source && t.key == key)
                {
                    return None;
                }
                self.typing = Some(Typing {
                    source,
                    key,
                    buf: text,
                });
                None
            }
            MenuCmd::Plugin(PluginCmd::Answer(yes)) => {
                let (source, key) = self.asking.take()?;
                yes.then_some(MenuCmd::Plugin(PluginCmd::Confirmed { source, key }))
            }
            MenuCmd::Set(StudyAction::Remove(ref r)) if self.editing.as_ref() == Some(r) => {
                self.editing = None;
                self.typing = None;
                self.asking = None;
                Some(cmd)
            }
            other => Some(other),
        }
    }
}

/// The bar under plot-local `x` (its start time, ms): what a click anchors a VWAP at.
/// `None` off the bars.
pub fn anchor_at(bars: &[Bar], vp: &Viewport, x: f64) -> Option<i64> {
    let i = vp.x_to_index(x).round();
    (i >= 0.0 && (i as usize) < bars.len()).then(|| bars[i as usize].time.0)
}

/// Upsert a template by name (names are unique; saving again replaces it).
pub fn save_template(templates: &mut Vec<StudyTemplate>, name: &str, set: StudySet) {
    match templates.iter_mut().find(|t| t.name == name) {
        Some(t) => t.set = set,
        None => templates.push(StudyTemplate {
            name: name.to_owned(),
            set,
        }),
    }
}

#[cfg(test)]
mod tests;
