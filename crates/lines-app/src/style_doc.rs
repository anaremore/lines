//! Saved form of user styles (decision 0026): the drawing style fields of `drawings.json`
//! (also the per-tool defaults in settings), and the color encoding every saved style shares
//! (studies use it too from PR 2 of decision 0026).
//!
//! Every field is optional and written only when it differs from the default, so a file
//! without styles (written before user styles) loads as before and older Lines builds read new
//! files: `color` (a palette index) and `dashed` keep their old meaning, a custom line color
//! goes in `line_color` (older builds fall back to `color`), and dotted / dash-dot lines also
//! set `dashed` (older builds draw them dashed).
//!
//! Colors are strings: `p0`..`p3` (theme palette), `text` (the theme's text color), `own` (a
//! study part's own color; never written, a missing color means it) or `#rrggbb` (custom).

use lines_core::drawings::Style;
use lines_core::style::{Color, FillStyle, LineType, Rgb, TextSize, TextStyle};
use serde::{Deserialize, Serialize};

/// A color's saved name.
pub fn color_key(c: Color) -> String {
    match c {
        Color::Palette(i) => format!("p{i}"),
        Color::Text => "text".to_owned(),
        Color::Own => "own".to_owned(),
        Color::Custom(rgb) => {
            let mut s = String::with_capacity(7);
            let _ = rgb.write_hex(&mut s);
            s
        }
    }
}

/// A saved color name back (`None`: not one this version knows).
pub fn parse_color(s: &str) -> Option<Color> {
    if s == "text" {
        return Some(Color::Text);
    }
    if s == "own" {
        return Some(Color::Own);
    }
    if let Some(i) = s.strip_prefix('p') {
        return i.parse().ok().map(Color::Palette);
    }
    if s.starts_with('#') {
        return Rgb::parse(s).map(Color::Custom);
    }
    None
}

fn one() -> u8 {
    1
}
fn hundred() -> u8 {
    100
}
fn is_zero(v: &u8) -> bool {
    *v == 0
}
fn is_one(v: &u8) -> bool {
    *v == 1
}
fn is_hundred(v: &u8) -> bool {
    *v == 100
}
fn not(v: &bool) -> bool {
    !*v
}

/// A drawing's style as saved (flattened into a drawing entry, or a per-tool default).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleDoc {
    /// The line's palette index (also the fallback for older builds when `line_color` is set).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub color: u8,
    /// A line color that isn't a palette index: `text` or `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_color: Option<String>,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub width: u8,
    /// Any non-solid line (older builds draw dotted and dash-dot lines dashed).
    #[serde(default, skip_serializing_if = "not")]
    pub dashed: bool,
    /// `dotted` or `dash-dot` (absent: `dashed` decides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// Percent.
    #[serde(default = "hundred", skip_serializing_if = "is_hundred")]
    pub opacity: u8,
    /// A zone's fill, when it isn't the line's color at 12 %.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_opacity: Option<u8>,
    /// A note's text: `s`, `m` (default), `l`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_color: Option<String>,
    /// The note's panel box (default on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_bg: Option<bool>,
    #[serde(default, skip_serializing_if = "not")]
    pub extend_left: bool,
    #[serde(default, skip_serializing_if = "not")]
    pub extend_right: bool,
    #[serde(default, skip_serializing_if = "not")]
    pub axis_tag: bool,
    /// An arrowhead at the end (trend lines, arrows, brush strokes). Optional: absent is none,
    /// so an arrow (head on by default) writes it.
    #[serde(default, skip_serializing_if = "not")]
    pub arrow_end: bool,
}

impl Default for StyleDoc {
    fn default() -> Self {
        StyleDoc::from_style(&Style::DEFAULT)
    }
}

/// A zone's fill when none is saved: its line's color, translucent (the look before styles).
fn legacy_fill(line: Color) -> FillStyle {
    FillStyle {
        color: line,
        ..FillStyle::DEFAULT
    }
}

