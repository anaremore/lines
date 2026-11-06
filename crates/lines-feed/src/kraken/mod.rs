//! Kraken public market data (roadmap 5d): spot over WebSocket v2 and USD-margined perpetuals
//! over the Kraken Futures public feed. Free, 24/7, no account or API key; nothing private is
//! touched. A second, very different crypto connector that exercises the provider abstraction,
//! and whose level-2 spot book carries a CRC-32 of the top of book, so the book we rebuild is
//! checked against the exchange on every update.
//!
//! Endpoints (public, unauthenticated, checked 2026-11-06):
//! - Spot WebSocket `wss://ws.kraken.com/v2`: `book` (depth 100, checksum on every message),
//!   `trade` (with a 50-trade snapshot), `ticker` (`event_trigger: bbo`).
//! - Spot REST `https://api.kraken.com/0/public/`: `AssetPairs` (tick and lot size, checked
//!   against the catalog on every connect), `OHLC` (1-minute candles: **only the newest 720**,
//!   i.e. 12 hours, whatever `since` asks), `Trades` (complete history, paged forward by a
//!   nanosecond cursor, 1000 per page; `trade_id` is sequential per pair).
//! - Futures WebSocket `wss://futures.kraken.com/ws/v1`: `book` (full-depth snapshot, then
//!   per-level deltas with a sequence number, **no checksum**), `trade` (100-trade snapshot),
//!   `ticker`. Public feeds need no authentication challenge.
//! - Futures REST: `derivatives/api/v3/instruments` (tick and size precision), charts
//!   `api/charts/v1/trade/{symbol}/1m` (candles, paged), `derivatives/api/v3/history` (the
//!   newest 100 trades per page, paged backwards by time).
//!
//! Vendor symbols (`BTC/USD`, `XBTUSD`, `PF_XBTUSD`) exist only in this module (decision 0003).
//! One provider id (`kraken`) and one protocol id serve both venues; the session type is chosen
//! by product, so a replay dispatches exactly like the live feed.

pub mod book;
pub mod futures;
pub mod num;
pub mod spot;

use lines_core::{
    Bar, Capabilities, ContractKey, Delivery, Price, Product, ProductId, Qty, UtcMillis,
};

use crate::catalog;
use crate::provider::{self, Provider, ProviderSession, SessionError};
pub use futures::FuturesSession;
pub use spot::SpotSession;

pub const SPOT_WS_URL: &str = "wss://ws.kraken.com/v2";
pub const SPOT_REST_URL: &str = "https://api.kraken.com/0/public";
pub const FUTURES_WS_URL: &str = "wss://futures.kraken.com/ws/v1";
pub const FUTURES_REST_URL: &str = "https://futures.kraken.com";
const PROTOCOL: &str = "kraken-ws2-futures1-1";

/// Where a product trades on Kraken, with its vendor symbols.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Market {
    /// Spot: WebSocket v2 symbol and REST pair name.
    Spot {
        ws: &'static str,
        rest: &'static str,
    },
    /// Kraken Futures product id.
    Futures { product: &'static str },
}

pub(crate) fn market(id: ProductId) -> Option<Market> {
    Some(match id {
        catalog::BTC_USD_KR => Market::Spot {
            ws: "BTC/USD",
            rest: "XBTUSD",
        },
        catalog::ETH_USD_KR => Market::Spot {
            ws: "ETH/USD",
            rest: "ETHUSD",
        },
        catalog::BTC_PERP_KR => Market::Futures {
            product: "PF_XBTUSD",
        },
        catalog::ETH_PERP_KR => Market::Futures {
            product: "PF_ETHUSD",
        },
        _ => return None,
    })
}

/// Whether a product is served by the Kraken Futures feed (else the spot feed).
pub fn is_futures(product: &Product) -> bool {
    matches!(market(product.id), Some(Market::Futures { .. }))
}

