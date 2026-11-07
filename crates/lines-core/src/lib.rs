//! Pure, vendor-neutral charting and pricing logic for Lines.
//!
//! No GPUI, Tokio, protobuf, or vendor units here (decisions 0002, 0003): plain data + math,
//! unit-tested.

pub mod bars;
pub mod book;
pub mod calendar;
pub mod chain;
pub mod columns;
pub mod continuous;
pub mod drawings;
pub mod econ;
pub mod fixed_str;
pub mod footprint;
pub mod geometry;
pub mod instrument;
pub mod ladder;
pub mod latency;
pub mod layout_lint;
pub mod levels;
pub mod price;
pub mod profile;
pub mod qty;
pub mod roll;
pub mod sessions;
pub mod stitch;
pub mod style;
pub mod tape;
pub mod ticks;
pub mod time;
pub mod timeframe;
pub mod trail;
pub mod vap;
pub mod viewport;

pub use bars::{Bar, BarSeries, Changed, TradeOutcome};
pub use book::{Level, OrderBook, Side};
pub use calendar::CivilDate;
pub use chain::{ChainRow, OptionChain, OptionChains, OptionQuote};
pub use fixed_str::FixedStr;
pub use geometry::{Candle, PriceGridLine, Scene, TimeGridLine};
pub use instrument::{
    Capabilities, ContractKey, Currency, Delivery, ExerciseStyle, ExpiryRule, MicroEquivalent,
    Month, MonthCycle, OptionKey, OptionRules, OptionTick, Product, ProductId, ProductKind, Ratio,
    Right, RollRule,
};
pub use price::{DisplayStyle, Price, PriceFormat, PriceScale, UnitConverter, UnitError};
pub use profile::{ProfileRows, SessionProfiles, SideVolume};
pub use qty::{Qty, QtyFormat};
pub use sessions::{
    HolidayData, HolidayTable, LocalTime, Session, SessionCalendar, SessionKind, SessionSpec,
};
pub use stitch::{StitchStats, Stitcher};
pub use tape::{Aggressor, Print, Tape};
pub use ticks::{BarKind, BarScale, TickBars, TickReset};
pub use time::UtcMillis;
pub use timeframe::{BarsView, Derived, Timeframe};
pub use vap::VolumeAtPrice;
pub use viewport::Viewport;