impl StyleDoc {
    pub fn from_style(s: &Style) -> StyleDoc {
        let (color, line_color) = match s.line.color {
            Color::Palette(i) => (i, None),
            c => (0, Some(color_key(c))),
        };
        let fill = (s.fill != legacy_fill(s.line.color)).then_some(s.fill);
        let t = s.text;
        StyleDoc {
            color,
            line_color,
            width: s.line.width,
            dashed: s.line.line != LineType::Solid,
            line: matches!(s.line.line, LineType::Dotted | LineType::DashDot)
                .then(|| s.line.line.key().to_owned()),
            opacity: s.line.opacity,
            fill_color: fill.map(|f| color_key(f.color)),
            fill_opacity: fill.map(|f| f.opacity),
            text_size: (t.size != TextStyle::DEFAULT.size).then(|| t.size.key().to_owned()),
            text_color: (t.color != TextStyle::DEFAULT.color).then(|| color_key(t.color)),
            text_bg: (t.background != TextStyle::DEFAULT.background).then_some(t.background),
            extend_left: s.extend_left,
            extend_right: s.extend_right,
            axis_tag: s.axis_tag,
            arrow_end: s.arrow_end,
        }
    }

    /// The style (normalised). Values this version doesn't know fall back to defaults.
    pub fn to_style(&self) -> Style {
        let line_color = self
            .line_color
            .as_deref()
            .and_then(parse_color)
            .unwrap_or(Color::Palette(self.color));
        let line = match self.line.as_deref().and_then(LineType::from_key) {
            Some(l) => l,
            None if self.dashed => LineType::Dashed,
            None => LineType::Solid,
        };
        let mut fill = legacy_fill(line_color);
        if let Some(c) = self.fill_color.as_deref().and_then(parse_color) {
            fill.color = c;
        }
        if let Some(o) = self.fill_opacity {
            fill.opacity = o;
        }
        let d = TextStyle::DEFAULT;
        Style {
            line: lines_core::style::LineStyle {
                color: line_color,
                line,
                width: self.width,
                opacity: self.opacity,
            },
            fill,
            text: TextStyle {
                size: self
                    .text_size
                    .as_deref()
                    .and_then(TextSize::from_key)
                    .unwrap_or(d.size),
                color: self
                    .text_color
                    .as_deref()
                    .and_then(parse_color)
                    .unwrap_or(d.color),
                background: self.text_bg.unwrap_or(d.background),
            },
            extend_left: self.extend_left,
            extend_right: self.extend_right,
            axis_tag: self.axis_tag,
            arrow_end: self.arrow_end,
        }
        .normalised()
    }
}

/// A tool's saved default (settings `drawing_styles`): its style, and the fields a newer
/// build wrote that this one doesn't know, kept and written back unchanged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedStyle {
    #[serde(flatten)]
    pub doc: StyleDoc,
    /// After `doc`, which takes its own fields first.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// JSON values have no NaN, so equality is total (settings compare whole files).
impl Eq for SavedStyle {}

