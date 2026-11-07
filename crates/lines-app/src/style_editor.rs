//! The style editor (decision 0026): the one popover that edits a [`LineStyle`], a
//! [`FillStyle`] or a [`TextStyle`], one tab per part of what is styled. Pure (no GPUI): the
//! chart renders [`StyleEditor::rows`] (`crate::style_editor_view`) and feeds keys and clicks
//! back; every change comes back as an [`Outcome`] the owner applies (a drawing: one undo step
//! per change).
//!
//! Drawings use it today ([`StyleEditor::for_drawing`], [`StyleEditor::drawing_style`]): a
//! line tab (a note's is its edge marker), a zone's fill, a note's text, and the drawing
//! options (extend left/right, the axis price tag) on the first tab. A study's editor (PR 2)
//! builds its own [`Part`]s (a VWAP's line, each band's line and fill, …) and reads them back
//! the same way; nothing here knows about drawings except those two functions.
//!
//! Keys: ↑↓ row, ←→ control, Enter or Space picks, Tab / Shift+Tab switch tabs, Esc closes (or
//! leaves the custom color entry first). A custom color is typed as six hex digits.
//!
//! The readability guard ([`StyleEditor::warning`]) never blocks: it words what
//! `lines_core::style::readability` found for the tab's color.

use lines_core::drawings::{Style, Tool};
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

/// A tab: what it styles and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub title: &'static str,
    pub style: PartStyle,
    /// Lines: whether line type and width are offered (a note's edge marker has neither).
    pub stroke: bool,
}

/// Drawing options shown on the first tab (`None`: not offered).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub extend_left: Option<bool>,
    pub extend_right: Option<bool>,
    pub axis_tag: Option<bool>,
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
    Background(bool),
    ExtendLeft,
    ExtendRight,
    AxisTag,
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
    /// "Save as default for …".
    SaveDefault,
    Close,
}

/// Longest hex entry: `rrggbb`.
const HEX_LEN: usize = 6;

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
    /// A custom color being typed (hex digits so far).
    pub hex: Option<String>,
    /// The current tab's style was just saved as the default (until the next change).
    pub saved: bool,
}

