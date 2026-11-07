//! Study styles (decision 0026, PR 2): which parts each study has, how a part is saved, and
//! the style editor a study opens from the studies menu ("Style…").
//!
//! - **Parts** ([`PartDef`]): a VWAP's line, ±σ band edges and band fill; cumulative delta's
//!   rising and falling candles; the volume profile's buy and sell rows, POC and value area;
//!   the footprint's cells, imbalance marks and POC; a level source's lines and zones. Each
//!   has a default that is exactly how it was drawn before styles: a part's own color
//!   ([`Color::Own`]: its theme token, the VWAP's line color, a level source's colors) at 100 %
//!   of its own translucency (`Theme::part_color`). Two exceptions (`docs/known-gaps.md`):
//!   unstyled sell × buy footprint cells are held back for their numbers, and VWAP band fills
//!   sit under the candles.
//! - **Saved** as optional fields: a study's [`PartStyles`] (a VWAP's and cumulative delta's in
//!   their `IndicatorConfig`, the chart-wide studies' in [`StudyStyles`], the chart's `styles`
//!   and a study set's). Each part is a flat object (`color`, `line`, `width`, `opacity`,
//!   `shown`) holding only what differs from its default; parts and fields this build doesn't
//!   know are kept and written back. A VWAP's line keeps its palette index in the config's
//!   `color` and its width in `width` (older builds read them), a custom color in its part.
//! - **Direction is never by color alone** whatever the colors: delta candles keep hollow
//!   bodies, profile sells sit at the base edge, imbalance marks sit under their side's number,
//!   footprint strips keep their open wedge.
//!
//! Pure (no GPUI): paint reads [`Look`]s resolved when the configuration changes, never the
//! saved JSON.

use std::collections::BTreeMap;

use lines_core::style::{Color, FillStyle, LineStyle, LineType, MAX_WIDTH, MIN_WIDTH, Rgb};
use lines_indicators::{IndicatorConfig, PartStyles};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::style_doc::{color_key, parse_color};
use crate::style_editor::{Own, OwnToken, Part, PartStyle, StyleEditor};

/// One part of a study: its saved key, its tab, and its default.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PartDef {
    /// The saved name (`"line"`).
    pub key: &'static str,
    /// The style editor's tab ("Line").
    pub title: &'static str,
    /// The default (today's look).
    pub style: PartStyle,
    /// Line type and width offered.
    pub stroke: bool,
    pub own: Option<Own>,
    /// Drawn only when shown (off by default: VWAP band edges).
    pub optional: bool,
    /// One line under its colors in the editor: how they apply.
    pub note: Option<&'static str>,
}

const fn line(color: Color, width: u8, opacity: u8) -> PartStyle {
    PartStyle::Line(LineStyle {
        color,
        line: LineType::Solid,
        width,
        opacity,
    })
}

const fn fill(opacity: u8) -> PartStyle {
    PartStyle::Fill(FillStyle {
        color: Color::Own,
        opacity,
    })
}

const fn own(name: &'static str, token: OwnToken) -> Option<Own> {
    Some(Own { name, token })
}

const fn part(
    key: &'static str,
    title: &'static str,
    style: PartStyle,
    stroke: bool,
    own: Option<Own>,
) -> PartDef {
    PartDef {
        key,
        title,
        style,
        stroke,
        own,
        optional: false,
        note: None,
    }
}

pub const VWAP_LINE: usize = 0;
pub const VWAP_BANDS: usize = 1;
pub const VWAP_FILL: usize = 2;
/// A VWAP: its line (palette color index and width in the config), the ±σ band edges
/// (optional: before styles a band was its fill only), and the band fill (its own color is
/// the line's at 24 %, so the default 25 % is the 6 % it always had).
pub const VWAP: [PartDef; 3] = [
    part("line", "Line", line(Color::Palette(0), 2, 100), true, None),
    PartDef {
        optional: true,
        ..part(
            "bands",
            "Band edges",
            line(Color::Own, 1, 50),
            true,
            own("Line color", OwnToken::Line),
        )
    },
    part(
        "fill",
        "Band fill",
        fill(25),
        false,
        own("Line color", OwnToken::Line),
    ),
];
/// The band fill's own translucency (`VWAP_FILL` at 100 %).
pub const VWAP_FILL_ALPHA: f32 = 0.24;

