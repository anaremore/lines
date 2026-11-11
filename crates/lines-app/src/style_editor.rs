//! The style editor (decision 0026): the one popover that edits a [`LineStyle`], a
//! [`FillStyle`] or a [`TextStyle`], one tab per part of what is styled. Pure (no GPUI): the
//! chart renders [`StyleEditor::rows`] (`crate::style_editor_view`) and feeds keys and clicks
//! back; every change comes back as an [`Outcome`] the owner applies (a drawing: one undo step
//! per change).
//!
//! Drawings ([`StyleEditor::for_drawing`], [`StyleEditor::drawing_style`]): a line tab (a
//! note's is its edge marker), a zone's fill, a note's text, and the drawing options (extend
//! left/right, the axis price tag) on the first tab. Studies ([`StyleEditor::for_study`],
//! built by `crate::study_style`): one tab per part (a VWAP's line, bands and band fill, ...);
//! a part may have its own color ([`Own`], offered first as [`Color::Own`]: a theme token, the
//! study's line, a level source's colors) and be optional (a "Show" checkbox: VWAP band
//! edges). Study colors are the theme's study colors, and a study part's opacity scales the
//! part's own translucency (`Theme::part_color`), so every part offers the line opacities.
//!
//! Keys: ↑↓ row, ←→ control, Enter or Space picks, Tab / Shift+Tab switch tabs, Esc closes (or
//! leaves the custom color picker first, putting back the color it opened with).
//!
//! Colors: the theme's swatches, then the last ten custom colors (Recent), then "Custom…",
//! which opens the picker (`crate::color_picker`: a saturation-brightness square, hue and
//! opacity sliders, a hex field and the eyedropper). The picker previews every change
//! ([`Outcome::Preview`]); Enter or a click anywhere else keeps it ([`Outcome::Changed`]).
//!
//! The readability guard ([`StyleEditor::warnings`]) never blocks: it words what
//! `lines_core::style::readability` found for the tab's color in each of the four theme and
//! palette combinations (a custom color is the same in all of them; a theme token follows).

use lines_core::drawings::{Style, Tool};
use lines_core::levels::{LabelAt, LabelColor};
use lines_core::style::{
    Color, FILL_OPACITIES, FillStyle, LINE_OPACITIES, LineStyle, LineType, MAX_WIDTH, MIN_WIDTH,
    Rgb, TextSize, TextStyle, readability,
};

use crate::theme::CUSTOM_SWATCHES;

/// One tab's style.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PartStyle {
    Line(LineStyle),
    Fill(FillStyle),
    Text(TextStyle),
}

impl PartStyle {
    fn color(&self) -> Color {
        match self {
            PartStyle::Line(l) => l.color,
            PartStyle::Fill(f) => f.color,
            PartStyle::Text(t) => t.color,
        }
    }

    fn set_color(&mut self, c: Color) {
        match self {
            PartStyle::Line(l) => l.color = c,
            PartStyle::Fill(f) => f.color = c,
            PartStyle::Text(t) => t.color = c,
        }
    }
}

/// What a study part's own color is (`Theme::own_color` shows it).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OwnToken {
    Up,
    Down,
    /// Up for buyers, down for sellers (footprint imbalances: each side keeps its place too).
    UpDown,
    /// The theme's neutral (dim text) shade: footprint cells.
    Neutral,
    /// The Volume study's neutral bars (`Theme::volume`).
    Volume,
    /// An external series line's theme role (`Theme::series`, by index).
    Series(u8),
    Text,
    ProfileBuy,
    ProfileSell,
    ProfilePoc,
    ProfileVa,
    /// The study's line color (a VWAP's bands and band fill).
    Line,
    /// The colors a level source gives its levels (decision 0022); the first one, for the
    /// swatch.
    Source(Option<Rgb>),
    /// This color (a level type's recommended one: a theme token or a fixed color): on the
    /// dark themes, then on the light ones.
    Is(Color, Color),
}

/// A study part's own color: what [`Color::Own`] is called and shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Own {
    /// "Theme", "Line color", "Source colors".
    pub name: &'static str,
    pub token: OwnToken,
}

/// A tab: what it styles and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub title: &'static str,
    pub style: PartStyle,
    /// Lines: whether line type and width are offered (a note's edge marker has neither).
    pub stroke: bool,
    /// Studies: the part's own color, offered first (`None`: drawings, a VWAP's line).
    pub own: Option<Own>,
    /// An optional part (drawn only when shown): whether it is (`None`: always drawn).
    pub shown: Option<bool>,
    /// Studies: one line under the part's colors saying how they apply (`None`: nothing).
    pub note: Option<&'static str>,
    /// The heading its tab is listed under (a level source's types: "Zones", "Lines"); `None`
    /// for every part: the tabs are one plain row.
    pub group: Option<&'static str>,
    /// A level type with zones: the fill between its edges, and the fill's own color.
    pub fill: Option<(FillStyle, Own)>,
    /// A level type: its labels' text color, and (a zone type) where they sit.
    pub label: Option<(LabelColor, Option<LabelAt>)>,
}

impl Part {
    /// A part always drawn, without an own color (drawings).
    pub fn new(title: &'static str, style: PartStyle, stroke: bool) -> Part {
        Part {
            title,
            style,
            stroke,
            own: None,
            shown: None,
            note: None,
            group: None,
            fill: None,
            label: None,
        }
    }
}

