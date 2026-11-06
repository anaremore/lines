//! The studies menu (Ctrl+I, the chart's "Studies…", the palette): one place for everything
//! drawn on a chart: VWAPs, the volume profile and external level sources. Pure (no GPUI):
//! the chart renders [`StudyMenu::items`] and feeds keys and clicks back as [`MenuCmd`]s.
//!
//! - **On this chart:** each study with a hide toggle (eye) and an editor (gear). The editor
//!   is a flat list of controls (reset, bands, colour, width, label; profile mode, session,
//!   split, grouping; level kinds and labels), each one a command.
//! - **Add:** a catalog filtered by typing, like the symbol picker.
//! - **Templates:** apply or delete saved study sets; save the chart's set as a template or as
//!   the default for new charts (decision 0021: scope is explicit).
//!
//! Every change is a [`StudyAction`] applied to the chart's [`StudySet`] by
//! [`StudySet::apply`] (pure), so menus, keys and tests share one path.

use lines_core::{Bar, Viewport};
use lines_indicators::{Bands, IndicatorConfig, Reset, VwapSpec};
use serde::{Deserialize, Serialize};

use crate::levels_layer::LevelsConfig;
use crate::profile::{ProfileConfig, ProfileMode, ProfileSession};

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
    ToggleLevelLabels(String),
    /// Replace every study with a template's.
    Replace(StudySet),
}

fn next_color(set: &StudySet) -> u8 {
    let used: Vec<u8> = set
        .indicators
        .iter()
        .map(|c| match c {
            IndicatorConfig::Vwap { color, .. } => *color,
        })
        .collect();
    (0..4u8).find(|c| !used.contains(c)).unwrap_or(0)
}

impl StudySet {
    fn vwap(
        &mut self,
        i: usize,
    ) -> Option<(&mut VwapSpec, &mut u8, &mut lines_indicators::Display)> {
        match self.indicators.get_mut(i)? {
            IndicatorConfig::Vwap {
                spec,
                color,
                display,
            } => Some((spec, color, display)),
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
            StudyAction::ToggleHidden(r) => match r {
                StudyRef::Vwap(i) => {
                    if let Some((.., d)) = self.vwap(*i) {
                        d.hidden = !d.hidden;
                    }
                }
                StudyRef::Profile => self.profile.hidden = !self.profile.hidden,
                StudyRef::Levels(id) => self.levels.toggle_source(id),
            },
            StudyAction::Remove(r) => match r {
                StudyRef::Vwap(i) => {
                    if *i < self.indicators.len() {
                        self.indicators.remove(*i);
                    }
                }
                StudyRef::Profile => {
                    self.profile.mode = ProfileMode::Off;
                    self.profile.hidden = false;
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
                if let Some((_, color, _)) = self.vwap(*i) {
                    *color = *c % 4;
                }
            }
            StudyAction::SetWidth(i, w) => {
                if let Some((.., d)) = self.vwap(*i) {
                    d.width = (*w).clamp(1, 3);
                }
            }
            StudyAction::ToggleLabel(i) => {
                if let Some((.., d)) = self.vwap(*i) {
                    d.label = !d.label;
                }
            }
            StudyAction::SetProfileMode(m) => self.profile.mode = *m,
            StudyAction::SetProfileSession(s) => self.profile.session = *s,
            StudyAction::ToggleProfileSplit => self.profile.split = !self.profile.split,
            StudyAction::SetProfileGrouping(g) => self.profile.grouping = *g,
            StudyAction::ToggleLevelKind(id, kind) => self.levels.toggle_kind(id, kind),
            StudyAction::ToggleLevelLabels(id) => self.levels.toggle_labels(id),
            StudyAction::Replace(set) => *self = set.clone(),
        }
    }
}

/// A command from the menu: a change to the studies, or a menu or template operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCmd {
    Set(StudyAction),
    /// Open (or close) the inline editor of a study.
    Edit(StudyRef),
    /// Wait for a click on a bar to anchor VWAP `index` (or a new one there).
    BeginAnchor(Option<usize>),
    ApplyTemplate(String),
    DeleteTemplate(String),
    /// Start typing a template name (Enter saves).
    StartSaveTemplate,
    SaveTemplate(String),
    /// Save the chart's studies as the default for new charts.
    SaveDefault,
    Close,
}

