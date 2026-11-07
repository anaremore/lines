//! Pure, vendor-neutral charting and pricing logic for Lines.
//!
//! No GPUI, Tokio, protobuf, or vendor units here (decisions 0002, 0003): plain data + math,
//! unit-tested.

pub mod bars;
pub mod book;
pub mod calendar;
pub mod columns;
pub mod continuous;
pub mod fixed_str;
pub mod footprint;
pub mod geometry;
pub mod instrument;
pub mod ladder;
pub mod levels;
pub mod price;
pub mod profile;
pub mod qty;
pub mod roll;
pub mod sessions;
pub mod stitch;
pub mod tape;
pub mod time;
pub mod timeframe;
pub mod trail;
pub mod vap;
pub mod viewport;

pub use bars::{Bar, BarSeries, Changed, TradeOutcome};
pub use book::{Level, OrderBook, Side};
pub use calendar::CivilDate;
pub use fixed_str::FixedStr;
pub use geometry::{Candle, PriceGridLine, Scene, TimeGridLine};
pub use instrument::{
    Capabilities, ContractKey, Currency, Delivery, ExpiryRule, MicroEquivalent, Month, MonthCycle,
    Product, ProductId, ProductKind, Ratio, RollRule,
};
pub use price::{DisplayStyle, Price, PriceFormat, PriceScale, UnitConverter, UnitError};
pub use profile::{ProfileRows, SessionProfiles, SideVolume};
pub use qty::{Qty, QtyFormat};
pub use sessions::{
    HolidayData, HolidayTable, LocalTime, Session, SessionCalendar, SessionKind, SessionSpec,
};
pub use stitch::{StitchStats, Stitcher};
pub use tape::{Aggressor, Print, Tape};
pub use time::UtcMillis;
pub use timeframe::{BarsView, Derived, Timeframe};
pub use vap::VolumeAtPrice;
pub use viewport::Viewport;
