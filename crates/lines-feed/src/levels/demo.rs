//! A synthetic level source (decision 0010: synthetic data only) for tests and screenshots:
//! `demo` serves one static map for ES and MES, as a real adapter would: a transport that
//! returns a JSON body, and a pure `decode`. It also shows an anchored level: a line placed
//! relative to the chart's own price at a reference time (`ChartPrices::price_at`), withheld
//! when the chart has no price there (decision 0022).
//!
//! Built only for tests or with the `demo-levels` feature; never registered by default.

use std::time::Duration;

use anyhow::Result;
use lines_core::{Price, Product, UtcMillis};
use serde::Deserialize;

use lines_core::style::{Color, FillStyle, LineStyle, LineType, Rgb};

use super::{
    BoxFuture, ChartPrices, LevelContext, LevelFrame, LevelMap, LevelMeta, LevelSource, LevelStyle,
    LevelTransport, LevelType, LevelValue, RefreshMode, Series, SeriesColor, SeriesLineDef,
    SeriesStudyDef, SettingKind, SettingSpec, SettingValues, TypeStyle,
};

/// The demo's setting keys.
pub mod key {
    pub const FEED_URL: &str = "feed_url";
    pub const ALLOW_REMOTE: &str = "allow_remote";
    pub const SYMBOL: &str = "symbol";
    pub const REFRESH: &str = "refresh";
    pub const BANDS: &str = "bands";
    pub const STATS: &str = "stats";
    pub const BAND_WIDTH: &str = "band_width";
    pub const ANCHOR_OFFSET: &str = "anchor_offset";
    pub const LABELS: &str = "labels";
}

/// The demo's settings: one of each kind, as a real source's would be (the generic settings UI
/// and its tests use them). Its Connection settings change nothing (it is synthetic: nothing
/// is fetched) but are checked as a real source's would be; its Display settings change the
/// map.
pub static SETTINGS: [SettingSpec; 9] = [
    SettingSpec::new(
        key::FEED_URL,
        "Feed URL",
        SettingKind::Url,
        "http://127.0.0.1:7878",
        "Where a real source would fetch from (the demo fetches nothing). This machine only unless remote is allowed.",
    )
    .group("Connection")
    .connection(),
    SettingSpec::new(
        key::ALLOW_REMOTE,
        "Allow remote",
        SettingKind::Bool,
        "false",
        "Accept a feed URL that isn't this machine.",
    )
    .group("Connection")
    .confirm("Allow a remote feed? Lines will fetch from the address you enter, on any network it can reach.")
    .connection(),
    SettingSpec::new(
        key::SYMBOL,
        "Symbol",
        SettingKind::Choice(&["auto"]),
        "auto",
        "Auto follows the chart, or one symbol the feed lists.",
    )
    .group("Connection")
    .connection(),
    SettingSpec::new(
        key::REFRESH,
        "Refresh",
        SettingKind::Seconds { min: 1, max: 3600 },
        "5",
        "How often a real source would poll.",
    )
    .group("Connection")
    .connection(),
    SettingSpec::new(
        key::BANDS,
        "Bands",
        SettingKind::Bool,
        "true",
        "The VWAP band.",
    )
    .group("Display"),
    SettingSpec::new(
        key::STATS,
        "Stats",
        SettingKind::Bool,
        "true",
        "The legend's stats.",
    )
    .group("Display"),
    SettingSpec::new(
        key::BAND_WIDTH,
        "Band width",
        SettingKind::Number {
            min: 5,
            max: 500,
            step: 5,
            decimals: 2,
            unit: "",
            optional: false,
        },
        "1.00",
        "The VWAP band's width, times its usual.",
    )
    .group("Display"),
    SettingSpec::new(
        key::ANCHOR_OFFSET,
        "Anchor offset",
        SettingKind::Number {
            min: -40,
            max: 40,
            step: 1,
            decimals: 0,
            unit: "ticks",
            optional: true,
        },
        "",
        "Ticks from the anchor bar's open (empty: the map's own).",
    )
    .group("Display")
    .placeholder("from the map"),
    // Lines' own label switch does this.
    SettingSpec::new(
        key::LABELS,
        "Labels",
        SettingKind::Bool,
        "true",
        "Level names.",
    )
    .owned_by_lines(),
];

