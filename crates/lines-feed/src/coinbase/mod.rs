//! Coinbase Exchange public market data (roadmap milestone 3): free, 24/7, JSON over WSS.
//! A development tool to exercise the provider abstraction with live data; crypto trading
//! is not planned.
//!
//! Endpoints (public, unauthenticated as of 2026-11): websocket channels `matches`,
//! `ticker`, `heartbeat`; REST `GET /products/{id}/candles` for history.

pub mod session;

use lines_core::{Capabilities, Instrument, InstrumentId};

use crate::catalog;
use crate::provider::{self, Provider, ProviderSession, SessionError};
pub use session::CoinbaseSession;

pub const WS_URL: &str = "wss://ws-feed.exchange.coinbase.com";
pub const REST_URL: &str = "https://api.exchange.coinbase.com";
const PROTOCOL: &str = "coinbase-exchange-ws-1";

/// Internal instrument → Coinbase product id.
fn product_id(id: InstrumentId) -> Option<&'static str> {
    match id {
        catalog::BTC_USD => Some("BTC-USD"),
        catalog::ETH_USD => Some("ETH-USD"),
        _ => None,
    }
}

#[derive(Default)]
pub struct CoinbaseProvider;

impl Provider for CoinbaseProvider {
    fn id(&self) -> &'static str {
        provider::COINBASE
    }

    fn protocol(&self) -> String {
        PROTOCOL.into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            history: true,
            bbo: true,
            depth: false,
            fractional_qty: true,
            always_open: true,
        }
    }

    fn supports(&self, instrument: &Instrument) -> bool {
        product_id(instrument.id).is_some()
    }

    fn new_session(
        &self,
        instrument: &Instrument,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        let product = product_id(instrument.id).ok_or_else(|| {
            SessionError::Unavailable(format!("{} is not mapped for Coinbase", instrument.symbol))
        })?;
        Ok(Box::new(CoinbaseSession::new(
            product.to_owned(),
            instrument.clone(),
            REST_URL.to_owned(),
        )))
    }
}

pub fn protocol_compatible(protocol: &str) -> bool {
    protocol == PROTOCOL
}
