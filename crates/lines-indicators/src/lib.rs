//! The indicator engine (roadmap 5c, decision 0017) and built-in indicators.
//!
//! Pure: no GPUI, Tokio or I/O. The runtime that feeds it (an indicator thread, never the UI or
//! a feed thread) lives in `lines-feed::indicators`.
//!
//! - [`graph`]: the series graph: nodes keyed by (feed, spec), shared, acyclic, evaluated
//!   incrementally and deterministically; the [`Indicator`] trait (and how it maps to the
//!   plugin WIT world, decision 0008).
//! - [`series`]: the base series each feed's indicators start from: stitched bars and exact
//!   per-bucket trade sums, with exact vs approximate (bar-derived) buckets.
//! - [`vwap`]: VWAP with ETH/RTH/weekly/monthly/anchored resets and ±1/2/3σ bands.
//! - [`render`]: allocation-free chart geometry for a VWAP.
//! - [`IndicatorConfig`]: the serialisable per-chart configuration (persisted with the
//!   workspace).

pub mod delta;
pub mod graph;
pub mod render;
pub mod series;
pub mod vwap;

pub use delta::{CumDelta, DeltaPoint, DeltaReset, DeltaSpec};
pub use graph::{FeedId, Graph, GraphError, Indicator, Output, SeriesKey, SeriesType, Spec};
pub use series::{DeltaSums, Input, MarketSeries, Sums};
pub use vwap::{Bands, Point, Reset, Vwap, VwapSpec};

use serde::{Deserialize, Serialize};

/// A study's user styles (decision 0026), by part (`"line"`, `"bands"`, ...): each part a
/// flat object of optional fields the app reads and writes (`lines-app` `study_style`). Kept
/// as JSON so parts and fields a newer build wrote survive this one saving the study.
pub type PartStyles = std::collections::BTreeMap<String, serde_json::Value>;

/// One indicator on a chart, as saved with the workspace (decision 0016 persistence; the
/// workspaces milestone stores a `Vec` of these per chart pane). Stable, versioned by serde
/// field names; unknown kinds fail to load rather than silently vanish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IndicatorConfig {
    Vwap {
        #[serde(flatten)]
        spec: VwapSpec,
        /// Index into the theme's study colours.
        #[serde(default)]
        color: u8,
        /// How it is shown (line width, label, hidden): optional, defaulted.
        #[serde(flatten, default)]
        display: Display,
        /// User styles of its line, bands and band fill. Optional.
        #[serde(default, skip_serializing_if = "PartStyles::is_empty")]
        style: PartStyles,
    },
    /// Cumulative delta (roadmap 10e), drawn as candles in a sub-pane below the price plot.
    CumDelta {
        #[serde(flatten)]
        spec: DeltaSpec,
        #[serde(flatten, default)]
        display: Display,
        /// User styles of its rising and falling candles. Optional.
        #[serde(default, skip_serializing_if = "PartStyles::is_empty")]
        style: PartStyles,
    },
    /// Traded volume per bar: summed from the chart's own bars (the same trades as the
    /// candles), drawn in its own lower pane or along the plot's bottom. Needs no engine node.
    Volume {
        #[serde(flatten)]
        spec: VolumeSpec,
        #[serde(flatten, default)]
        display: Display,
        /// User styles of its bars, up/buy and down/sell bars and average line. Optional.
        #[serde(default, skip_serializing_if = "PartStyles::is_empty")]
        style: PartStyles,
    },
}

/// The Volume study's options; every one off by default (plain bars in their own pane).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeSpec {
    /// Bars in candle colors: rising filled, falling hollow.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub color_by_candle: bool,
    /// An average line over `average_len` bars.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub average: bool,
    #[serde(skip_serializing_if = "VolumeSpec::default_len")]
    pub average_len: u16,
    /// Stacked by aggressor side (trades without a side count only toward the total).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub split: bool,
    /// Short bars along the bottom of the price plot instead of a lower pane.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overlay: bool,
}

impl VolumeSpec {
    /// The average's length when first turned on.
    pub const DEFAULT_LEN: u16 = 20;
    /// Lengths kept in range (a damaged file never asks for an empty or huge window).
    pub const MAX_LEN: u16 = 500;

    fn default_len(n: &u16) -> bool {
        *n == Self::DEFAULT_LEN
    }