/// A level source on the chart, as the menu shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    pub id: String,
    pub label: String,
    pub kinds: Vec<String>,
}

/// What the menu is built from.
pub struct MenuInput<'a> {
    pub set: &'a StudySet,
    pub sources: &'a [SourceInfo],
    pub templates: &'a [String],
    /// The chart's product has RTH (else RTH choices are not offered).
    pub has_rth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Swatch {
    /// A theme study colour index.
    Study(u8),
    /// No swatch.
    None,
}

/// One line of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Header(&'static str),
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
    /// An "Add" catalog entry or a template action.
    Action {
        label: String,
        cmd: MenuCmd,
        /// Delete key / "delete" affordance (templates).
        delete: Option<MenuCmd>,
    },
}

impl Item {
    pub fn focusable(&self) -> bool {
        !matches!(self, Item::Header(_) | Item::Note(_))
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
}

fn reset_label(r: Reset) -> String {
    match r {
        Reset::Anchored { from } => format!("anchored {}", lines_core::UtcMillis(from)),
        r => r.label().to_owned(),
    }
}

fn vwap_label(spec: &VwapSpec) -> String {
    let mut s = format!("VWAP {}", reset_label(spec.reset));
    for k in 1..=3u8 {
        if spec.bands.has(k) {
            s.push_str(&format!(" ±{k}σ"));
        }
    }
    s
}

/// The Add catalog: `(label, command)`.
fn catalog(input: &MenuInput) -> Vec<(String, MenuCmd)> {
    let set = |a| MenuCmd::Set(a);
    let mut v = vec![(
        "VWAP · ETH session".to_owned(),
        set(StudyAction::AddVwap(Reset::Eth)),
    )];
    if input.has_rth {
        v.push((
            "VWAP · RTH session".into(),
            set(StudyAction::AddVwap(Reset::Rth)),
        ));
    }
    v.push((
        "VWAP · weekly".into(),
        set(StudyAction::AddVwap(Reset::Weekly)),
    ));
    v.push((
        "VWAP · monthly".into(),
        set(StudyAction::AddVwap(Reset::Monthly)),
    ));
    v.push((
        "Anchored VWAP (click a bar)".into(),
        MenuCmd::BeginAnchor(None),
    ));
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

fn controls(input: &MenuInput, r: &StudyRef, out: &mut Vec<Item>) {
    let set = |a| MenuCmd::Set(a);
    let mut c = |group: &'static str, label: String, on: bool, swatch: Swatch, cmd: MenuCmd| {
        out.push(Item::Control {
            group,
            label,
            on,
            swatch,
            cmd,
        })
    };
    match r {
        StudyRef::Vwap(i) => {
            let Some(IndicatorConfig::Vwap {
                spec,
                color,
                display,
            }) = input.set.indicators.get(*i)
            else {
                return;
            };
            let i = *i;
            let mut resets = vec![(Reset::Eth, "ETH")];
            if input.has_rth {
                resets.push((Reset::Rth, "RTH"));
            }
            resets.extend([(Reset::Weekly, "Week"), (Reset::Monthly, "Month")]);
            for (r, name) in resets {
                c(
                    "Reset",
                    name.into(),
                    spec.reset == r,
                    Swatch::None,
                    set(StudyAction::SetReset(i, r)),
                );
            }
            c(
                "Reset",
                "Anchored…".into(),
                matches!(spec.reset, Reset::Anchored { .. }),
                Swatch::None,
                MenuCmd::BeginAnchor(Some(i)),
            );
            for k in 1..=3u8 {
                c(
                    "Bands",
                    format!("{k}σ"),
                    spec.bands.has(k),
                    Swatch::None,
                    set(StudyAction::ToggleBand(i, k)),
                );
            }
            for k in 0..4u8 {
                c(
                    "Color",
                    format!("{}", k + 1),
                    *color == k,
                    Swatch::Study(k),
                    set(StudyAction::SetColor(i, k)),
                );
            }
            for (w, name) in [(1u8, "Thin"), (2, "Normal"), (3, "Thick")] {
                c(
                    "Width",
                    name.into(),
                    display.width == w,
                    Swatch::None,
                    set(StudyAction::SetWidth(i, w)),
                );
            }
            c(
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
                c(
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
                c(
                    "Session",
                    name.into(),
                    p.session == s,
                    Swatch::None,
                    set(StudyAction::SetProfileSession(s)),
                );
            }
            c(
                "Split",
                "Buy / sell".into(),
                p.split,
                Swatch::None,
                set(StudyAction::ToggleProfileSplit),
            );
            c(
                "Grouping",
                "Auto".into(),
                p.grouping.is_none(),
                Swatch::None,
                set(StudyAction::SetProfileGrouping(None)),
            );
            for g in lines_core::ladder::GROUPING_PRESETS {
                c(
                    "Grouping",
                    format!("{g}"),
                    p.grouping == Some(g),
                    Swatch::None,
                    set(StudyAction::SetProfileGrouping(Some(g))),
                );
            }
        }
        StudyRef::Levels(id) => {
            let Some(s) = input.sources.iter().find(|s| s.id == *id) else {
                return;
            };
            for k in &s.kinds {
                c(
                    "Kinds",
                    k.clone(),
                    input.set.levels.kind_shown(id, k),
                    Swatch::None,
                    set(StudyAction::ToggleLevelKind(id.clone(), k.clone())),
                );
            }
            c(
                "Labels",
                "Show labels".into(),
                input.set.levels.labels_shown(id),
                Swatch::None,
                set(StudyAction::ToggleLevelLabels(id.clone())),
            );
        }
    }
}

impl StudyMenu {
    /// The menu's lines, top to bottom.
    pub fn items(&self, input: &MenuInput) -> Vec<Item> {
        let mut v = vec![Item::Header("On this chart")];
        let mut studies: Vec<(StudyRef, String, Swatch, bool)> = Vec::new();
        for (i, c) in input.set.indicators.iter().enumerate() {
            let IndicatorConfig::Vwap {
                spec,
                color,
                display,
            } = c;
            studies.push((
                StudyRef::Vwap(i),
                vwap_label(spec),
                Swatch::Study(*color),
                display.hidden,
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
            }
        }
        v.push(Item::Header("Add"));
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
        v.push(Item::Header("Templates"));
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
        v.push(Item::Note("Changes apply to this chart.".into()));
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

    /// Keep the cursor on a focusable item after the items changed.
    pub fn clamp(&mut self, items: &[Item]) {
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
        let add = items.iter().position(|i| *i == Item::Header("Add"))?;
        items[add..]
            .iter()
            .position(|i| matches!(i, Item::Action { .. }))
            .map(|p| add + p)
    }

    /// The primary command of item `i` (a click on it, or Enter).
    pub fn activate(&mut self, items: &[Item], i: usize) -> Option<MenuCmd> {
        self.cursor = i;
        match items.get(i)? {
            Item::Study { r, .. } => Some(MenuCmd::Edit(r.clone())),
            Item::Control { cmd, .. } | Item::Action { cmd, .. } => Some(cmd.clone()),
            _ => None,
        }
    }

    /// The eye of a study row (Space).
    pub fn eye(&mut self, items: &[Item], i: usize) -> Option<MenuCmd> {
        self.cursor = i;
        match items.get(i)? {
            Item::Study { r, .. } => Some(MenuCmd::Set(StudyAction::ToggleHidden(r.clone()))),
            Item::Control { cmd, .. } => Some(cmd.clone()),
            _ => None,
        }
    }

    /// Delete on an item: remove a study, delete a template.
    pub fn delete(&mut self, items: &[Item], i: usize) -> Option<MenuCmd> {
        self.cursor = i;
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
            MenuCmd::Set(StudyAction::Remove(ref r)) if self.editing.as_ref() == Some(r) => {
                self.editing = None;
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
