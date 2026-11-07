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
    },
    /// Cumulative delta (roadmap 10e), drawn as candles in a sub-pane below the price plot.
    CumDelta {
        #[serde(flatten)]
        spec: DeltaSpec,
        #[serde(flatten, default)]
        display: Display,
    },
}

/// How a study is shown on a chart. Hiding keeps the study (and its configuration); a hidden
/// study is not computed or drawn until shown again. Every field is optional in files.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    /// Line width step, 1..=3 (thin, normal, thick).
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
            IndicatorConfig::Vwap { display, .. } | IndicatorConfig::CumDelta { display, .. } => {
                display
            }
        }
    }

    pub fn display_mut(&mut self) -> &mut Display {
        match self {
            IndicatorConfig::Vwap { display, .. } | IndicatorConfig::CumDelta { display, .. } => {
                display
            }
        }
    }

    /// Drawn in a sub-pane below the price plot (not in it).
    pub fn in_subpane(&self) -> bool {
        matches!(self, IndicatorConfig::CumDelta { .. })
    }

    /// The graph spec for a chart of bar period `period_ms`.
    pub fn spec(&self, period_ms: i64) -> Spec {
        match self {
            IndicatorConfig::Vwap { spec, .. } => Spec::Vwap {
                period_ms,
                vwap: *spec,
            },
            IndicatorConfig::CumDelta { spec, .. } => Spec::CumDelta {
                period_ms,
                delta: *spec,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_json() {
        let cfgs = vec![
            IndicatorConfig::Vwap {
                spec: VwapSpec::default(),
                color: 0,
                display: Default::default(),
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
            },
        ];
        let mut cfgs = cfgs;
        cfgs.push(IndicatorConfig::CumDelta {
            spec: DeltaSpec {
                reset: DeltaReset::Rth,
            },
            display: Default::default(),
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
}