    /// The average's length, in range.
    pub fn len(&self) -> usize {
        usize::from(self.average_len.clamp(1, Self::MAX_LEN))
    }
}

impl Default for VolumeSpec {
    fn default() -> Self {
        VolumeSpec {
            color_by_candle: false,
            average: false,
            average_len: Self::DEFAULT_LEN,
            split: false,
            overlay: false,
        }
    }
}

/// How a study is shown on a chart. Hiding keeps the study (and its configuration); a hidden
/// study is not computed or drawn until shown again. Every field is optional in files.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    /// Line width step, 1..=4 (thin, normal, thick, heavy; the app draws a VWAP at 1.25, 1.75,
    /// 2.5 and 3.25 px).
    #[serde(skip_serializing_if = "Display::normal_width")]
    pub width: u8,
    /// Shown in the chart's legend.
    #[serde(skip_serializing_if = "Display::yes")]
    pub label: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

impl Default for Display {
    fn default() -> Self {
        Display {
            width: 2,
            label: true,
            hidden: false,
        }
    }
}

impl Display {
    fn normal_width(w: &u8) -> bool {
        *w == 2
    }
    fn yes(b: &bool) -> bool {
        *b
    }
}

impl IndicatorConfig {
    /// How it is shown (every kind has one).
    pub fn display(&self) -> &Display {
        match self {
            IndicatorConfig::Vwap { display, .. }
            | IndicatorConfig::CumDelta { display, .. }
            | IndicatorConfig::Volume { display, .. } => display,
        }
    }

    pub fn display_mut(&mut self) -> &mut Display {
        match self {
            IndicatorConfig::Vwap { display, .. }
            | IndicatorConfig::CumDelta { display, .. }
            | IndicatorConfig::Volume { display, .. } => display,
        }
    }

    /// Its user styles (every kind has them).
    pub fn style(&self) -> &PartStyles {
        match self {
            IndicatorConfig::Vwap { style, .. }
            | IndicatorConfig::CumDelta { style, .. }
            | IndicatorConfig::Volume { style, .. } => style,
        }
    }

    pub fn style_mut(&mut self) -> &mut PartStyles {
        match self {
            IndicatorConfig::Vwap { style, .. }
            | IndicatorConfig::CumDelta { style, .. }
            | IndicatorConfig::Volume { style, .. } => style,
        }
    }

    /// The same study but for its styles (a restyle needs no new engine node): its `style`
    /// and, for a VWAP, its line's palette `color` and `display.width` (the style editor
    /// writes the line's color and width there).
    pub fn same_but_style(&self, other: &IndicatorConfig) -> bool {
        fn unstyled(c: &IndicatorConfig) -> IndicatorConfig {
            let mut c = c.clone();
            c.style_mut().clear();
            if let IndicatorConfig::Vwap { color, display, .. } = &mut c {
                *color = 0;
                display.width = Display::default().width;
            }
            c
        }
        unstyled(self) == unstyled(other)
    }

    /// Drawn in a lower pane below the price plot (not in it): the study's own declaration,
    /// which is all the chart's lower-pane stack needs (`lines-app` `subpane`).
    pub fn in_subpane(&self) -> bool {
        match self {
            IndicatorConfig::Vwap { .. } => false,
            IndicatorConfig::CumDelta { .. } => true,
            IndicatorConfig::Volume { spec, .. } => !spec.overlay,
        }
    }

