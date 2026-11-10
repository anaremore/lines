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
    SeriesStudyDef, TypeStyle,
};

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
    {"kind": "zone",   "lower": 601750, "upper": 602250, "label": "Supply", "color": "888800ff"},
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
/// The pivot, support and the zones' and VWAP band's edges: a neutral gray, light on dark and deep on
/// light (no blue in it: the colorblind palette's up is blue).
const GRAY: (Color, Color) = (
    Color::Custom(Rgb::hex(0xc8c8c8)),
    Color::Custom(Rgb::hex(0x7a7a7a)),
);

/// The demo's level types (`LevelSource::types`): every kind its map uses. The lines in a
/// color per theme, bright on dark and deep on light, each clear of up and down in both
/// palettes (the theme's study teal and orchid sit beside the mint up and rose down, an amber
/// beside the colorblind orange down), so they are told apart by line type: pivot solid,
/// support dashed, resistance dash-dot, the anchored line dotted, all 1 px (none louder than
/// the last price). The zones: solid gray edges (drawn whole, 3:1 on both charts; a level's
/// own look draws edges at 60 %) around a visible fill in each level's own color, each
/// labeled just under its low edge (`LabelAt::Low`, with that edge's price); the VWAP band's
/// edges dotted. Labels are in each level's color, or theme text where that isn't AA on the
/// label (the light themes' grays and olive: the fallback).
pub static TYPES: [LevelType; 6] = [
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
            ..themed(GRAY.0, Some(GRAY.1), LineType::Solid, 1)
        },
    ),
    LevelType::zone(
        "band",
        "VWAP band",
        "Zones",
        themed(GRAY.0, Some(GRAY.1), LineType::Dotted, 1),
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
                b = b.zone(
                    &r.kind,
                    LevelValue::VwapOffset(lo as f64),
                    LevelValue::VwapOffset(hi as f64),
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
            b = b.level(
                "anchor",
                lines_core::levels::LevelShape::Line(LevelValue::Tick(Price(p.0 + a.offset))),
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
            stats: vec![
                stat("Range", "12.50", lines_core::levels::StatTone::Plain),
                stat("Bias", "+1.5", lines_core::levels::StatTone::Up),
                stat("Skew", "\u{2212}0.4", lines_core::levels::StatTone::Down),
            ],
        });
        b.build(ctx.product.price.tick)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}
