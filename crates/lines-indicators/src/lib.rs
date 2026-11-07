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
//! - [`bars`]: the chart's bars (any timeframe, tick bars too) as an engine series.
//! - [`average`]: SMA, EMA and Bollinger Bands on the chart's bars.
//! - [`render`]: allocation-free chart geometry for line studies (VWAP, averages).
//! - [`IndicatorConfig`]: the serialisable per-chart configuration (persisted with the
//!   workspace).

pub mod average;
pub mod bars;
pub mod delta;
pub mod graph;
pub mod render;
pub mod series;
pub mod vwap;

pub use average::{Average, AverageKind, AverageSpec, Sigma, Source};
pub use bars::{BarsOut, ChartBars};
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
    /// Simple moving average of the chart's bars (roadmap 10a).
    Sma {
        #[serde(flatten)]
        params: AverageParams,
        #[serde(default)]
        color: u8,
        #[serde(flatten, default)]
        display: Display,
    },
    /// Exponential moving average of the chart's bars.
    Ema {
        #[serde(flatten)]
        params: AverageParams,
        #[serde(default)]
        color: u8,
        #[serde(flatten, default)]
        display: Display,
    },
    /// Bollinger Bands: the SMA plus and minus `sigma` standard deviations.
    Bollinger {
        #[serde(flatten)]
        params: AverageParams,
        /// Optional (2 sigma).
        #[serde(default)]
        sigma: Sigma,
        #[serde(default)]
        color: u8,
        #[serde(flatten, default)]
        display: Display,
    },
}

/// An average's saved length and source (both optional in files: 20 bars of closes).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AverageParams {
    #[serde(default = "AverageParams::default_length")]
    pub length: u16,
    #[serde(default, skip_serializing_if = "AverageParams::is_close")]
    pub source: Source,
}

impl Default for AverageParams {
    fn default() -> Self {
        AverageParams {
            length: Self::default_length(),
            source: Source::Close,
        }
    }
}