/// Decimal places of a product's price grid. Kraken products are all on decimal grids.
pub(crate) fn price_decimals(p: &Product) -> Option<u32> {
    let (num, den) = (p.price.scale.num(), p.price.scale.den());
    if num != 1 {
        return None;
    }
    let d = den.checked_ilog10()?;
    (10i64.checked_pow(d) == Some(den)).then_some(d)
}

/// Bars from trades (any order), bucketed by `period_ms`; trade time decides open/close.
pub(crate) fn bars_from(trades: &mut [(UtcMillis, Price, Qty)], period_ms: i64) -> Vec<Bar> {
    trades.sort_by_key(|t| t.0);
    let mut bars: Vec<Bar> = Vec::new();
    for &(time, price, qty) in trades.iter() {
        let bucket = time.floor_to(period_ms);
        match bars.last_mut() {
            Some(b) if b.time == bucket => b.apply_trade(price, qty),
            _ => bars.push(Bar::from_trade(bucket, price, qty)),
        }
    }
    bars
}

#[derive(Clone, Debug)]
pub struct KrakenProvider {
    pub spot_rest: String,
    pub futures_rest: String,
}

impl Default for KrakenProvider {
    fn default() -> Self {
        KrakenProvider {
            spot_rest: SPOT_REST_URL.into(),
            futures_rest: FUTURES_REST_URL.into(),
        }
    }
}

impl Provider for KrakenProvider {
    fn id(&self) -> &'static str {
        provider::KRAKEN
    }

    fn protocol(&self) -> String {
        PROTOCOL.into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            history: true,
            bbo: true,
            depth: true,
            fractional_qty: true,
            always_open: true,
        }
    }

    fn supports(&self, product: &Product) -> bool {
        product.is_spot() && market(product.id).is_some() && price_decimals(product).is_some()
    }

    fn new_session(
        &self,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        let m = market(product.id).ok_or_else(|| {
            SessionError::Unavailable(format!("{} is not a Kraken product", product.symbol))
        })?;
        if let Some(k) = contract
            && (k.product != product.id || k.delivery != Delivery::Spot)
        {
            return Err(SessionError::Unavailable(format!(
                "Kraken spot and perpetuals have a single contract, not {k:?}"
            )));
        }
        let decimals = price_decimals(product).ok_or_else(|| {
            SessionError::BadMetadata(format!("{} is not on a decimal price grid", product.symbol))
        })?;
        Ok(match m {
            Market::Spot { ws, rest } => Box::new(SpotSession::new(
                ws,
                rest,
                product.clone(),
                decimals,
                self.spot_rest.clone(),
            )),
            Market::Futures { product: symbol } => Box::new(FuturesSession::new(
                symbol,
                product.clone(),
                decimals,
                self.futures_rest.clone(),
            )),
        })
    }
}

pub fn protocol_compatible(protocol: &str) -> bool {
    protocol == PROTOCOL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn products_map_to_one_venue_each() {
        for p in catalog::all() {
            let kraken = KrakenProvider::default();
            match market(p.id) {
                Some(_) => {
                    assert!(kraken.supports(&p), "{}", p.symbol);
                    assert!(
                        catalog::exchange(p.id).starts_with("Kraken"),
                        "{}",
                        p.symbol
                    );
                }
                None => assert!(!kraken.supports(&p), "{}", p.symbol),
            }
        }
        assert!(is_futures(&catalog::get(catalog::BTC_PERP_KR).unwrap()));
        assert!(!is_futures(&catalog::get(catalog::BTC_USD_KR).unwrap()));
    }

    #[test]
    fn bars_follow_trade_time() {
        let t = |s: i64, p: i64| (UtcMillis(s * 1000), Price(p), Qty(1));
        let mut trades = vec![t(61, 12), t(5, 10), t(59, 11), t(60, 13)];
        let bars = bars_from(&mut trades, 60_000);
        assert_eq!(bars.len(), 2);
        assert_eq!(
            (bars[0].open, bars[0].close, bars[0].volume),
            (Price(10), Price(11), Qty(2))
        );
        assert_eq!((bars[1].open, bars[1].close), (Price(13), Price(12)));
    }
}