/// Drawing options shown on the first tab (`None`: not offered).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub extend_left: Option<bool>,
    pub extend_right: Option<bool>,
    pub axis_tag: Option<bool>,
    /// A level source's labels carry the price (`name : price`).
    pub label_price: Option<bool>,
}

/// One control in a row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Color(Color),
    /// Type a custom color (hex).
    Custom,
    LineType(LineType),
    Width(u8),
    Opacity(u8),
    TextSize(TextSize),
    /// A note's box behind its text (a checkbox).
    TextBox,
    ExtendLeft,
    ExtendRight,
    AxisTag,
    /// An optional part drawn (a checkbox).
    Shown,
    /// A level type's zone fill: its color, typing a custom one, its opacity.
    FillColor(Color),
    FillCustom,
    FillOpacity(u8),
    /// A level source's labels with the price (a checkbox).
    LabelPrice,
    /// A level type's label text color.
    LabelColor(LabelColor),
    /// A level type's label edge (a zone type's).
    LabelAt(LabelAt),
    SaveDefault,
}

/// A row of controls: its label and each control with whether it is the current value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub label: &'static str,
    pub controls: Vec<(Control, bool)>,
}

/// What a key or click did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to apply (moved, typed into the hex entry), or not the editor's key.
    Nothing,
    /// A style changed: apply it.
    Changed,
    /// The custom picker changed the color: show it, not yet kept (a drawing: no undo step
    /// until it is, [`Outcome::Changed`]).
    Preview,
    /// "Custom…": the host opens the picker on the color as the theme shows it
    /// ([`StyleEditor::open_custom`]).
    OpenCustom {
        fill: bool,
    },
    /// The picker's eyedropper: the host takes a color from the chart
    /// ([`StyleEditor::take_color`]); Esc cancels it.
    Dropper,
    /// "Save as default for …".
    SaveDefault,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleEditor {
    /// The popover's title, e.g. "Trend line style".
    pub title: String,
    pub parts: Vec<Part>,
    pub options: Options,
    /// Theme palette colors offered (`Color::Palette(0..palette)`).
    pub palette: u8,
    /// "Save as default for …" (none: not offered).
    pub default_label: Option<String>,
    pub tab: usize,
    pub row: usize,
    pub col: usize,
    /// The custom color picker, while open.
    pub custom: Option<crate::color_picker::Custom>,
    /// The eyedropper is armed (the chart shows the color under the pointer).
    pub dropper: bool,
    /// The last ten custom colors, newest first (settings `recent_colors`).
    pub recent: Vec<Rgb>,
    /// `recent` changed since the host saved it.
    pub recent_changed: bool,
    /// While a press drags in the picker's square or a slider: the warnings slot, fixed when
    /// the drag began, so nothing around the picker changes size or place under the pointer;
    /// the warnings show live inside it and the slot settles on release. `None` otherwise.
    pub warn_slot: Option<WarnSlot>,
    /// The current tab's style was just saved as the default (until the next change).
    pub saved: bool,
    /// A study's editor: theme colors are the study colors, every part offers the line
    /// opacities (a study part's opacity scales its own translucency).
    pub study: bool,
}

/// A tool's name in the editor: its title ("Zone style") and the plural of "Save as default
/// for zones".
pub fn tool_names(tool: Tool) -> (&'static str, &'static str) {
    match tool {
        Tool::HLine => ("Horizontal line", "horizontal lines"),
        Tool::HRay => ("Horizontal ray", "horizontal rays"),
        Tool::Trend => ("Trend line", "trend lines"),
        Tool::Ray => ("Ray", "rays"),
        Tool::Rect => ("Zone", "zones"),
        Tool::VLine => ("Vertical line", "vertical lines"),
        Tool::Note => ("Note", "notes"),
        Tool::Measure => ("Measure", "measures"),
    }
}

/// Whether a control is a checkbox (on/off), not one of a row's choices.
pub fn is_toggle(c: Control) -> bool {
    matches!(
        c,
        Control::TextBox
            | Control::ExtendLeft
            | Control::ExtendRight
            | Control::AxisTag
            | Control::Shown
            | Control::LabelPrice
    )
}

/// One theme and palette combination the guard checks a color in (decision 0026: a custom
/// color is the same in every theme, so it is checked in all four).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ThemeCtx {
    pub dark: bool,
    pub colorblind: bool,
    /// The palette `Color::Palette` indexes (drawings: the theme's drawing colors).
    pub palette: [Rgb; 4],
    /// `Color::Text`.
    pub text: Rgb,
    /// The chart's background, and a note box's.
    pub chart: Rgb,
    pub panel: Rgb,
    /// Colors with a meaning: (name, color), names as `Theme::meanings` gives them.
    pub meanings: [(&'static str, Rgb); 6],
}

impl ThemeCtx {
    pub fn resolve(&self, c: Color) -> Rgb {
        match c {
            Color::Palette(i) => self.palette[usize::from(i) % self.palette.len()],
            Color::Text => self.text,
            Color::Custom(rgb) => rgb,
            // Never checked (a part's own color is the theme's or its source's).
            Color::Own => self.palette[0],
        }
    }

    /// The combination in words, always naming the palette (a palette isn't a theme): "dark
    /// theme, standard palette", "light theme, colorblind palette".
    fn label(&self) -> &'static str {
        match (self.dark, self.colorblind) {
            (true, false) => "dark theme, standard palette",
            (false, false) => "light theme, standard palette",
            (true, true) => "dark theme, colorblind palette",
            (false, true) => "light theme, colorblind palette",
        }
    }
}