pub const DELTA_UP: usize = 0;
pub const DELTA_DOWN: usize = 1;
pub const DELTA: [PartDef; 2] = [
    part(
        "up",
        "Rising",
        fill(100),
        false,
        own("Theme up", OwnToken::Up),
    ),
    part(
        "down",
        "Falling",
        fill(100),
        false,
        own("Theme down", OwnToken::Down),
    ),
];

pub const PROFILE_BUY: usize = 0;
pub const PROFILE_SELL: usize = 1;
pub const PROFILE_POC: usize = 2;
pub const PROFILE_VA: usize = 3;
/// The volume profile: buy and sell rows (split by aggressor), the POC (width in device
/// pixels: 1 is the hairline it always was) and the value area (its rows; its edges follow).
pub const PROFILE: [PartDef; 4] = [
    PartDef {
        note: Some(SIDES_NOTE),
        ..part(
            "buy",
            "Buy rows",
            fill(100),
            false,
            own("Theme", OwnToken::ProfileBuy),
        )
    },
    PartDef {
        note: Some(SIDES_NOTE),
        ..part(
            "sell",
            "Sell rows",
            fill(100),
            false,
            own("Theme", OwnToken::ProfileSell),
        )
    },
    part(
        "poc",
        "POC",
        line(Color::Own, 1, 100),
        true,
        own("Theme", OwnToken::ProfilePoc),
    ),
    part(
        "va",
        "Value area",
        fill(100),
        false,
        own("Theme", OwnToken::ProfileVa),
    ),
];

/// Buy and sell rows are told apart by place, whatever their colors.
const SIDES_NOTE: &str = "Sells sit at the profile's base edge, buys after them.";

pub const FP_CELLS: usize = 0;
pub const FP_IMBALANCE: usize = 1;
pub const FP_POC: usize = 2;
/// The footprint: cell shading, imbalance marks (each under its side's number) and the
/// bar's POC outline.
pub const FOOTPRINT: [PartDef; 3] = [
    PartDef {
        note: Some(
            "Opacity scales the cells' own light shading; a strong color is held back so the counts on it stay readable.",
        ),
        ..part(
            "cells",
            "Cells",
            fill(100),
            false,
            own("Theme", OwnToken::Neutral),
        )
    },
    part(
        "imbalance",
        "Imbalances",
        line(Color::Own, 1, 100),
        false,
        own("Theme up / down", OwnToken::UpDown),
    ),
    part(
        "poc",
        "POC",
        line(Color::Own, 1, 100),
        false,
        own("Theme", OwnToken::Text),
    ),
];

pub const LEVEL_LINES: usize = 0;
pub const LEVEL_ZONES: usize = 1;
/// A level source's override: color and opacity of its lines and of its zones (line type and
/// weight stay the source's). Its own colors are the source's (decision 0022): each level
/// keeps its own; a color picked applies to all of them. Zone edges take the Zones color and
/// the Lines opacity. Zones' own translucency is 40 %, so the default 25 % is the 10 % they
/// always had.
pub const LEVELS: [PartDef; 2] = [
    PartDef {
        note: Some(
            "Source colors: each level keeps its own color. A color picked here applies to all of this source's lines.",
        ),
        ..part(
            "lines",
            "Lines",
            line(Color::Own, 1, 100),
            false,
            own("Source colors", OwnToken::Source(None)),
        )
    },
    PartDef {
        note: Some(
            "Source colors: each zone keeps its own color. Zone edges take this color, at the Lines opacity.",
        ),
        ..part(
            "zones",
            "Zones",
            fill(25),
            false,
            own("Source colors", OwnToken::Source(None)),
        )
    },
];
/// Zone fills' own translucency (`LEVEL_ZONES` at 100 %).
pub const LEVEL_ZONE_ALPHA: f32 = 0.40;

/// How one part is drawn now.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub style: PartStyle,
    /// An optional part is drawn (always `true` for the others).
    pub shown: bool,
}

impl Look {
    pub fn color(&self) -> Color {
        match self.style {
            PartStyle::Line(l) => l.color,
            PartStyle::Fill(f) => f.color,
            PartStyle::Text(t) => t.color,
        }
    }

    pub fn opacity(&self) -> u8 {
        match self.style {
            PartStyle::Line(l) => l.opacity,
            PartStyle::Fill(f) => f.opacity,
            PartStyle::Text(_) => 100,
        }
    }

    /// As a line (a fill: solid, 1 px).
    pub fn line(&self) -> LineStyle {
        match self.style {
            PartStyle::Line(l) => l,
            _ => LineStyle {
                color: self.color(),
                opacity: self.opacity(),
                ..LineStyle::DEFAULT
            },
        }
    }