fn value<'a>(values: &'a SettingValues, key: &str) -> &'a str {
    values.get(key).map_or_else(
        || {
            SETTINGS
                .iter()
                .find(|s| s.key == key)
                .map_or("", |s| s.default)
        },
        String::as_str,
    )
}

/// Two synthetic lower-pane studies (decision 0010: synthetic): three lines around 50 to 65 and one
/// line swinging around zero, a value a minute over the synthetic sessions, with a gap.
pub const SERIES_STUDIES: &[SeriesStudyDef] = &[
    SeriesStudyDef {
        id: "osc",
        name: "Demo oscillator",
        lines: &[
            SeriesLineDef {
                key: "osc.a",
                label: "A",
                color: SeriesColor::Foreground,
                line: LineType::Solid,
            },
            SeriesLineDef {
                key: "osc.b",
                label: "B",
                color: SeriesColor::Orange,
                line: LineType::Dashed,
            },
            SeriesLineDef {
                key: "osc.c",
                label: "C",
                color: SeriesColor::Blue,
                line: LineType::Dotted,
            },
        ],
        zero_line: false,
        stale_after_secs: 300,
    },
    SeriesStudyDef {
        id: "swing",
        name: "Demo swing",
        lines: &[SeriesLineDef {
            key: "swing",
            label: "Swing",
            color: SeriesColor::Amber,
            line: LineType::Solid,
        }],
        zero_line: true,
        stale_after_secs: 300,
    },
];

/// The synthetic series: one point a minute from `from` to `to`, a gap of `gap` minutes
/// starting `gap_at` minutes in. Deterministic.
fn demo_series(key: &str, from: i64, to: i64, f: impl Fn(f64) -> f64) -> Series {
    const MIN: i64 = 60_000;
    let (gap_at, gap) = (from + 420 * MIN, 20 * MIN);
    let mut points = Vec::new();
    let mut t = from;
    while t <= to {
        if !(gap_at..gap_at + gap).contains(&t) {
            let x = ((t - from) / MIN) as f64;
            points.push((UtcMillis(t), (f(x) * 100.0).round() / 100.0));
        }
        t += MIN;
    }
    Series {
        key: key.to_owned(),
        points,
        segments: vec![
            (UtcMillis(from), UtcMillis(gap_at - MIN)),
            (UtcMillis(gap_at + gap), UtcMillis(to)),
        ],
    }
}

pub const ID: &str = "demo";

/// The demo map, in catalog price units (ES: hundredths of a point). Synthetic. Its colors are
/// its own (decision 0022), drawn as given in every theme, so each is a mid-tone that reads as a
/// line (3:1) on both the dark and the light chart (tested in `lines-app`'s levels layer). Only
/// the zones draw in them by default: the line kinds' types recommend a color per theme
/// ([`TYPES`]).
pub const BODY: &str = r#"{
  "as_of": "2026-11-05T13:00:00",
  "levels": [
    {"kind": "pivot",  "line": 601000, "label": "Pivot 6010",  "color": "838799ff", "weight": 2},
    {"kind": "resist", "line": 602500, "label": "R1 6025",     "color": "a87719ff", "weight": 1},
    {"kind": "support","line": 599750, "label": "S1 5997.50",  "color": "2a9196ff", "weight": 1, "dashed": true},
    {"kind": "supply", "lower": 601750, "upper": 602250, "label": "Supply", "color": "888800ff"},
    {"kind": "zone",   "lower": 599000, "upper": 599500, "label": "Demand", "color": "868686ff"},
    {"kind": "band",   "vwap_lower": -750, "vwap_upper": 750, "label": "VWAP ±7.50", "color": "7a7f91ff", "dashed": true}
  ],
  "anchor": {"at": "2026-11-05T14:30:00", "offset": 750, "label": "Open +7.50", "color": "b0619cff"}
}"#;

#[derive(Deserialize)]
struct Doc {
    as_of: String,
    levels: Vec<Row>,
    anchor: Option<Anchor>,
}

#[derive(Deserialize)]
struct Row {
    kind: String,
    line: Option<i64>,
    lower: Option<i64>,
    upper: Option<i64>,
    /// A band relative to the chart's ETH VWAP (catalog units).
    vwap_lower: Option<i64>,
    vwap_upper: Option<i64>,
    label: Option<String>,
    color: String,
    #[serde(default = "one")]
    weight: u8,
    #[serde(default)]
    dashed: bool,
}

