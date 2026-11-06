//! Pure charting and pricing logic for Lines.
//!
//! No GPUI, Tokio, or protobuf here: everything is plain data + math and is unit-tested.

pub mod bars;
pub mod fixed_str;
pub mod geometry;
pub mod price;
pub mod time;
pub mod viewport;

pub use bars::{Bar, BarSeries, TradeOutcome};
pub use fixed_str::FixedStr;
pub use geometry::{Candle, PriceGridLine, Scene, TimeGridLine};
pub use price::{DisplayStyle, Price, PriceFormat, PriceScale, ScaleError};
pub use time::UtcMillis;
pub use viewport::Viewport;