    /// The default look of every part of `defs`.
    pub fn defaults<const N: usize>(defs: &[PartDef; N]) -> [Look; N] {
        defs.map(|d| Look {
            style: d.style,
            shown: !d.optional,
        })
    }
}

/// A saved color a part may take: `own` only where the part has one; never the text color.
fn usable(d: &PartDef, c: Color) -> bool {
    match c {
        Color::Own => d.own.is_some(),
        Color::Text => false,
        Color::Palette(_) | Color::Custom(_) => true,
    }
}

fn read_part(v: Option<&Value>, d: &PartDef) -> Look {
    let mut look = Look {
        style: d.style,
        shown: !d.optional,
    };
    let Some(Value::Object(o)) = v else {
        return look;
    };
    let color = o
        .get("color")
        .and_then(Value::as_str)
        .and_then(parse_color)
        .filter(|c| usable(d, *c));
    let num = |k: &str| o.get(k).and_then(Value::as_u64).map(|n| n.min(255) as u8);
    match &mut look.style {
        PartStyle::Line(l) => {
            if let Some(c) = color {
                l.color = c;
            }
            if d.stroke {
                if let Some(t) = o.get("line").and_then(Value::as_str) {
                    l.line = LineType::from_key(t).unwrap_or(l.line);
                }
                if let Some(w) = num("width") {
                    l.width = w;
                }
            }
            if let Some(op) = num("opacity") {
                l.opacity = op;
            }
            *l = l.normalised();
        }
        PartStyle::Fill(f) => {
            if let Some(c) = color {
                f.color = c;
            }
            if let Some(op) = num("opacity") {
                f.opacity = op;
            }
            *f = f.normalised();
        }
        PartStyle::Text(_) => {}
    }
    if d.optional
        && let Some(on) = o.get("shown").and_then(Value::as_bool)
    {
        look.shown = on;
    }
    look
}

/// Each part's look from `map` (missing parts and fields: the default).
pub fn read<const N: usize>(map: &PartStyles, defs: &[PartDef; N]) -> [Look; N] {
    defs.map(|d| read_part(map.get(d.key), &d))
}

fn set(o: &mut Map<String, Value>, key: &str, v: Option<Value>) {
    match v {
        Some(v) => {
            o.insert(key.to_owned(), v);
        }
        None => {
            o.remove(key);
        }
    }
}

fn write_part(map: &mut PartStyles, d: &PartDef, look: &Look) {
    // What this build reads there now: a part whose look didn't change is left as saved
    // (values out of range or from a newer build stay), and only the fields that changed are
    // written; each is removed when back at its default.
    let was = read_part(map.get(d.key), d);
    if was == *look {
        return;
    }
    let mut o = match map.remove(d.key) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    let differs = |a: Color, b: Color| (a != b).then(|| Value::String(color_key(a)));
    match (look.style, was.style, d.style) {
        (PartStyle::Line(l), PartStyle::Line(w), PartStyle::Line(def)) => {
            if l.color != w.color {
                set(&mut o, "color", differs(l.color, def.color));
            }
            // Line type and width only where they are read (a part with a stroke).
            if d.stroke && l.line != w.line {
                let v = (l.line != def.line).then(|| Value::String(l.line.key().to_owned()));
                set(&mut o, "line", v);
            }
            if d.stroke && l.width != w.width {
                set(
                    &mut o,
                    "width",
                    (l.width != def.width).then(|| Value::from(l.width)),
                );
            }
            if l.opacity != w.opacity {
                set(
                    &mut o,
                    "opacity",
                    (l.opacity != def.opacity).then(|| Value::from(l.opacity)),
                );
            }
        }
        (PartStyle::Fill(f), PartStyle::Fill(w), PartStyle::Fill(def)) => {
            if f.color != w.color {
                set(&mut o, "color", differs(f.color, def.color));
            }
            if f.opacity != w.opacity {
                set(
                    &mut o,
                    "opacity",
                    (f.opacity != def.opacity).then(|| Value::from(f.opacity)),
                );
            }
        }
        _ => {}
    }
    if d.optional && look.shown != was.shown {
        set(&mut o, "shown", look.shown.then_some(Value::Bool(true)));
    }
    if !o.is_empty() {
        map.insert(d.key.to_owned(), Value::Object(o));
    }
}

