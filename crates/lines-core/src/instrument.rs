//! Provider-neutral instrument identity and feed capabilities (decision 0003).
//!
//! Vendor symbols (CQG `EP`, Coinbase `BTC-USD`, …) never appear here; providers map
//! [`InstrumentId`]s to their own symbols inside `lines-feed`.

use std::fmt;

use crate::price::PriceFormat;
use crate::qty::QtyFormat;

/// Stable internal identifier. Never reuse a retired id.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstrumentId(pub u32);

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// ISO 4217-style currency code.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Currency(pub [u8; 3]);

impl Currency {
    pub const USD: Currency = Currency(*b"USD");
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instrument {
    pub id: InstrumentId,
    /// Internal canonical symbol for display and lookup (e.g. `ES`, `BTC/USD`).
    pub symbol: &'static str,
    pub description: &'static str,
    pub price: PriceFormat,
    pub qty: QtyFormat,
    /// Currency prices are quoted in.
    pub currency: Currency,
}

/// What a market-data provider can deliver. The UI enables features by capability,
/// never by asset class or provider name.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Server-side historical bars (back-fill on connect/reconnect).
    pub history: bool,
    /// Best bid/offer updates.
    pub bbo: bool,
    /// Order-book depth.
    pub depth: bool,
    /// Quantities may be fractional.
    pub fractional_qty: bool,
    /// Available around the clock (useful for development).
    pub always_open: bool,
}