/// A capitalized label: "trend line" → "Trend line".
fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
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
            hex: None,
            saved: false,
        }
    }

    // ---- drawings -----------------------------------------------------------------------

    /// The editor for a drawing of `tool` styled `s`: tabs for the parts the tool has.
    pub fn for_drawing(tool: Tool, s: &Style) -> StyleEditor {
        let line = Part {
            title: if tool.has_stroke() { "Line" } else { "Marker" },
            style: PartStyle::Line(s.line),
            stroke: tool.has_stroke(),
        };
        let mut parts = Vec::with_capacity(2);
        if tool.has_text() {
            parts.push(Part {
                title: "Text",
                style: PartStyle::Text(s.text),
                stroke: false,
            });
        }
        parts.push(line);
        if tool.has_fill() {
            parts.push(Part {
                title: "Fill",
                style: PartStyle::Fill(s.fill),
                stroke: false,
            });
        }
        let (l, r) = tool.extends();
        let mut e = StyleEditor::new(
            format!("{} style", capitalized(tool.label())),
            parts,
            Style::COLORS,
        );
        e.options = Options {
            extend_left: l.then_some(s.extend_left),
            extend_right: r.then_some(s.extend_right),
            axis_tag: tool.has_axis_tag().then_some(s.axis_tag),
        };
        e.default_label = Some(format!("Save as default for {}s", tool.label()));
        e
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

    fn color_row(&self, current: Color, text: bool) -> Row {
        let mut controls = Vec::with_capacity(16);
        if text {
            controls.push((Control::Color(Color::Text), current == Color::Text));
        }
        for i in 0..self.palette {
            let c = Color::Palette(i);
            controls.push((Control::Color(c), current == c));
        }
        for hex in CUSTOM_SWATCHES {
            let c = Color::Custom(Rgb::hex(hex));
            controls.push((Control::Color(c), current == c));
        }
        // A custom color not among the swatches shows as its own chip.
        if matches!(current, Color::Custom(_)) && !controls.iter().any(|(_, on)| *on) {
            controls.push((Control::Color(current), true));
        }
        controls.push((Control::Custom, self.hex.is_some()));
        Row {
            label: "Color",
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
        match part.style {
            PartStyle::Line(l) => {
                rows.push(self.color_row(l.color, false));
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
                    controls: LINE_OPACITIES
                        .iter()
                        .map(|&o| (Control::Opacity(o), o == l.opacity))
                        .collect(),
                });
            }
            PartStyle::Fill(f) => {
                rows.push(self.color_row(f.color, false));
                rows.push(Row {
                    label: "Opacity",
                    controls: FILL_OPACITIES
                        .iter()
                        .map(|&o| (Control::Opacity(o), o == f.opacity))
                        .collect(),
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
                rows.push(self.color_row(t.color, true));
                rows.push(Row {
                    label: "Box",
                    controls: vec![
                        (Control::Background(true), t.background),
                        (Control::Background(false), !t.background),
                    ],
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
        }
        if self.default_label.is_some() {
            rows.push(Row {
                label: "",
                controls: vec![(Control::SaveDefault, self.saved)],
            });
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
        self.row = row;
        self.col = col;
        let rows = self.rows();
        self.clamp(&rows);
        match self.focused(&rows) {
            Some(c) => self.activate(c),
            None => Outcome::Nothing,
        }
    }

    /// A click on tab `i`.
    pub fn select_tab(&mut self, i: usize) {
        if i < self.parts.len() && i != self.tab {
            self.tab = i;
            self.row = 0;
            self.col = 0;
            self.hex = None;
            self.saved = false;
        }
    }

    /// A key. `text`: the typed character (no Ctrl/Alt/Cmd).
    pub fn key(&mut self, key: &str, shift: bool, text: Option<&str>) -> Outcome {
        if let Some(hex) = self.hex.as_mut() {
            match key {
                "escape" => self.hex = None,
                "backspace" => {
                    hex.pop();
                }
                "enter" => {
                    if let Some(rgb) = Rgb::parse(hex) {
                        self.hex = None;
                        return self.set_color(Color::Custom(rgb));
                    }
                }
                _ => {
                    if let Some(t) = text {
                        for c in t.chars().filter(char::is_ascii_hexdigit) {
                            if hex.len() < HEX_LEN {
                                hex.push(c.to_ascii_lowercase());
                            }
                        }
                    }
                }
            }
            return Outcome::Nothing;
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
                self.select_tab(next);
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
            Control::Custom => {
                self.hex = Some(match self.parts.get(tab).map(|p| p.style.color()) {
                    Some(Color::Custom(rgb)) => {
                        let mut s = String::new();
                        let _ = rgb.write_hex(&mut s);
                        s.trim_start_matches('#').to_owned()
                    }
                    _ => String::new(),
                });
                return Outcome::Nothing;
            }
            Control::SaveDefault => {
                self.saved = true;
                return Outcome::SaveDefault;
            }
            Control::ExtendLeft => flip(&mut self.options.extend_left),
            Control::ExtendRight => flip(&mut self.options.extend_right),
            Control::AxisTag => flip(&mut self.options.axis_tag),
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
                    (PartStyle::Text(t), Control::Background(b)) => t.background = b,
                    _ => {}
                }
                p.style != before
            }
        };
        if changed {
            self.saved = false;
            Outcome::Changed
        } else {
            Outcome::Nothing
        }
    }

    /// Set the current tab's color. A line's new color carries to a fill that matched its old
    /// one (a zone recolored keeps its fill in step).
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
        if was_line {
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

    // ---- readability guard ----------------------------------------------------------------

    /// The current tab's readability warning, in words, or `None`. `resolve` gives a color in
    /// the current theme; `chart` and `panel` are the chart background and a text box's;
    /// `meanings` the theme's colors with a meaning. Never blocks anything.
    pub fn warning(
        &self,
        resolve: impl Fn(Color) -> Rgb,
        chart: Rgb,
        panel: Rgb,
        meanings: &[(&'static str, Rgb)],
    ) -> Option<String> {
        let part = self.parts.get(self.tab)?;
        let (color, opacity, bg, text, contrast) = match part.style {
            PartStyle::Line(l) => (l.color, l.opacity, chart, false, true),
            // A fill is translucent area behind data: only its hue can mislead.
            PartStyle::Fill(f) => (f.color, 100, chart, false, false),
            PartStyle::Text(t) => (
                t.color,
                100,
                if t.background { panel } else { chart },
                true,
                true,
            ),
        };
        let r = readability(resolve(color), opacity, bg, meanings, text);
        let mut out = Vec::with_capacity(2);
        if let Some(name) = r.near {
            out.push(format!(
                "Close to {name}, which already means something on the chart."
            ));
        }
        if let Some(c) = r.low_contrast.filter(|_| contrast) {
            let what = if text { "text" } else { "lines" };
            let against = if text && bg == panel {
                "its box"
            } else {
                "the chart"
            };
            out.push(format!(
                "Low contrast: {c:.1}:1 against {against} ({what} need {}:1).",
                r.needed
            ));
        }
        (!out.is_empty()).then(|| out.join(" "))
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

/// What a control reads (chips in the editor; swatches have no words and are named for the
/// accessibility tree).
pub fn control_label(c: Control) -> String {
    match c {
        Control::Color(Color::Text) => "Text".to_owned(),
        Control::Color(Color::Palette(i)) => format!("Theme {}", i + 1),
        Control::Color(Color::Custom(rgb)) => {
            let mut s = String::from("Custom ");
            let _ = rgb.write_hex(&mut s);
            s
        }
        Control::Custom => "Custom…".to_owned(),
        Control::LineType(t) => t.label().to_owned(),
        Control::Width(w) => format!("{w} px"),
        Control::Opacity(o) => format!("{o}%"),
        Control::TextSize(s) => s.label().to_owned(),
        Control::Background(true) => "On".to_owned(),
        Control::Background(false) => "Off".to_owned(),
        Control::ExtendLeft => "Left".to_owned(),
        Control::ExtendRight => "Right".to_owned(),
        Control::AxisTag => "On axis".to_owned(),
        Control::SaveDefault => "Save as default".to_owned(),
    }
}

#[cfg(test)]
mod tests;