/// One readability warning: its words and, for a color with a meaning it looks like, that
/// color (shown as a small swatch beside the words).
#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub text: String,
    pub swatch: Option<Rgb>,
}

/// What a color that looks like a meaning may be misread as.
fn misread(name: &str) -> &'static str {
    match name {
        "up" => "up",
        "down" => "down",
        "last-price" => "the last price",
        "order target" => "an order target",
        "order stop" => "an order stop",
        _ => "a warning",
    }
}

/// Where a finding holds, from the contexts it holds in (`hit[i]` for `ctxs[i]`), always
/// naming themes and palettes: " in every theme and palette", " in the light theme, both
/// palettes", " in the colorblind palette, both themes", or the combinations named.
pub(crate) fn where_phrase(ctxs: &[ThemeCtx], hit: &[bool]) -> String {
    if hit.iter().all(|h| *h) {
        return " in every theme and palette".into();
    }
    let exactly = |f: fn(&ThemeCtx) -> bool| {
        ctxs.iter().filter(|c| f(c)).count() >= 2 && ctxs.iter().zip(hit).all(|(c, h)| *h == f(c))
    };
    if exactly(|c| c.dark) {
        return " in the dark theme, both palettes".into();
    }
    if exactly(|c| !c.dark) {
        return " in the light theme, both palettes".into();
    }
    if exactly(|c| c.colorblind) {
        return " in the colorblind palette, both themes".into();
    }
    if exactly(|c| !c.colorblind) {
        return " in the standard palette, both themes".into();
    }
    let names: Vec<&str> = ctxs
        .iter()
        .zip(hit)
        .filter(|(_, h)| **h)
        .map(|(c, _)| c.label())
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => format!(" in the {one}"),
        [init @ .., last] => format!(" in the {}; and the {last}", init.join("; the ")),
    }
}

/// The warnings slot a picker drag holds (logical px at 100 %, whole, rounded up): `drawn`,
/// the box's painted height, is what is drawn (clipped); `room`, the layout's estimate at the
/// press (`style_editor_view::warnings_height`), is what the editor's height and place count,
/// so neither the editor nor anything in it moves when the press starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WarnSlot {
    pub drawn: u32,
    pub room: u32,
}

impl StyleEditor {
    pub fn new(title: String, parts: Vec<Part>, palette: u8) -> StyleEditor {
        StyleEditor {
            title,
            parts,
            options: Options::default(),
            palette,
            default_label: None,
            tab: 0,
            row: 0,
            col: 0,
            custom: None,
            dropper: false,
            recent: Vec::new(),
            recent_changed: false,
            warn_slot: None,
            saved: false,
            study: false,
        }
    }

    // ---- studies ------------------------------------------------------------------------

    /// A study's editor titled `title` with `parts` (from `crate::study_style`): study
    /// colors, no "save as default" (the studies menu saves whole study sets).
    pub fn for_study(title: String, parts: Vec<Part>) -> StyleEditor {
        let mut e = StyleEditor::new(title, parts, 4);
        e.study = true;
        e.focus_current();
        e
    }