    /// The graph spec for a chart of bar period `period_ms`; `None` for a study drawn from
    /// the chart's own bars (Volume), which holds no engine node.
    pub fn spec(&self, period_ms: i64) -> Option<Spec> {
        match self {
            IndicatorConfig::Vwap { spec, .. } => Some(Spec::Vwap {
                period_ms,
                vwap: *spec,
            }),
            IndicatorConfig::CumDelta { spec, .. } => Some(Spec::CumDelta {
                period_ms,
                delta: *spec,
            }),
            IndicatorConfig::Volume { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N1: a VWAP's line color and width are styles; its reset, bands and visibility aren't.
    #[test]
    fn same_but_style_ignores_a_vwaps_color_and_width() {
        let base = IndicatorConfig::Vwap {
            spec: VwapSpec::default(),
            color: 0,
            display: Display::default(),
            style: Default::default(),
        };
        let mut restyled = base.clone();
        if let IndicatorConfig::Vwap {
            color,
            display,
            style,
            ..
        } = &mut restyled
        {
            *color = 2;
            display.width = 4;
            style.insert("fill".into(), serde_json::json!({"opacity": 80}));
        }
        assert!(base.same_but_style(&restyled));
        let mut hidden = base.clone();
        hidden.display_mut().hidden = true;
        assert!(!base.same_but_style(&hidden));
        let mut rth = base.clone();
        if let IndicatorConfig::Vwap { spec, .. } = &mut rth {
            spec.reset = Reset::Rth;
        }
        assert!(!base.same_but_style(&rth));
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfgs = vec![
            IndicatorConfig::Vwap {
                spec: VwapSpec::default(),
                color: 0,
                display: Default::default(),
                style: Default::default(),
            },
            IndicatorConfig::Vwap {
                spec: VwapSpec {
                    reset: Reset::Anchored {
                        from: 1_793_973_600_000,
                    },
                    bands: Bands(0b011),
                },
                color: 2,
                display: Default::default(),
                style: Default::default(),
            },
        ];
        let mut cfgs = cfgs;
        cfgs.push(IndicatorConfig::CumDelta {
            spec: DeltaSpec {
                reset: DeltaReset::Rth,
            },
            display: Default::default(),
            style: Default::default(),
        });
        let json = serde_json::to_string(&cfgs).unwrap();
        assert!(json.contains(r#""kind":"vwap""#), "{json}");
        assert!(
            json.contains(r#"{"kind":"cum_delta","reset":"rth"}"#),
            "{json}"
        );
        let back: Vec<IndicatorConfig> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfgs);
        // The reset is optional (ETH).
        let bare: IndicatorConfig = serde_json::from_str(r#"{"kind":"cum_delta"}"#).unwrap();
        assert!(
            matches!(bare, IndicatorConfig::CumDelta { spec, .. } if spec.reset == DeltaReset::Eth)
        );
    }

    /// Volume: every option is optional and off by default (`{"kind":"volume"}`); the
    /// average's length is written only when not 20, and a damaged one is kept in range.
    #[test]
    fn volume_options_are_optional() {
        let plain = IndicatorConfig::Volume {
            spec: VolumeSpec::default(),
            display: Default::default(),
            style: Default::default(),
        };
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"kind":"volume"}"#
        );
        let bare: IndicatorConfig = serde_json::from_str(r#"{"kind":"volume"}"#).unwrap();
        assert_eq!(bare, plain);
        assert!(plain.in_subpane() && plain.spec(60_000).is_none());
        let all = IndicatorConfig::Volume {
            spec: VolumeSpec {
                color_by_candle: true,
                average: true,
                average_len: 50,
                split: true,
                overlay: true,
            },
            display: Default::default(),
            style: Default::default(),
        };
        let json = serde_json::to_string(&all).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"volume","color_by_candle":true,"average":true,"average_len":50,"split":true,"overlay":true}"#
        );
        assert_eq!(serde_json::from_str::<IndicatorConfig>(&json).unwrap(), all);
        assert!(!all.in_subpane(), "an overlay draws in the price plot");
        let zero: IndicatorConfig =
            serde_json::from_str(r#"{"kind":"volume","average":true,"average_len":0}"#).unwrap();
        let IndicatorConfig::Volume { spec, .. } = zero else {
            unreachable!()
        };
        assert_eq!(spec.len(), 1);
    }

    /// Styles are an optional nested field: a config without them writes none and loads as
    /// before, and a newer build's parts and fields come back unchanged.
    #[test]
    fn styles_are_optional_and_kept_whole() {
        let plain = IndicatorConfig::Vwap {
            spec: VwapSpec::default(),
            color: 1,
            display: Default::default(),
            style: Default::default(),
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("style"), "{json}");
        let styled = r##"{"kind":"vwap","reset":"eth","bands":0,"color":1,
            "style":{"line":{"color":"#f719cb","glow":2},"halo":{"radius":3}}}"##;
        let c: IndicatorConfig = serde_json::from_str(styled).unwrap();
        assert!(c.same_but_style(&plain));
        assert_eq!(c.style()["line"]["glow"], 2);
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["style"]["halo"]["radius"], 3);
        assert_eq!(back["style"]["line"]["color"], "#f719cb");
    }
}