fn one() -> u8 {
    1
}

#[derive(Deserialize)]
struct Anchor {
    at: String,
    offset: i64,
    label: String,
    color: String,
}

/// A recommended color per theme (`TypeStyle::light`): a theme token follows the theme itself;
/// a fixed color for dark has its darker counterpart for light. Strokes as the map gives them.
const fn themed(dark: Color, light: Option<Color>, line: LineType, width: u8) -> TypeStyle {
    TypeStyle {
        line: LineStyle {
            color: dark,
            line,
            width,
            opacity: 100,
        },
        light,
        ..TypeStyle::OWN
    }
}

/// Resistance and the anchored line: olive, clear of every color with a meaning in both
/// palettes (an amber would sit beside warn and the colorblind palette's orange down); deeper
/// on the light themes.
const OLIVE: (Color, Color) = (
    Color::Custom(Rgb::hex(0xbbdd44)),
    Color::Custom(Rgb::hex(0x777700)),
);
/// The pivot and support: a neutral gray, light on dark and deep on light (no blue in it: the
/// colorblind palette's up is blue).
const GRAY: (Color, Color) = (
    Color::Custom(Rgb::hex(0xc8c8c8)),
    Color::Custom(Rgb::hex(0x7a7a7a)),
);
/// The zones' and the VWAP band's edges: a dim gray on each theme, quieter than the candles
/// (a band's edges frame it; they aren't levels to read across the chart). Their labels fall
/// back to the theme's text (the dim gray isn't AA on a label).
const EDGE: (Color, Color) = (
    Color::Custom(Rgb::hex(0x6b707c)),
    Color::Custom(Rgb::hex(0xb4b8c1)),
);

/// The demo's level types (`LevelSource::types`): every kind its map uses. The lines in a
/// color per theme, bright on dark and deep on light, each clear of up and down in both
/// palettes (the theme's study teal and orchid sit beside the mint up and rose down, an amber
/// beside the colorblind orange down), so they are told apart by line type: pivot solid,
/// support dashed, resistance dash-dot, the anchored line dotted, all 1 px (none louder than
/// the last price). The zones: solid dim gray edges (quieter than the candles) around a
/// visible fill in each level's own color: a demand
/// zone labeled just under its low edge (`LabelAt::Low`), a supply zone (split from it, so
/// hiding zones hides both) just above its high edge (`LabelAt::High`), each with that edge's
/// price; the VWAP band's edges dotted. Labels are in each level's color, or theme text where that isn't AA on the
/// label (the light themes' grays and olive: the fallback).
pub static TYPES: [LevelType; 7] = [
    LevelType::line(
        "pivot",
        "Pivot",
        "Lines",
        themed(GRAY.0, Some(GRAY.1), LineType::Solid, 1),
    ),
    LevelType::line(
        "resist",
        "Resistance",
        "Lines",
        themed(OLIVE.0, Some(OLIVE.1), LineType::DashDot, 1),
    ),
    LevelType::line(
        "support",
        "Support",
        "Lines",
        themed(GRAY.0, Some(GRAY.1), LineType::Dashed, 1),
    ),
    LevelType::line(
        "anchor",
        "Anchored",
        "Lines",
        themed(OLIVE.0, Some(OLIVE.1), LineType::Dotted, 1),
    ),
    LevelType::zone(
        "zone",
        "Zone",
        "Zones",
        TypeStyle {
            fill: FillStyle {
                opacity: 40,
                ..TypeStyle::OWN.fill
            },
            label_at: lines_core::levels::LabelAt::Low,
            ..themed(EDGE.0, Some(EDGE.1), LineType::Solid, 1)
        },
    ),
    LevelType::zone(
        "supply",
        "Supply zone",
        "Zones",
        TypeStyle {
            fill: FillStyle {
                opacity: 40,
                ..TypeStyle::OWN.fill
            },
            label_at: lines_core::levels::LabelAt::High,
            ..themed(EDGE.0, Some(EDGE.1), LineType::Solid, 1)
        },
    )
    .child_of("zone"),
    LevelType::zone(
        "band",
        "VWAP band",
        "Zones",
        themed(EDGE.0, Some(EDGE.1), LineType::Dotted, 1),
    ),
];

pub struct DemoLevels;