/// Write `looks` into `map`: only the parts whose look changed, only their changed fields
/// (each saved only while it differs from the part's default); a part's fields this build
/// doesn't know or doesn't read, and parts it doesn't know, stay.
pub fn write(map: &mut PartStyles, defs: &[PartDef], looks: &[Look]) {
    for (d, l) in defs.iter().zip(looks) {
        write_part(map, d, l);
    }
}

/// A VWAP line's width in logical px for its width step (1–4; 2 = the 1.75 px it always had).
pub fn vwap_px(width: u8) -> f32 {
    match width {
        1 => 1.25,
        3 => 2.5,
        4 => 3.25,
        _ => 1.75,
    }
}

/// A VWAP's looks: its line's palette color and width from the config, the rest saved.
pub fn vwap_looks(c: &IndicatorConfig) -> [Look; 3] {
    let mut looks = read(c.style(), &VWAP);
    if let IndicatorConfig::Vwap { color, display, .. } = c
        && let PartStyle::Line(l) = &mut looks[VWAP_LINE].style
    {
        if !matches!(l.color, Color::Custom(_)) {
            l.color = Color::Palette(*color % 4);
        }
        l.width = display.width.clamp(MIN_WIDTH, MAX_WIDTH);
    }
    looks
}

/// Restyle a VWAP: a palette line color goes to the config's `color`, the width to its
/// `width` (what older builds read), everything else to its saved parts.
pub fn set_vwap(c: &mut IndicatorConfig, looks: &[Look]) {
    let mut looks: Vec<Look> = looks.to_vec();
    if let IndicatorConfig::Vwap { color, display, .. } = c
        && let Some(PartStyle::Line(l)) = looks.get_mut(VWAP_LINE).map(|l| &mut l.style)
    {
        if let Color::Palette(i) = l.color {
            *color = i % 4;
            l.color = Color::Palette(0);
        }
        display.width = l.width.clamp(MIN_WIDTH, MAX_WIDTH);
        l.width = 2;
    }
    write(c.style_mut(), &VWAP, &looks);
}

/// The chart-wide studies' styles (the chart's `styles` field, a study set's): the volume
/// profile, the footprint, and a style override per level source. Optional; studies a newer
/// build styles are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudyStyles {
    #[serde(skip_serializing_if = "PartStyles::is_empty")]
    pub profile: PartStyles,
    #[serde(skip_serializing_if = "PartStyles::is_empty")]
    pub footprint: PartStyles,
    /// By level source id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub levels: BTreeMap<String, PartStyles>,
    /// What a newer build saved here, written back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl StudyStyles {
    pub fn is_empty(&self) -> bool {
        *self == StudyStyles::default()
    }

    /// Restyle level source `id` (an override back to the source's own style is dropped).
    pub fn set_levels(&mut self, id: &str, looks: &[Look]) {
        let m = self.levels.entry(id.to_owned()).or_default();
        write(m, &LEVELS, looks);
        if m.is_empty() {
            self.levels.remove(id);
        }
    }
}

/// The style editor for a study's parts, titled `title`. `source`: a level source's first
/// color (its own color's swatch).
pub fn editor(title: String, defs: &[PartDef], looks: &[Look], source: Option<Rgb>) -> StyleEditor {
    let parts = defs
        .iter()
        .zip(looks)
        .map(|(d, l)| Part {
            title: d.title,
            style: l.style,
            stroke: d.stroke,
            own: d.own.map(|o| match o.token {
                OwnToken::Source(_) => Own {
                    token: OwnToken::Source(source),
                    ..o
                },
                _ => o,
            }),
            shown: d.optional.then_some(l.shown),
            note: d.note,
        })
        .collect();
    StyleEditor::for_study(title, parts)
}

/// The looks an editor shows (its parts in order).
pub fn looks_of(e: &StyleEditor) -> Vec<Look> {
    e.parts
        .iter()
        .map(|p| Look {
            style: p.style,
            shown: p.shown.unwrap_or(true),
        })
        .collect()
}

/// Another study's line drawn on the chart (a VWAP's line or band edges, the profile's POC),
/// which an edited part shouldn't look like: its color in each theme (`Theme::all` order) and
/// its line type.
#[derive(Clone, Debug, PartialEq)]
pub struct OtherLine {
    /// As the warning names it ("the VWAP ETH line").
    pub name: String,
    pub rgb: Vec<Rgb>,
    pub line: LineType,
}

/// What a study's style editor warnings need from its chart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartCtx {
    /// The footprint's mode (its Cells tab warns for it).
    pub footprint_mode: crate::footprint::FootprintMode,
    /// Band fills nesting at the edited VWAP's innermost band (0: no bands).
    pub vwap_layers: u8,
    /// The other visible studies' lines.
    pub others: Vec<OtherLine>,
}