impl AverageParams {
    fn default_length() -> u16 {
        20
    }
    fn is_close(s: &Source) -> bool {
        *s == Source::Close
    }
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
            IndicatorConfig::Vwap { display, .. }
            | IndicatorConfig::CumDelta { display, .. }
            | IndicatorConfig::Sma { display, .. }
            | IndicatorConfig::Ema { display, .. }
            | IndicatorConfig::Bollinger { display, .. } => display,
        }
    }

    pub fn display_mut(&mut self) -> &mut Display {
        match self {
            IndicatorConfig::Vwap { display, .. }
            | IndicatorConfig::CumDelta { display, .. }
            | IndicatorConfig::Sma { display, .. }
            | IndicatorConfig::Ema { display, .. }
            | IndicatorConfig::Bollinger { display, .. } => display,
        }
    }

    /// The theme study color index of a line study (VWAP, averages).
    pub fn color(&self) -> Option<u8> {
        match self {
            IndicatorConfig::Vwap { color, .. }
            | IndicatorConfig::Sma { color, .. }
            | IndicatorConfig::Ema { color, .. }
            | IndicatorConfig::Bollinger { color, .. } => Some(*color),
            IndicatorConfig::CumDelta { .. } => None,
        }
    }

    pub fn color_mut(&mut self) -> Option<&mut u8> {
        match self {
            IndicatorConfig::Vwap { color, .. }
            | IndicatorConfig::Sma { color, .. }
            | IndicatorConfig::Ema { color, .. }
            | IndicatorConfig::Bollinger { color, .. } => Some(color),
            IndicatorConfig::CumDelta { .. } => None,
        }
    }

    /// An average's (SMA, EMA, Bollinger Bands) engine spec, validated.
    pub fn average(&self) -> Option<AverageSpec> {
        let (kind, p, sigma) = match self {
            IndicatorConfig::Sma { params, .. } => (AverageKind::Sma, params, Sigma::TWO),
            IndicatorConfig::Ema { params, .. } => (AverageKind::Ema, params, Sigma::TWO),
            IndicatorConfig::Bollinger { params, sigma, .. } => {
                (AverageKind::Bollinger, params, *sigma)
            }
            _ => return None,
        };
        Some(AverageSpec::new(kind, p.length, p.source, sigma))
    }

    pub fn average_params_mut(&mut self) -> Option<&mut AverageParams> {
        match self {
            IndicatorConfig::Sma { params, .. }
            | IndicatorConfig::Ema { params, .. }
            | IndicatorConfig::Bollinger { params, .. } => Some(params),
            _ => None,
        }
    }

    /// A new average of `kind` (length 20 of closes; Bollinger 2 sigma).
    pub fn new_average(kind: AverageKind, color: u8) -> IndicatorConfig {
        let (params, display) = (AverageParams::default(), Display::default());
        match kind {
            AverageKind::Sma => IndicatorConfig::Sma {
                params,
                color,
                display,
            },
            AverageKind::Ema => IndicatorConfig::Ema {
                params,
                color,
                display,
            },
            AverageKind::Bollinger => IndicatorConfig::Bollinger {
                params,
                sigma: Sigma::TWO,
                color,
                display,
            },
        }
    }

    /// Computed on the chart's bars (so it follows the chart's timeframe).
    pub fn on_chart_bars(&self) -> bool {
        self.average().is_some()
    }

    /// Drawn in a sub-pane below the price plot (not in it).
    pub fn in_subpane(&self) -> bool {
        matches!(self, IndicatorConfig::CumDelta { .. })
    }

    /// The graph spec on a feed whose base series has bar period `period_ms`, for a chart
    /// showing `bars` (bar studies follow them; VWAP and cumulative delta are computed per
    /// base bar and sampled per chart bar when drawn).
    pub fn spec(&self, period_ms: i64, bars: ChartBars) -> Spec {
        if let Some(average) = self.average() {
            return Spec::Average {
                period_ms,
                bars,
                average,
            };
        }
        match self {
            IndicatorConfig::Vwap { spec, .. } => Spec::Vwap {
                period_ms,
                vwap: *spec,
            },
            IndicatorConfig::CumDelta { spec, .. } => Spec::CumDelta {
                period_ms,
                delta: *spec,
            },
            IndicatorConfig::Sma { .. }
            | IndicatorConfig::Ema { .. }
            | IndicatorConfig::Bollinger { .. } => unreachable!("averages return above"),
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

    /// Roadmap 10a: averages are optional-field entries in the chart's indicator list.
    #[test]
    fn averages_round_trip_with_optional_fields() {
        let ema = IndicatorConfig::new_average(AverageKind::Ema, 1);
        let json = serde_json::to_string(&ema).unwrap();
        assert_eq!(json, r#"{"kind":"ema","length":20,"color":1}"#);
        let mut bb = IndicatorConfig::new_average(AverageKind::Bollinger, 2);
        if let IndicatorConfig::Bollinger { params, sigma, .. } = &mut bb {
            params.source = Source::Hlc3;
            *sigma = Sigma(25);
        }
        let json = serde_json::to_string(&bb).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"bollinger","length":20,"source":"hlc3","sigma":2.5,"color":2}"#
        );
        assert_eq!(serde_json::from_str::<IndicatorConfig>(&json).unwrap(), bb);
        // Everything but the kind is optional.
        let bare: IndicatorConfig = serde_json::from_str(r#"{"kind":"sma"}"#).unwrap();
        assert_eq!(bare, IndicatorConfig::new_average(AverageKind::Sma, 0));
        let bare: IndicatorConfig = serde_json::from_str(r#"{"kind":"bollinger"}"#).unwrap();
        assert_eq!(bare.average().unwrap().sigma, Sigma::TWO);
        // A damaged length is clamped when the spec is made, never a 0-bar window.
        let bad: IndicatorConfig = serde_json::from_str(r#"{"kind":"ema","length":0}"#).unwrap();
        assert_eq!(bad.average().unwrap().length, 1);
        // Bar studies follow the chart's bars.
        let tf = ChartBars::new(
            lines_core::Timeframe::Ticks(500),
            lines_core::TickReset::Session,
        );
        assert!(matches!(ema.spec(60_000, tf), Spec::Average { bars, .. } if bars == tf));
        assert!(ema.on_chart_bars());
    }
}
