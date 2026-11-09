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

use lines_core::style::{LineStyle, LineType};

use super::{
    BoxFuture, ChartPrices, LevelContext, LevelFrame, LevelMap, LevelMeta, LevelSource, LevelStyle,
    LevelTransport, LevelType, LevelValue, RefreshMode, TypeStyle,
};

pub const ID: &str = "demo";

/// The demo map, in catalog price units (ES: hundredths of a point). Synthetic.
pub const BODY: &str = r#"{
  "as_of": "2026-11-05T13:00:00",
  "levels": [
    {"kind": "pivot",  "line": 601000, "label": "Pivot 6010",  "color": "c4c8d6ff", "weight": 2},
    {"kind": "resist", "line": 602500, "label": "R1 6025",     "color": "e0a84aff", "weight": 1},
    {"kind": "support","line": 599750, "label": "S1 5997.50",  "color": "4cc4c9ff", "weight": 1, "dashed": true},
    {"kind": "zone",   "lower": 601750, "upper": 602250, "label": "Supply", "color": "e0a84aff"},
    {"kind": "zone",   "lower": 599000, "upper": 599500, "label": "Demand", "color": "4cc4c9ff"},
    {"kind": "band",   "vwap_lower": -750, "vwap_upper": 750, "label": "VWAP ±7.50", "color": "b8bccbff", "dashed": true}
  ],
  "anchor": {"at": "2026-11-05T14:30:00", "offset": 750, "label": "Open +7.50", "color": "d98ac4ff"}
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

/// Its own colors with the strokes its map gives each kind (pivot 2 px; support, the VWAP band
/// and the anchor dashed).
const fn own(line: LineType, width: u8) -> TypeStyle {
    TypeStyle {
        line: LineStyle {
            line,
            width,
            ..TypeStyle::OWN.line
        },
        ..TypeStyle::OWN
    }
}

/// The demo's level types (`LevelSource::types`): every kind its map uses, in the levels'
/// own colors.
pub static TYPES: [LevelType; 6] = [
    LevelType::line("pivot", "Pivot", "Lines", own(LineType::Solid, 2)),
    LevelType::line("resist", "Resistance", "Lines", own(LineType::Solid, 1)),
    LevelType::line("support", "Support", "Lines", own(LineType::Dashed, 1)),
    LevelType::line("anchor", "Anchored", "Lines", own(LineType::Dashed, 1)),
    LevelType::zone("zone", "Zone", "Zones", own(LineType::Solid, 1)),
    LevelType::zone("band", "VWAP band", "Zones", own(LineType::Dashed, 1)),
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
        b.build(ctx.product.price.tick)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}