struct DemoTransport;

impl LevelTransport for DemoTransport {
    fn fetch(&mut self) -> BoxFuture<'_, Result<Vec<LevelFrame>>> {
        Box::pin(async { Ok(vec![LevelFrame::new("demo/levels", BODY)]) })
    }
}

fn rgba(hex: &str) -> Result<u32, String> {
    u32::from_str_radix(hex, 16)
        .ok()
        .filter(|_| hex.len() == 8)
        .ok_or_else(|| format!("bad color {hex:?}"))
}

impl LevelSource for DemoLevels {
    fn id(&self) -> &'static str {
        ID
    }
    fn label(&self) -> &'static str {
        "Demo levels"
    }
    fn serves(&self, product: &Product) -> bool {
        matches!(product.symbol, "ES" | "MES")
    }
    fn refresh(&self) -> RefreshMode {
        RefreshMode::OneShot
    }
    /// Synthetic: screenshots draw it over any replay (`--demo-levels`).
    fn synthetic(&self) -> bool {
        true
    }
    fn stale_after(&self) -> Duration {
        Duration::from_secs(24 * 3600)
    }
    fn series_studies(&self) -> &'static [SeriesStudyDef] {
        SERIES_STUDIES
    }
    fn types(&self) -> &'static [LevelType] {
        &TYPES
    }
    fn settings(&self) -> &'static [SettingSpec] {
        &SETTINGS
    }
    fn choices(&self, key: &str) -> Option<Vec<(String, String)>> {
        (key == key::SYMBOL).then(|| {
            [("auto", "Auto"), ("ES", "ES"), ("MES", "MES")]
                .map(|(v, l)| (v.to_owned(), l.to_owned()))
                .to_vec()
        })
    }
    fn check(&self, values: &SettingValues) -> Vec<(&'static str, String)> {
        let url = value(values, key::FEED_URL);
        let remote = super::setting::is_on(value(values, key::ALLOW_REMOTE));
        if !remote && !super::setting::is_loopback_url(url) {
            vec![(
                key::FEED_URL,
                "Not this machine. Turn on Allow remote to use another address.".to_owned(),
            )]
        } else {
            Vec::new()
        }
    }
    fn connect(&self, _: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>> {
        Box::pin(async { Ok(Box::new(DemoTransport) as Box<dyn LevelTransport>) })
    }
    fn decode(
        &self,
        ctx: &LevelContext,
        prices: &dyn ChartPrices,
        _prev: Option<&LevelMap>,
        frames: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        let Some(f) = frames.iter().find(|f| f.topic == "demo/levels") else {
            return Ok(None);
        };
        let doc: Doc = serde_json::from_slice(&f.body).map_err(|e| e.to_string())?;
        let set = &ctx.settings;
        let bands = super::setting::is_on(value(set, key::BANDS));
        let stats = super::setting::is_on(value(set, key::STATS));
        // Hundredths (the spec's 2 places); the anchor's ticks.
        let width = super::setting::parse(&SETTINGS[6], value(set, key::BAND_WIDTH), None)
            .ok()
            .and_then(|w| w.replace('.', "").parse::<i64>().ok())
            .unwrap_or(100);
        let anchor_ticks = value(set, key::ANCHOR_OFFSET).trim().parse::<i64>().ok();
        let time = |s: &str| UtcMillis::parse_iso(s).ok_or_else(|| format!("bad time {s:?}"));
        let mut b = LevelMap::builder(ID, ctx.product.id, time(&doc.as_of)?);
        if let Some(c) = ctx.contract {
            b = b.contract(c);
        }
        for r in &doc.levels {
            let style = LevelStyle {
                rgba: rgba(&r.color)?,
                weight: r.weight,
                dashed: r.dashed,
            };
            let label = r.label.as_deref();
            if let (Some(lo), Some(hi)) = (r.vwap_lower, r.vwap_upper) {
                if !bands {
                    continue;
                }
                let w = width as f64 / 100.0;
                b = b.zone(
                    &r.kind,
                    LevelValue::VwapOffset(lo as f64 * w),
                    LevelValue::VwapOffset(hi as f64 * w),
                    label,
                    style,
                );
                continue;
            }
            b = match (r.line, r.lower, r.upper) {
                (Some(p), None, None) => b.line(&r.kind, LevelValue::Tick(Price(p)), label, style),
                (None, Some(lo), Some(hi)) => b.zone(
                    &r.kind,
                    LevelValue::Tick(Price(lo)),
                    LevelValue::Tick(Price(hi)),
                    label,
                    style,
                ),
                _ => {
                    return Err(format!(
                        "level of kind {:?}: need line or lower+upper",
                        r.kind
                    ));
                }
            };
        }
        // Anchored: the chart's open at `at`, plus the offset. Withheld if the chart has no
        // price there (no silent guess).
        if let Some(a) = &doc.anchor
            && let Some(p) = prices.price_at(time(&a.at)?, 0)
        {
            let offset = anchor_ticks.map_or(a.offset, |n| n * ctx.product.price.tick);
            b = b.level(
                "anchor",
                lines_core::levels::LevelShape::Line(LevelValue::Tick(Price(p.0 + offset))),
                Some(&a.label),
                LevelStyle {
                    rgba: rgba(&a.color)?,
                    weight: 1,
                    dashed: true,
                },
                LevelMeta {
                    as_of: Some(time(&a.at)?),
                    session: None,
                    provenance: Some("chart open at anchor + offset".into()),
                },
            );
        }
        // The synthetic series over the synthetic sessions (2026-11-05, 08:00-22:00 UTC).
        let (from, to) = (
            time("2026-11-05T08:00:00")?.0,
            time("2026-11-05T22:00:00")?.0,
        );
        let sin = |x: f64, p: f64| (x / p).sin();
        let series = [
            demo_series("osc.a", from, to, |x| 55.0 + 6.0 * sin(x, 47.0)),
            demo_series("osc.b", from, to, |x| 48.0 + 8.0 * sin(x + 30.0, 61.0)),
            demo_series("osc.c", from, to, |x| 66.0 + 5.0 * sin(x, 83.0)),
            demo_series("swing", from, to, |x| {
                4.0 * sin(x, 37.0) + 1.5 * sin(x, 11.0)
            }),
        ];
        for s in series {
            b = b.series(std::sync::Arc::new(s));
        }
        // The legend: a title and signed stats (synthetic words and numbers).
        let stat = |name: &str, text: &str, tone| lines_core::levels::LegendStat {
            name: name.into(),
            text: text.into(),
            tone,
        };
        b = b.legend(lines_core::levels::LevelLegend {
            title: Some("Synthetic".into()),
            stats: if stats {
                vec![
                    stat("Range", "12.50", lines_core::levels::StatTone::Plain),
                    stat("Bias", "+1.5", lines_core::levels::StatTone::Up),
                    stat("Skew", "\u{2212}0.4", lines_core::levels::StatTone::Down),
                ]
            } else {
                Vec::new()
            },
        });
        b.build(ctx.product.price.tick)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

// ---- a second source that crowds the demo's labels -------------------------------------

/// The crowding source's id.
pub const CROWD_ID: &str = "demo-crowd";

/// A second synthetic source for the same ES/MES charts (`--demo-crowd`): its labels sit at and
/// beside the demo's (one layout for every source on a chart: none may draw over another's),
/// half of them a low-priority type that gives way first.
pub const CROWD_BODY: &str = r#"{
  "as_of": "2026-11-05T13:00:00",
  "levels": [
    {"kind": "dyn",  "line": 601000, "label": "Dyn HP 6010",   "color": "5b8def"},
    {"kind": "det",  "line": 601025, "label": "Det IRL 6010.25", "color": "9a7fd1"},
    {"kind": "dyn",  "line": 602475, "label": "Dyn MHP 6024.75", "color": "5b8def"},
    {"kind": "det",  "line": 602500, "label": "Det YL 6025", "color": "9a7fd1"},
    {"kind": "det",  "line": 599775, "label": "Det RL 5997.75", "color": "9a7fd1"},
    {"kind": "dyn",  "line": 599500, "label": "Dyn VAL 5995", "color": "5b8def"}
  ]
}"#;

/// The crowding source's types: `dyn` at Normal priority, `det` (a detector's guesses) Low.
pub static CROWD_TYPES: [LevelType; 2] = [
    LevelType::line(
        "dyn",
        "Dynamic",
        "Lines",
        themed(GRAY.0, Some(GRAY.1), LineType::Solid, 1),
    ),
    LevelType::line(
        "det",
        "Detector",
        "Lines",
        themed(OLIVE.0, Some(OLIVE.1), LineType::Dashed, 1),
    )
    .label_priority(lines_core::levels::LabelPriority::Low),
];

pub struct DemoCrowd;

struct CrowdTransport;

impl LevelTransport for CrowdTransport {
    fn fetch(&mut self) -> BoxFuture<'_, Result<Vec<LevelFrame>>> {
        Box::pin(async { Ok(vec![LevelFrame::new("demo-crowd/levels", CROWD_BODY)]) })
    }
}

