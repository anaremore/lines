//! Coinbase Exchange public market data (roadmap milestone 3): free, 24/7, JSON over WSS.
//! A development tool to exercise the provider abstraction with live data; crypto trading
//! is not planned.
//!
//! Endpoints (public, unauthenticated as of 2026-11): websocket channels `matches`,
//! `ticker`, `heartbeat`; REST `GET /products/{id}/candles` for history.

pub mod session;

use lines_core::{Capabilities, ContractKey, Delivery, Product};

use crate::catalog;
use crate::provider::{self, Provider, ProviderSession, SessionError};
pub use session::CoinbaseSession;

pub const WS_URL: &str = "wss://ws-feed.exchange.coinbase.com";
pub const REST_URL: &str = "https://api.exchange.coinbase.com";
const PROTOCOL: &str = "coinbase-exchange-ws-1";

/// Coinbase product id (its root symbol; spot products have no contract months).
fn product_id(product: &Product) -> Option<&'static str> {
    catalog::roots(product.id).coinbase
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
            depth: true,
            fractional_qty: true,
            always_open: true,
        }
    }

    fn supports(&self, product: &Product) -> bool {
        product.is_spot() && product_id(product).is_some()
    }

    fn new_session(
        &self,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        let id = product_id(product).ok_or_else(|| {
            SessionError::Unavailable(format!("{} is not mapped for Coinbase", product.symbol))
        })?;
        if let Some(k) = contract
            && (k.product != product.id || k.delivery != Delivery::Spot)
        {
            return Err(SessionError::Unavailable(format!(
                "Coinbase has only spot contracts, not {k:?}"
            )));
        }
        Ok(Box::new(CoinbaseSession::new(
            id.to_owned(),
            product.clone(),
            REST_URL.to_owned(),
        )))
    }
}

pub fn protocol_compatible(protocol: &str) -> bool {
    protocol == PROTOCOL
}