impl ChartCtx {
    /// The context of study `r` on a chart showing `set`, for `themes` (`Theme::all`).
    pub fn of(
        set: &crate::study_menu::StudySet,
        r: &crate::study_menu::StudyRef,
        themes: &[crate::theme::Theme],
    ) -> ChartCtx {
        use crate::study_menu::StudyRef;
        use crate::theme::rgb_of;
        let mut others = Vec::new();
        for (i, c) in set.indicators.iter().enumerate() {
            let IndicatorConfig::Vwap { spec, display, .. } = c else {
                continue;
            };
            if display.hidden || *r == StudyRef::Vwap(i) {
                continue;
            }
            let looks = vwap_looks(c);
            let name = format!("VWAP {}", crate::study_menu::reset_label(spec.reset));
            let line = looks[VWAP_LINE].line();
            let at = |t: &crate::theme::Theme| t.study_line(0, line.color);
            others.push(OtherLine {
                name: format!("the {name} line"),
                rgb: themes.iter().map(|t| rgb_of(at(t))).collect(),
                line: line.line,
            });
            let bands = looks[VWAP_BANDS];
            if bands.shown && spec.bands.0 & 0b111 != 0 {
                let b = bands.line();
                others.push(OtherLine {
                    name: format!("the {name} band edges"),
                    rgb: themes
                        .iter()
                        .map(|t| rgb_of(t.part_color(b.color, at(t), 100)))
                        .collect(),
                    line: b.line,
                });
            }
        }
        if set.profile.mode != crate::profile::ProfileMode::Off
            && !set.profile.hidden
            && *r != StudyRef::Profile
        {
            let poc = read(&set.styles.profile, &PROFILE)[PROFILE_POC].line();
            others.push(OtherLine {
                name: "the profile's POC".to_owned(),
                rgb: themes
                    .iter()
                    .map(|t| rgb_of(t.part_color(poc.color, t.profile_poc, 100)))
                    .collect(),
                line: poc.line,
            });
        }
        let vwap_layers = match r {
            StudyRef::Vwap(i) => match set.indicators.get(*i) {
                Some(IndicatorConfig::Vwap { spec, .. }) => crate::studies::band_layers(spec.bands),
                _ => 0,
            },
            _ => 0,
        };
        ChartCtx {
            footprint_mode: set.footprint.mode,
            vwap_layers,
            others,
        }
    }
}

/// A VWAP's Band fill held back under the candles (`Theme::under_candles`, `layers` nested)
/// in some theme or palette. Unstyled fills never are (tested). `themes` in `ctxs` order.
pub fn band_fill_warning(
    looks: &[Look],
    layers: u8,
    themes: &[crate::theme::Theme],
    ctxs: &[crate::style_editor::ThemeCtx],
) -> Option<crate::style_editor::Warning> {
    let (Some(line), Some(fill)) = (looks.get(VWAP_LINE), looks.get(VWAP_FILL)) else {
        return None;
    };
    if layers == 0 || fill.style == VWAP[VWAP_FILL].style {
        return None;
    }
    let hit: Vec<bool> = themes
        .iter()
        .map(|t| {
            let base = t.study_line(0, line.color());
            let c = t.part_color(fill.color(), base.opacity(VWAP_FILL_ALPHA), fill.opacity());
            t.under_candles(c, layers).1
        })
        .collect();
    under_candles_warning("band fill", ctxs, &hit)
}

/// A level source's Zones held back under the candles (`crate::levels_layer::zone_scale`) in
/// some theme or palette; its own colors are judged by `source` (its first color). Unstyled
/// zones never are (tested). `themes` in `ctxs` order.
pub fn zones_warning(
    looks: &[Look],
    source: Option<Rgb>,
    themes: &[crate::theme::Theme],
    ctxs: &[crate::style_editor::ThemeCtx],
) -> Option<crate::style_editor::Warning> {
    let looks: &[Look; 2] = looks.try_into().ok()?;
    let z = looks[LEVEL_ZONES];
    if z.style == LEVELS[LEVEL_ZONES].style {
        return None;
    }
    let rgba = match (z.color(), source) {
        (Color::Own, Some(c)) => {
            (u32::from(c.r) << 24) | (u32::from(c.g) << 16) | (u32::from(c.b) << 8) | 0xff
        }
        (Color::Own, None) => return None,
        _ => 0x0000_00ff,
    };
    let hit: Vec<bool> = themes
        .iter()
        .map(|t| crate::levels_layer::zone_scale(t, looks, std::iter::once(rgba)) < 1.0)
        .collect();
    under_candles_warning("zones", ctxs, &hit)
}