#[derive(Deserialize)]
struct CrowdDoc {
    as_of: String,
    levels: Vec<CrowdRow>,
}

#[derive(Deserialize)]
struct CrowdRow {
    kind: String,
    line: i64,
    label: String,
    color: String,
}

impl LevelSource for DemoCrowd {
    fn id(&self) -> &'static str {
        CROWD_ID
    }
    fn label(&self) -> &'static str {
        "Crowd levels"
    }
    fn serves(&self, product: &Product) -> bool {
        matches!(product.symbol, "ES" | "MES")
    }
    fn refresh(&self) -> RefreshMode {
        RefreshMode::OneShot
    }
    fn synthetic(&self) -> bool {
        true
    }
    fn stale_after(&self) -> Duration {
        Duration::from_secs(24 * 3600)
    }
    fn types(&self) -> &'static [LevelType] {
        &CROWD_TYPES
    }
    fn connect(&self, _: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>> {
        Box::pin(async { Ok(Box::new(CrowdTransport) as Box<dyn LevelTransport>) })
    }
    fn decode(
        &self,
        ctx: &LevelContext,
        _prices: &dyn ChartPrices,
        _prev: Option<&LevelMap>,
        frames: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        let Some(f) = frames.iter().find(|f| f.topic == "demo-crowd/levels") else {
            return Ok(None);
        };
        let doc: CrowdDoc = serde_json::from_slice(&f.body).map_err(|e| e.to_string())?;
        let at = UtcMillis::parse_iso(&doc.as_of).ok_or("bad time")?;
        let mut b = LevelMap::builder(CROWD_ID, ctx.product.id, at);
        if let Some(c) = ctx.contract {
            b = b.contract(c);
        }
        for r in &doc.levels {
            let style = LevelStyle {
                rgba: rgba(&format!("{}ff", r.color))?,
                weight: 1,
                dashed: r.kind == "det",
            };
            b = b.line(
                &r.kind,
                LevelValue::Tick(Price(r.line)),
                Some(&r.label),
                style,
            );
        }
        b.build(ctx.product.price.tick)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

// ---- a synthetic event: lines that begin at its bar, markers, a box, one group -------------

/// The event source's id.
pub const EVENT_ID: &str = "demo-event";
/// Its one event's group id.
pub const EVENT_GROUP: &str = "demo/event-1";

/// A synthetic event on the synthetic ES session (`--demo-event`): an anchor (A) at 15:58 ET
/// (20:58 UTC), a first-wave extreme (E1) at 16:06 and a V low (hollow: unconfirmed) at 16:12,
/// joined by a dashed path; the event's lines begin at their own bars (A's, the impact and the
/// target lines at the anchor's, E1's at its own, the run's at V's), never earlier; a box from
/// the anchor to the target, from the anchor's bar to E1's. All one group: pointing at any of
/// them highlights the event. A second, ungrouped line across the whole plot stays as lines
/// always were.
pub static EVENT_TYPES: [LevelType; 6] = [
    LevelType::line(
        "ev-anchor",
        "Event anchor",
        "Event",
        themed(GRAY.0, Some(GRAY.1), LineType::Solid, 1),
    ),
    LevelType::line(
        "ev-e1",
        "First-wave extreme",
        "Event",
        themed(GRAY.0, Some(GRAY.1), LineType::Solid, 1),
    ),
    LevelType::line(
        "ev-impact",
        "Impact line",
        "Event",
        themed(OLIVE.0, Some(OLIVE.1), LineType::Dashed, 1),
    ),
    LevelType::line(
        "ev-target",
        "Target line",
        "Event",
        themed(OLIVE.0, Some(OLIVE.1), LineType::Solid, 1),
    ),
    LevelType::line(
        "ev-run",
        "Run line",
        "Event",
        themed(GRAY.0, Some(GRAY.1), LineType::Dashed, 1),
    ),
    LevelType::zone(
        "ev-box",
        "Anchor to target",
        "Event",
        themed(EDGE.0, Some(EDGE.1), LineType::Solid, 1),
    ),
];

pub struct DemoEvent;

struct EventTransport;

impl LevelTransport for EventTransport {
    fn fetch(&mut self) -> BoxFuture<'_, Result<Vec<LevelFrame>>> {
        Box::pin(async { Ok(vec![LevelFrame::new("demo-event/levels", "{}")]) })
    }
}

impl LevelSource for DemoEvent {
    fn id(&self) -> &'static str {
        EVENT_ID
    }
    fn label(&self) -> &'static str {
        "Event levels"
    }
    fn serves(&self, product: &Product) -> bool {
        matches!(product.symbol, "ES" | "MES")
    }
    fn refresh(&self) -> RefreshMode {
        RefreshMode::OneShot
    }
    fn synthetic(&self) -> bool {
        true
    }
    fn stale_after(&self) -> Duration {
        Duration::from_secs(24 * 3600)
    }
    fn types(&self) -> &'static [LevelType] {
        &EVENT_TYPES
    }
    fn connect(&self, _: &LevelContext) -> BoxFuture<'static, Result<Box<dyn LevelTransport>>> {
        Box::pin(async { Ok(Box::new(EventTransport) as Box<dyn LevelTransport>) })
    }
    fn decode(
        &self,
        ctx: &LevelContext,
        _prices: &dyn ChartPrices,
        _prev: Option<&LevelMap>,
        frames: &[LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        if !frames.iter().any(|f| f.topic == "demo-event/levels") {
            return Ok(None);
        }
        let t = |s: &str| UtcMillis::parse_iso(s).ok_or_else(|| format!("bad time {s:?}"));
        let (a_at, e1_at, v_at) = (
            t("2026-11-05T20:58:00")?,
            t("2026-11-05T21:06:00")?,
            t("2026-11-05T21:12:00")?,
        );
        let style = |rgba: u32, dashed: bool| LevelStyle {
            rgba,
            weight: 1,
            dashed,
        };
        let tick = |p: i64| LevelValue::Tick(Price(p));
        let mut b = LevelMap::builder(EVENT_ID, ctx.product.id, t("2026-11-05T13:00:00")?);
        if let Some(c) = ctx.contract {
            b = b.contract(c);
        }
        b = b
            .line(
                "ev-anchor",
                tick(600_500),
                Some("A 6005"),
                style(0x838799ff, false),
            )
            .from(a_at)
            .group(EVENT_GROUP)
            .line(
                "ev-e1",
                tick(602_200),
                Some("E1 6022"),
                style(0x838799ff, false),
            )
            .from(e1_at)
            .group(EVENT_GROUP)
            .line(
                "ev-impact",
                tick(601_350),
                Some("Impact 6013.50"),
                style(0xa87719ff, true),
            )
            .from(a_at)
            .group(EVENT_GROUP)
            .line(
                "ev-target",
                tick(602_400),
                Some("Target 6024"),
                style(0xa87719ff, false),
            )
            .from(a_at)
            .group(EVENT_GROUP)
            .zone(
                "ev-box",
                tick(600_500),
                tick(602_400),
                None,
                style(0x868686ff, false),
            )
            .from(a_at)
            .to(e1_at)
            .group(EVENT_GROUP)
            // The run's line: at V's low, from V's bar (when the run ended).
            .line(
                "ev-run",
                tick(601_600),
                Some("Run 6016"),
                style(0xc8c8c8ff, true),
            )
            .from(v_at)
            .group(EVENT_GROUP)
            .marker(a_at, tick(600_500), Some("A"))
            .group(EVENT_GROUP)
            .marker(e1_at, tick(602_200), Some("E1"))
            .group(EVENT_GROUP)
            .joined()
            .marker(v_at, tick(601_600), Some("V"))
            .group(EVENT_GROUP)
            .joined()
            .hollow()
            // An ordinary level beside the event: across the whole plot, as always.
            .line(
                "ev-anchor",
                tick(599_300),
                Some("Prior A 5993"),
                style(0x838799ff, false),
            );
        b.build(ctx.product.price.tick)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}