    /// The current tab's own color's name ("Theme"), for the color row's name line.
    pub fn own_name(&self) -> &'static str {
        self.parts
            .get(self.tab)
            .and_then(|p| p.own)
            .map_or("Default", |o| o.name)
    }

    /// The current tab's zone fill's own color's name.
    pub fn fill_own_name(&self) -> &'static str {
        self.parts
            .get(self.tab)
            .and_then(|p| p.fill)
            .map_or("Default", |(_, o)| o.name)
    }

    // ---- drawings -----------------------------------------------------------------------

    /// The editor for a drawing of `tool` styled `s`: tabs for the parts the tool has.
    pub fn for_drawing(tool: Tool, s: &Style) -> StyleEditor {
        let line = Part::new(
            if tool.has_stroke() { "Line" } else { "Marker" },
            PartStyle::Line(s.line),
            tool.has_stroke(),
        );
        let mut parts = Vec::with_capacity(2);
        if tool.has_text() {
            parts.push(Part::new("Text", PartStyle::Text(s.text), false));
        }
        parts.push(line);
        if tool.has_fill() {
            parts.push(Part::new("Fill", PartStyle::Fill(s.fill), false));
        }
        let (l, r) = tool.extends();
        let (name, plural) = tool_names(tool);
        let mut e = StyleEditor::new(format!("{name} style"), parts, Style::COLORS);
        e.options = Options {
            extend_left: l.then_some(s.extend_left),
            extend_right: r.then_some(s.extend_right),
            axis_tag: tool.has_axis_tag().then_some(s.axis_tag),
            label_price: None,
        };
        e.default_label = Some(format!("Save as default for {plural}"));
        e.focus_current();
        e
    }

    /// Focus the first row's current value (an editor opened or a tab switched starts on what
    /// is set, so the color row names the current color).
    fn focus_current(&mut self) {
        self.row = 0;
        self.col = self
            .rows()
            .first()
            .and_then(|r| r.controls.iter().position(|(_, on)| *on))
            .unwrap_or(0);
    }

    /// `base` with this editor's parts and options (what it doesn't edit stays).
    pub fn drawing_style(&self, base: &Style) -> Style {
        let mut s = *base;
        for p in &self.parts {
            match p.style {
                PartStyle::Line(l) => s.line = l,
                PartStyle::Fill(f) => s.fill = f,
                PartStyle::Text(t) => s.text = t,
            }
        }
        let o = self.options;
        s.extend_left = o.extend_left.unwrap_or(s.extend_left);
        s.extend_right = o.extend_right.unwrap_or(s.extend_right);
        s.axis_tag = o.axis_tag.unwrap_or(s.axis_tag);
        s.normalised()
    }

    /// Show `s` (the drawing changed underneath: an undo, another chart), keeping the tab and
    /// focus.
    pub fn sync_drawing(&mut self, s: &Style) {
        for p in &mut self.parts {
            p.style = match p.style {
                PartStyle::Line(_) => PartStyle::Line(s.line),
                PartStyle::Fill(_) => PartStyle::Fill(s.fill),
                PartStyle::Text(_) => PartStyle::Text(s.text),
            };
        }
        let o = &mut self.options;
        o.extend_left = o.extend_left.map(|_| s.extend_left);
        o.extend_right = o.extend_right.map(|_| s.extend_right);
        o.axis_tag = o.axis_tag.map(|_| s.axis_tag);
    }

    // ---- rows -----------------------------------------------------------------------------

    fn part(&self) -> &Part {
        &self.parts[self.tab.min(self.parts.len().saturating_sub(1))]
    }

    fn color_row(&self, current: Color, text: bool, own: bool) -> Row {
        self.colors(current, text, own, false)
    }

    /// A color row: the tab's color, or (`fill`) its zone fill's ("Fill").
    fn colors(&self, current: Color, text: bool, own: bool, fill: bool) -> Row {
        let chip = |c: Color| {
            if fill {
                Control::FillColor(c)
            } else {
                Control::Color(c)
            }
        };
        let mut controls = Vec::with_capacity(12);
        if own {
            controls.push((chip(Color::Own), current == Color::Own));
        }
        if text {
            controls.push((chip(Color::Text), current == Color::Text));
        }
        for i in 0..self.palette {
            let c = Color::Palette(i);
            // The first theme drawing color is the theme's text color: text offers it once,
            // as "Text" (unless a note already uses the palette entry).
            if text && i == 0 && current != c {
                continue;
            }
            controls.push((chip(c), current == c));
        }
        for (hex, _) in CUSTOM_SWATCHES {
            let c = Color::Custom(Rgb::hex(hex));
            controls.push((chip(c), current == c));
        }
        // A custom color not among the swatches shows as its own chip, unless it is the
        // line color and a recent one (checked and named on the Recent row, which is the line
        // color's): a fill keeps its chip, so the Fill row always checks what it holds.
        if let Color::Custom(rgb) = current
            && !controls.iter().any(|(_, on)| *on)
            && (fill || !self.recent.contains(&rgb))
        {
            controls.push((chip(current), true));
        }
        let typing = self.custom.as_ref().is_some_and(|c| c.fill == fill);
        let custom = if fill {
            Control::FillCustom
        } else {
            Control::Custom
        };
        controls.push((custom, typing));
        Row {
            label: if fill { "Fill" } else { "Color" },
            controls,
        }
    }

    /// The current tab's rows (the drawing options and "save as default" included).
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::with_capacity(8);
        if self.parts.is_empty() {
            return rows;
        }
        let part = self.part();
        let own = part.own.is_some();
        if let Some(on) = part.shown {
            rows.push(Row {
                label: "Show",
                controls: vec![(Control::Shown, on)],
            });
        }
        match part.style {
            PartStyle::Line(l) => {
                rows.push(self.color_row(l.color, false, own));
                if part.stroke {
                    rows.push(Row {
                        label: "Line",
                        controls: LineType::ALL
                            .iter()
                            .map(|&t| (Control::LineType(t), t == l.line))
                            .collect(),
                    });
                    rows.push(Row {
                        label: "Width",
                        controls: (MIN_WIDTH..=MAX_WIDTH)
                            .map(|w| (Control::Width(w), w == l.width))
                            .collect(),
                    });
                }
                rows.push(Row {
                    label: "Opacity",
                    controls: opacity_controls(&LINE_OPACITIES, l.opacity, Control::Opacity),
                });
            }
            PartStyle::Fill(f) => {
                rows.push(self.color_row(f.color, false, own));
                // A study part's opacity scales its own translucency: the line choices.
                let choices: &[u8] = if self.study {
                    &LINE_OPACITIES
                } else {
                    &FILL_OPACITIES
                };
                rows.push(Row {
                    label: "Opacity",
                    controls: opacity_controls(choices, f.opacity, Control::Opacity),
                });
            }
            PartStyle::Text(t) => {
                rows.push(Row {
                    label: "Size",
                    controls: TextSize::ALL
                        .iter()
                        .map(|&s| (Control::TextSize(s), s == t.size))
                        .collect(),
                });
                rows.push(self.color_row(t.color, true, own));
                rows.push(Row {
                    label: "Box",
                    controls: vec![(Control::TextBox, t.background)],
                });
            }
        }
        if let Some((f, _)) = part.fill {
            rows.push(self.colors(f.color, false, true, true));
            rows.push(Row {
                label: "Fill opacity",
                controls: opacity_controls(&LINE_OPACITIES, f.opacity, Control::FillOpacity),
            });
        }
        if let Some((color, at)) = part.label {
            rows.push(Row {
                label: "Label color",
                controls: LabelColor::ALL
                    .iter()
                    .map(|&c| (Control::LabelColor(c), c == color))
                    .collect(),
            });
            if let Some(at) = at {
                rows.push(Row {
                    label: "Label at",
                    controls: LabelAt::ALL
                        .iter()
                        .map(|&a| (Control::LabelAt(a), a == at))
                        .collect(),
                });
            }
        }
        if self.tab == 0 {
            let o = self.options;
            let mut extend = Vec::with_capacity(2);
            if let Some(on) = o.extend_left {
                extend.push((Control::ExtendLeft, on));
            }
            if let Some(on) = o.extend_right {
                extend.push((Control::ExtendRight, on));
            }
            if !extend.is_empty() {
                rows.push(Row {
                    label: "Extend",
                    controls: extend,
                });
            }
            if let Some(on) = o.axis_tag {
                rows.push(Row {
                    label: "Price tag",
                    controls: vec![(Control::AxisTag, on)],
                });
            }
            if let Some(on) = o.label_price {
                rows.push(Row {
                    label: "Labels",
                    controls: vec![(Control::LabelPrice, on)],
                });
            }
        }
        if self.default_label.is_some() {
            rows.push(Row {
                label: "",
                controls: vec![(Control::SaveDefault, self.saved)],
            });
        }
        // The last custom colors, right under the theme's.
        if !self.recent.is_empty()
            && let Some(i) = rows.iter().position(|r| r.label == "Color")
        {
            let current = part.style.color();
            rows.insert(
                i + 1,
                Row {
                    label: "Recent",
                    controls: self
                        .recent
                        .iter()
                        .map(|&c| {
                            (
                                Control::Color(Color::Custom(c)),
                                current == Color::Custom(c),
                            )
                        })
                        .collect(),
                },
            );
        }
        rows
    }

    /// The control under the keyboard focus.
    pub fn focused(&self, rows: &[Row]) -> Option<Control> {
        rows.get(self.row)
            .and_then(|r| r.controls.get(self.col))
            .map(|(c, _)| *c)
    }

    fn clamp(&mut self, rows: &[Row]) {
        self.row = self.row.min(rows.len().saturating_sub(1));
        let n = rows.get(self.row).map_or(0, |r| r.controls.len());
        self.col = self.col.min(n.saturating_sub(1));
    }

    // ---- input ----------------------------------------------------------------------------

    /// A click on control `col` of row `row`.
    pub fn click(&mut self, row: usize, col: usize) -> Outcome {
        // The control clicked, in the rows as drawn: keeping the picker's color may add a
        // Recent row, which moves the rows under it.
        self.row = row;
        self.col = col;
        let rows = self.rows();
        self.clamp(&rows);
        let clicked = self.focused(&rows);
        let label = rows.get(self.row).map(|r| r.label);
        // A click anywhere else keeps the picker's color and closes it ("Custom…" opens it
        // again).
        let kept = self.close_custom(true);
        let rows = self.rows();
        if let Some(c) = clicked
            && let Some((r, k)) = rows.iter().enumerate().find_map(|(r, row)| {
                (Some(row.label) == label)
                    .then(|| row.controls.iter().position(|(x, _)| *x == c))
                    .flatten()
                    .map(|k| (r, k))
            })
        {
            self.row = r;
            self.col = k;
        }
        self.clamp(&rows);
        let out = match clicked {
            Some(c) => self.activate(c),
            None => Outcome::Nothing,
        };
        if kept && out == Outcome::Nothing {
            Outcome::Changed
        } else {
            out
        }
    }

    /// A click on tab `i`. `true`: the picker was open and its color is kept (apply it).
    pub fn select_tab(&mut self, i: usize) -> bool {
        if i < self.parts.len() && i != self.tab {
            let kept = self.close_custom(true);
            self.tab = i;
            self.saved = false;
            self.focus_current();
            return kept;
        }
        false
    }

    // ---- the custom color picker (crate::color_picker) -----------------------------------

    fn tab_ix(&self) -> usize {
        self.tab.min(self.parts.len().saturating_sub(1))
    }

    /// The tab's color (or its zone fill's), for the host to resolve when it opens the picker.
    pub fn picked_color(&self, fill: bool) -> Option<Color> {
        self.picked(fill).map(|(c, _)| c)
    }

    /// The color and opacity the picker edits: the tab's, or its zone fill's.
    fn picked(&self, fill: bool) -> Option<(Color, Option<u8>)> {
        let p = self.parts.get(self.tab_ix())?;
        if fill {
            return p.fill.map(|(f, _)| (f.color, Some(f.opacity)));
        }
        Some(match p.style {
            PartStyle::Line(l) => (l.color, Some(l.opacity)),
            PartStyle::Fill(f) => (f.color, Some(f.opacity)),
            PartStyle::Text(t) => (t.color, None),
        })
    }

    /// Open the picker on the tab's color (or zone fill's), `rgb` as the theme shows it.
    pub fn open_custom(&mut self, fill: bool, rgb: Rgb) {
        if let Some((c, o)) = self.picked(fill) {
            self.custom = Some(crate::color_picker::Custom::open(fill, c, rgb, o));
            self.dropper = false;
            self.warn_slot = None;
        }
    }

    /// Put `c` and `opacity` on the tab (or its zone fill).
    fn put(&mut self, fill: bool, c: Color, opacity: Option<u8>) {
        if fill {
            self.set_fill_color(c);
        } else {
            self.set_color(c);
        }
        let tab = self.tab_ix();
        if let (Some(o), Some(p)) = (opacity, self.parts.get_mut(tab)) {
            match (fill, &mut p.style, &mut p.fill) {
                (true, _, Some((f, _))) => f.opacity = o,
                (false, PartStyle::Line(l), _) => l.opacity = o,
                (false, PartStyle::Fill(f), _) => f.opacity = o,
                _ => {}
            }
        }
        self.saved = false;
    }

    /// Apply what the picker holds now (a preview).
    fn apply_custom(&mut self) -> Outcome {
        let Some(p) = self.custom.as_ref() else {
            return Outcome::Nothing;
        };
        let (fill, c, o) = (p.fill, Color::Custom(p.color()), p.opacity);
        self.put(fill, c, o);
        Outcome::Preview
    }

    /// Close the picker: keep its color (remembered among the recent ones) or put back the
    /// one it opened with. `true` if it was open.
    fn close_custom(&mut self, keep: bool) -> bool {
        let Some(p) = self.custom.take() else {
            return false;
        };
        self.dropper = false;
        self.warn_slot = None;
        if keep {
            if let Some((Color::Custom(rgb), _)) = self.picked(p.fill)
                && (Color::Custom(rgb), p.opacity) != p.initial
            {
                crate::color_picker::remember(&mut self.recent, rgb);
                self.recent_changed = true;
            }
        } else {
            let (c, o) = p.initial;
            self.put(p.fill, c, o);
        }
        true
    }

    /// The editor closes some other way than Esc (a click outside, another popover): the
    /// picker's color is kept.
    pub fn keep_custom(&mut self) -> Outcome {
        if self.close_custom(true) {
            Outcome::Changed
        } else {
            Outcome::Nothing
        }
    }

    /// The square at fractions `x` (saturation) and `y` (0 = bright) of its size.
    pub fn custom_square(&mut self, x: f32, y: f32) -> Outcome {
        self.custom_event(|p| p.set_square(x, y))
    }

    /// The hue slider at fraction `x`.
    pub fn custom_hue(&mut self, x: f32) -> Outcome {
        self.custom_event(|p| p.set_hue(x))
    }

    /// The opacity slider at fraction `x`.
    pub fn custom_opacity(&mut self, x: f32) -> Outcome {
        self.custom_event(|p| p.set_opacity(x))
    }

    /// Focus a zone of the picker (a click on it).
    pub fn custom_zone(&mut self, z: crate::color_picker::Zone) -> Outcome {
        // Any other use of the picker ends the eyedropper.
        self.dropper = false;
        if let Some(p) = self.custom.as_mut() {
            p.zone = z;
        }
        Outcome::Nothing
    }

    /// The eyedropper's button (Esc while armed ends it: [`StyleEditor::key`]).
    /// Pressed again while armed, it cancels (a way out by pointer as well as Esc).
    pub fn arm_dropper(&mut self) -> Outcome {
        if self.dropper {
            self.dropper = false;
            return Outcome::Nothing;
        }
        if self.custom.is_some() {
            self.dropper = true;
            return Outcome::Dropper;
        }
        Outcome::Nothing
    }

    /// The eyedropper took `rgb` from the chart: the picker shows it (a preview).
    pub fn take_color(&mut self, rgb: Rgb) -> Outcome {
        self.dropper = false;
        self.custom_event(|p| p.set_color(rgb))
    }

    fn custom_event(
        &mut self,
        f: impl FnOnce(&mut crate::color_picker::Custom) -> crate::color_picker::Event,
    ) -> Outcome {
        use crate::color_picker::Event;
        // Any other use of the picker ends the eyedropper.
        self.dropper = false;
        let Some(p) = self.custom.as_mut() else {
            return Outcome::Nothing;
        };
        match f(p) {
            Event::Nothing => Outcome::Nothing,
            Event::Changed => self.apply_custom(),
            Event::Keep => {
                self.close_custom(true);
                Outcome::Changed
            }
            Event::Revert => {
                self.close_custom(false);
                Outcome::Changed
            }
            Event::Dropper => self.arm_dropper(),
        }
    }

    /// A key. `text`: the typed character (no Ctrl/Alt/Cmd).
    pub fn key(&mut self, key: &str, shift: bool, text: Option<&str>) -> Outcome {
        if self.dropper {
            // Armed: Esc ends it (the chart takes the click that picks).
            if key == "escape" {
                self.dropper = false;
            }
            return Outcome::Nothing;
        }
        if self.custom.is_some() {
            if key != "tab" {
                return self.custom_event(|p| p.key(key, shift, text));
            }
            // Tab keeps the color: to the next tab, or (one tab) just closing the picker.
            if self.parts.len() <= 1 {
                return self.keep_custom();
            }
        }
        let rows = self.rows();
        match key {
            "escape" => return Outcome::Close,
            "tab" if !self.parts.is_empty() => {
                let n = self.parts.len();
                let next = if shift {
                    (self.tab + n - 1) % n
                } else {
                    (self.tab + 1) % n
                };
                if self.select_tab(next) {
                    return Outcome::Changed;
                }
            }
            "up" => self.row = self.row.saturating_sub(1),
            "down" => self.row = (self.row + 1).min(rows.len().saturating_sub(1)),
            "left" => self.col = self.col.saturating_sub(1),
            "right" => self.col += 1,
            "home" => self.col = 0,
            "end" => self.col = usize::MAX,
            "enter" | "space" => {
                if let Some(c) = self.focused(&rows) {
                    return self.activate(c);
                }
            }
            _ => {}
        }
        let rows = self.rows();
        self.clamp(&rows);
        Outcome::Nothing
    }

    fn activate(&mut self, c: Control) -> Outcome {
        let tab = self.tab.min(self.parts.len().saturating_sub(1));
        let changed = match c {
            Control::Color(c) => return self.set_color(c),
            Control::FillColor(c) => return self.set_fill_color(c),
            Control::FillCustom => return Outcome::OpenCustom { fill: true },
            Control::FillOpacity(o) => match self.parts.get_mut(tab) {
                Some(Part {
                    fill: Some((f, _)), ..
                }) if f.opacity != o => {
                    f.opacity = o;
                    true
                }
                _ => false,
            },
            Control::LabelPrice => flip(&mut self.options.label_price),
            Control::LabelColor(c) => match self.parts.get_mut(tab) {
                Some(Part {
                    label: Some((color, _)),
                    ..
                }) if *color != c => {
                    *color = c;
                    true
                }
                _ => false,
            },
            Control::LabelAt(a) => match self.parts.get_mut(tab) {
                Some(Part {
                    label: Some((_, Some(at))),
                    ..
                }) if *at != a => {
                    *at = a;
                    true
                }
                _ => false,
            },
            Control::Custom => return Outcome::OpenCustom { fill: false },
            Control::SaveDefault => {
                self.saved = true;
                return Outcome::SaveDefault;
            }
            Control::ExtendLeft => flip(&mut self.options.extend_left),
            Control::ExtendRight => flip(&mut self.options.extend_right),
            Control::AxisTag => flip(&mut self.options.axis_tag),
            Control::Shown => match self.parts.get_mut(tab) {
                Some(p) => flip(&mut p.shown),
                None => false,
            },
            other => {
                let Some(p) = self.parts.get_mut(tab) else {
                    return Outcome::Nothing;
                };
                let before = p.style;
                match (&mut p.style, other) {
                    (PartStyle::Line(l), Control::LineType(t)) => l.line = t,
                    (PartStyle::Line(l), Control::Width(w)) => l.width = w,
                    (PartStyle::Line(l), Control::Opacity(o)) => l.opacity = o,
                    (PartStyle::Fill(f), Control::Opacity(o)) => f.opacity = o,
                    (PartStyle::Text(t), Control::TextSize(s)) => t.size = s,
                    (PartStyle::Text(t), Control::TextBox) => t.background = !t.background,
                    _ => {}
                }
                let changed = p.style != before;
                if changed {
                    show(p);
                }
                changed
            }
        };
        if changed {
            self.saved = false;
            Outcome::Changed
        } else {
            Outcome::Nothing
        }
    }

    /// Set the current tab's color. A drawing's line's new color carries to a fill that
    /// matched its old one (a zone recolored keeps its fill in step).
    fn set_color(&mut self, c: Color) -> Outcome {
        let tab = self.tab.min(self.parts.len().saturating_sub(1));
        let Some(p) = self.parts.get_mut(tab) else {
            return Outcome::Nothing;
        };
        let old = p.style.color();
        if old == c {
            return Outcome::Nothing;
        }
        let was_line = matches!(p.style, PartStyle::Line(_));
        p.style.set_color(c);
        show(p);
        // Drawings only: a study's parts follow their line through their own color instead.
        if was_line && !self.study {
            for q in &mut self.parts {
                if let PartStyle::Fill(f) = &mut q.style
                    && f.color == old
                {
                    f.color = c;
                }
            }
        }
        self.saved = false;
        Outcome::Changed
    }

    /// Set the current tab's zone fill color.
    fn set_fill_color(&mut self, c: Color) -> Outcome {
        let tab = self.tab.min(self.parts.len().saturating_sub(1));
        match self.parts.get_mut(tab) {
            Some(Part {
                fill: Some((f, _)), ..
            }) if f.color != c => {
                f.color = c;
                self.saved = false;
                Outcome::Changed
            }
            _ => Outcome::Nothing,
        }
    }

    // ---- readability guard ----------------------------------------------------------------

    /// The current tab's readability warnings, in words: colors with a meaning it looks like
    /// first (with that color, for a swatch), then low contrast; each says in which theme or
    /// palette it holds. `ctxs`: the four theme and palette combinations. Never blocks.
    pub fn warnings(&self, ctxs: &[ThemeCtx]) -> Vec<Warning> {
        let mut out = Vec::new();
        let Some(part) = self.parts.get(self.tab) else {
            return out;
        };
        if part.style.color() == Color::Own {
            // The theme's own color for the part (tested with the theme), or the colors a
            // level source chose (decision 0022): nothing the user picked to check.
            return out;
        }
        let (color, opacity, text, contrast, boxed) = match part.style {
            PartStyle::Line(l) => (l.color, l.opacity, false, true, false),
            // A fill is translucent area behind data: only its hue can mislead.
            PartStyle::Fill(f) => (f.color, 100, false, false, false),
            PartStyle::Text(t) => (t.color, 100, true, true, t.background),
        };
        let found: Vec<_> = ctxs
            .iter()
            .map(|c| {
                let bg = if boxed { c.panel } else { c.chart };
                readability(c.resolve(color), opacity, bg, &c.meanings, text)
            })
            .collect();
        // Looks like a meaning: one warning per meaning, in the order first found.
        let mut names: Vec<&'static str> = Vec::with_capacity(4);
        for r in &found {
            if let Some(n) = r.near
                && !names.contains(&n)
            {
                names.push(n);
            }
        }
        for name in names {
            let hit: Vec<bool> = found.iter().map(|r| r.near == Some(name)).collect();
            let swatch = ctxs
                .iter()
                .zip(&hit)
                .find(|(_, h)| **h)
                .and_then(|(c, _)| c.meanings.iter().find(|(n, _)| *n == name))
                .map(|(_, rgb)| *rgb);
            out.push(Warning {
                text: format!(
                    "Looks like the {name} color{}; may be misread as {}.",
                    where_phrase(ctxs, &hit),
                    misread(name)
                ),
                swatch,
            });
        }
        if contrast {
            // One figure per theme (the worst), naming the palettes it holds in (a theme
            // color differs between palettes).
            let mut low: Vec<(String, f64)> = Vec::with_capacity(2);
            for dark in [true, false] {
                let hits: Vec<(bool, f64)> = ctxs
                    .iter()
                    .zip(&found)
                    .filter(|(c, _)| c.dark == dark)
                    .filter_map(|(c, r)| r.low_contrast.map(|k| (c.colorblind, k)))
                    .collect();
                let Some(worst) = hits.iter().map(|h| h.1).min_by(f64::total_cmp) else {
                    continue;
                };
                let palettes = ctxs.iter().filter(|c| c.dark == dark).count();
                let palette = match hits.as_slice() {
                    [(cb, _)] if palettes > 1 => {
                        if *cb {
                            "colorblind palette"
                        } else {
                            "standard palette"
                        }
                    }
                    _ => "both palettes",
                };
                let theme = if dark { "dark" } else { "light" };
                low.push((format!("{theme} theme, {palette}"), worst));
            }
            if !low.is_empty() {
                let against = if boxed { " against its box" } else { "" };
                let what = if text {
                    "text needs 4.5:1"
                } else {
                    "lines need 3:1"
                };
                let themes: Vec<String> = low
                    .iter()
                    .map(|(n, r)| format!("the {n} ({r:.1}:1)"))
                    .collect();
                let mut words =
                    format!("Low contrast{against} in {}; {what}.", themes.join(" and "));
                if text && !boxed {
                    words.push_str(" The note shows its box there.");
                }
                out.push(Warning {
                    text: words,
                    swatch: None,
                });
            }
        }
        out
    }
}

