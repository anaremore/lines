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

pub mod graph;
pub mod render;
pub mod series;
pub mod vwap;

pub use graph::{FeedId, Graph, GraphError, Indicator, Output, SeriesKey, SeriesType, Spec};
pub use series::{Input, MarketSeries, Sums};
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
    },
}

impl IndicatorConfig {
    /// The graph spec for a chart of bar period `period_ms`.
    pub fn spec(&self, period_ms: i64) -> Spec {
        match self {
            IndicatorConfig::Vwap { spec, .. } => Spec::Vwap {
                period_ms,
                vwap: *spec,
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
            },
            IndicatorConfig::Vwap {
                spec: VwapSpec {
                    reset: Reset::Anchored {
                        from: 1_793_973_600_000,
                    },
                    bands: Bands(0b011),
                },
                color: 2,
            },
        ];
        let json = serde_json::to_string(&cfgs).unwrap();
        assert!(json.contains(r#""kind":"vwap""#), "{json}");
        let back: Vec<IndicatorConfig> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfgs);
    }
}