fn under_candles_warning(
    what: &str,
    ctxs: &[crate::style_editor::ThemeCtx],
    hit: &[bool],
) -> Option<crate::style_editor::Warning> {
    hit.iter().any(|h| *h).then(|| crate::style_editor::Warning {
        text: format!(
            "Shown fainter{}, so the candles (3:1) and the last-price line over the {what} stay visible.",
            crate::style_editor::where_phrase(ctxs, hit)
        ),
        swatch: None,
    })
}

/// The edited part (a line, shown) looks like another study's line in some theme or palette:
/// a perceptually close color (CIEDE2000 under the guard's "looks like" gap) and the same line
/// type. One warning per line it looks like.
pub fn alike_warnings(
    e: &StyleEditor,
    others: &[OtherLine],
    themes: &[crate::theme::Theme],
    ctxs: &[crate::style_editor::ThemeCtx],
) -> Vec<crate::style_editor::Warning> {
    use lines_core::style::{NEAR_MEANING, delta_e_2000};
    let Some(p) = e.parts.get(e.tab) else {
        return Vec::new();
    };
    let PartStyle::Line(l) = p.style else {
        return Vec::new();
    };
    if p.shown == Some(false) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for o in others.iter().filter(|o| o.line == l.line) {
        let hit: Vec<bool> = themes
            .iter()
            .zip(&o.rgb)
            .map(|(t, other)| {
                let c = crate::theme::rgb_of(t.swatch_color(e, l.color));
                delta_e_2000(c, *other) < NEAR_MEANING
            })
            .collect();
        if hit.iter().any(|h| *h) {
            out.push(crate::style_editor::Warning {
                text: format!(
                    "Looks like {}{}: same line type and a close color. Pick another color or line type to tell them apart.",
                    o.name,
                    crate::style_editor::where_phrase(ctxs, &hit)
                ),
                swatch: None,
            });
        }
    }
    out
}

/// Everything a study's style editor warns about on its current tab, naming theme and palette
/// each time: the editor's guard (a picked color against the colors with a meaning and the
/// chart), a line that looks like another study's, the footprint's cells shown fainter for
/// their counts, a band fill or zones shown fainter for the candles over them, and profile
/// rows whose buy and sell look alike. `themes`: every theme and palette (`Theme::all`).
pub fn warnings(
    r: &crate::study_menu::StudyRef,
    e: &StyleEditor,
    themes: &[crate::theme::Theme],
    chart: &ChartCtx,
) -> Vec<crate::style_editor::Warning> {
    use crate::study_menu::StudyRef;
    let ctxs: Vec<_> = themes.iter().map(crate::theme::Theme::study_ctx).collect();
    let mut out = e.warnings(&ctxs);
    out.extend(alike_warnings(e, &chart.others, themes, &ctxs));
    let looks = looks_of(e);
    match r {
        StudyRef::Footprint if e.tab == FP_CELLS => {
            if let Ok(l) = <[Look; 3]>::try_from(looks.as_slice()) {
                out.extend(crate::footprint::cells_warning(
                    &l,
                    chart.footprint_mode,
                    themes,
                    &ctxs,
                ));
            }
        }
        StudyRef::Footprint if e.tab == FP_IMBALANCE => {
            if let Ok(l) = <[Look; 3]>::try_from(looks.as_slice()) {
                out.extend(crate::footprint::imbalance_warning(&l));
            }
        }
        StudyRef::Vwap(_) if e.tab == VWAP_FILL => {
            out.extend(band_fill_warning(&looks, chart.vwap_layers, themes, &ctxs));
        }
        StudyRef::Levels(_) if e.tab == LEVEL_ZONES => {
            let source = e.parts.get(LEVEL_ZONES).and_then(|p| match p.own {
                Some(Own {
                    token: OwnToken::Source(c),
                    ..
                }) => c,
                _ => None,
            });
            out.extend(zones_warning(&looks, source, themes, &ctxs));
        }
        StudyRef::Profile if e.tab == PROFILE_BUY || e.tab == PROFILE_SELL => {
            if let Ok(l) = <[Look; 4]>::try_from(looks.as_slice()) {
                out.extend(crate::profile::sides_warning(&l, themes, &ctxs));
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests;