/// Styling an optional part that isn't shown shows it (the change is seen at once).
fn show(p: &mut Part) {
    if p.shown == Some(false) {
        p.shown = Some(true);
    }
}

fn flip(o: &mut Option<bool>) -> bool {
    match o {
        Some(v) => {
            *v = !*v;
            true
        }
        None => false,
    }
}

/// A color's name: a theme drawing color's from `names` (the theme's, e.g. "Lime"), a
/// starting swatch's own, else "Custom #RRGGBB".
pub fn color_name(c: Color, names: &[&'static str]) -> String {
    match c {
        Color::Text => "Text".to_owned(),
        Color::Own => "Default".to_owned(),
        Color::Palette(i) => names
            .get(usize::from(i) % names.len().max(1))
            .map_or_else(|| format!("Theme {}", i + 1), |n| (*n).to_owned()),
        Color::Custom(rgb) => match CUSTOM_SWATCHES.iter().find(|(h, _)| Rgb::hex(*h) == rgb) {
            Some((_, n)) => (*n).to_owned(),
            None => {
                let mut hex = String::with_capacity(7);
                let _ = rgb.write_hex(&mut hex);
                format!("Custom {}", hex.to_ascii_uppercase())
            }
        },
    }
}

/// An opacity row's chips, the current one checked: one of `choices`, else (a source's or a
/// saved value the editor doesn't offer) a "Custom NN%" chip after them, checked, so the row
/// always shows what is drawn.
fn opacity_controls(choices: &[u8], current: u8, make: fn(u8) -> Control) -> Vec<(Control, bool)> {
    let mut out: Vec<(Control, bool)> = choices.iter().map(|&o| (make(o), o == current)).collect();
    if !choices.contains(&current) {
        out.push((make(current), true));
    }
    out
}

/// An opacity no row offers as a choice: shown as "Custom NN%".
fn custom_opacity(o: u8) -> bool {
    !LINE_OPACITIES.contains(&o) && !FILL_OPACITIES.contains(&o)
}

/// What a control reads (chips and checkboxes; a swatch's name shows beside its row's label,
/// [`color_name`]).
pub fn control_label(c: Control, names: &[&'static str]) -> String {
    match c {
        Control::Color(c) | Control::FillColor(c) => color_name(c, names),
        Control::Custom | Control::FillCustom => "Custom…".to_owned(),
        Control::FillOpacity(o) if !LINE_OPACITIES.contains(&o) => format!("Custom {o}%"),
        Control::FillOpacity(o) => format!("{o}%"),
        Control::LabelPrice => "With the price".to_owned(),
        Control::LabelColor(c) => c.label().to_owned(),
        Control::LabelAt(a) => a.label().to_owned(),
        Control::LineType(t) => t.label().to_owned(),
        Control::Width(w) => format!("{w} px"),
        Control::Opacity(o) if custom_opacity(o) => format!("Custom {o}%"),
        Control::Opacity(o) => format!("{o}%"),
        Control::TextSize(s) => s.label().to_owned(),
        Control::TextBox => "Behind the text".to_owned(),
        Control::ExtendLeft => "Left".to_owned(),
        Control::ExtendRight => "Right".to_owned(),
        Control::AxisTag => "On the price axis".to_owned(),
        Control::Shown => "Drawn on the chart".to_owned(),
        Control::SaveDefault => "Save as default".to_owned(),
    }
}

#[cfg(test)]
mod tests;