/// Save `style` as the default for the tool keyed `key`: a newer build's fields on that entry
/// stay; the default style with nothing else to keep removes the entry.
pub fn save_default(
    map: &mut std::collections::BTreeMap<String, SavedStyle>,
    key: &str,
    style: &Style,
) {
    let doc = StyleDoc::from_style(style);
    match map.get_mut(key) {
        Some(e) => e.doc = doc,
        None => {
            map.insert(
                key.to_owned(),
                SavedStyle {
                    doc,
                    extra: serde_json::Map::new(),
                },
            );
        }
    }
    if map
        .get(key)
        .is_some_and(|e| e.extra.is_empty() && e.doc == StyleDoc::default())
    {
        map.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::style::LineStyle;

    /// N5: "own" is a study part's color; a drawing file holding it (hand-edited, or a
    /// study style pasted in) loads as the drawing's default color, everywhere it can sit.
    #[test]
    fn own_on_a_drawing_loads_as_the_default_color() {
        let d: StyleDoc =
            serde_json::from_str(r#"{"line_color":"own","fill_color":"own","text_color":"own"}"#)
                .unwrap();
        let s = d.to_style();
        assert_eq!(s.line.color, Color::Palette(0));
        assert_eq!(s.fill.color, Color::Palette(0));
        assert_eq!(s.text.color, Color::Palette(0));
        assert_eq!(Style::DEFAULT.normalised(), Style::DEFAULT);
    }

    #[test]
    fn colors_round_trip() {
        for c in [
            Color::Palette(0),
            Color::Palette(3),
            Color::Text,
            Color::Own,
            Color::Custom(Rgb::hex(0x12ab9f)),
        ] {
            assert_eq!(parse_color(&color_key(c)), Some(c));
        }
        for bad in ["", "p", "px", "#12", "red", "P1"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn default_style_writes_nothing() {
        let json = serde_json::to_string(&StyleDoc::default()).unwrap();
        assert_eq!(json, "{}");
        let back: StyleDoc = serde_json::from_str("{}").unwrap();
        assert_eq!(back.to_style(), Style::DEFAULT);
    }

    #[test]
    fn every_part_round_trips() {
        let s = Style {
            line: LineStyle {
                color: Color::Custom(Rgb::hex(0xe5484d)),
                line: LineType::DashDot,
                width: 3,
                opacity: 50,
            },
            fill: FillStyle {
                color: Color::Palette(2),
                opacity: 40,
            },
            text: TextStyle {
                size: TextSize::Large,
                color: Color::Custom(Rgb::hex(0x3d7bf5)),
                background: false,
            },
            extend_left: true,
            extend_right: true,
            axis_tag: true,
            arrow_end: true,
        };
        let doc = StyleDoc::from_style(&s);
        assert!(doc.arrow_end, "the arrowhead is written");
        // Older builds still see a dashed line in palette color 0.
        assert!(doc.dashed);
        assert_eq!(doc.color, 0);
        let json = serde_json::to_string(&doc).unwrap();
        let back: StyleDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_style(), s);
    }

    #[test]
    fn unknown_values_fall_back_to_defaults() {
        let doc: StyleDoc = serde_json::from_str(
            r#"{"color": 2, "dashed": true, "line": "wavy", "line_color": "plaid",
                "text_size": "xl", "width": 40, "opacity": 0}"#,
        )
        .unwrap();
        let s = doc.to_style();
        assert_eq!(s.line.color, Color::Palette(2));
        assert_eq!(s.line.line, LineType::Dashed);
        assert_eq!(s.text.size, TextSize::Medium);
        assert_eq!((s.line.width, s.line.opacity), (4, 10));
        // A zone without a saved fill fills in its line's color.
        assert_eq!(s.fill.color, Color::Palette(2));
    }

    /// A newer build's fields on a saved default survive this build saving a new default.
    #[test]
    fn saved_defaults_keep_a_newer_builds_fields() {
        let mut map: std::collections::BTreeMap<String, SavedStyle> = serde_json::from_str(
            r#"{"trend": {"width": 2, "glow": {"radius": 3}}, "spiral": {"turns": 4}}"#,
        )
        .unwrap();
        assert_eq!(map["trend"].doc.width, 2);
        let s = Style {
            line: LineStyle {
                width: 4,
                ..LineStyle::DEFAULT
            },
            ..Style::DEFAULT
        };
        save_default(&mut map, "trend", &s);
        save_default(&mut map, "hline", &Style::DEFAULT);
        let json = serde_json::to_value(&map).unwrap();
        assert_eq!(json["trend"]["width"], 4);
        assert_eq!(json["trend"]["glow"]["radius"], 3);
        assert_eq!(json["spiral"]["turns"], 4, "an unknown tool's entry stays");
        assert!(
            json.get("hline").is_none(),
            "the default style writes nothing"
        );
        // Back to the default: the newer fields still keep the entry.
        save_default(&mut map, "trend", &Style::DEFAULT);
        assert_eq!(
            serde_json::to_value(&map).unwrap()["trend"]["glow"]["radius"],
            3
        );
    }
}
