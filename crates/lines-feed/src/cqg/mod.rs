//! CQG WebAPI provider (protobuf over WSS). The live connection is untested until
//! credentials exist; the in-process [`mock`] speaks the same protocol.

pub mod mock;
pub mod session;

use std::sync::Arc;

use lines_core::{Capabilities, Instrument, InstrumentId};
use parking_lot::Mutex;

use crate::catalog;
use crate::provider::{self, Provider, ProviderSession, SessionError};
pub use session::{CqgSession, LogonParams};

/// Default CQG demo endpoint (requires CQG-issued demo credentials).
pub const DEMO_URL: &str = "wss://demoapi.cqg.com:443";

/// Internal instrument → CQG symbol. CQG's own naming; verify with CQG symbol
/// resolution before relying on anything beyond ES.
fn cqg_symbol(id: InstrumentId) -> Option<&'static str> {
    match id {
        catalog::ES => Some("EP"),
        catalog::NQ => Some("ENQ"),
        catalog::ZN => Some("TYA"),
        _ => None,
    }
}

pub struct CqgProvider {
    logon: LogonParams,
    /// Contract the symbol resolved to on the previous connection (roll detection).
    last_contract: Arc<Mutex<Option<String>>>,
}

impl CqgProvider {
    pub fn new(logon: LogonParams) -> Self {
        CqgProvider {
            logon,
            last_contract: Arc::new(Mutex::new(None)),
        }
    }
}

impl Provider for CqgProvider {
    fn id(&self) -> &'static str {
        provider::CQG
    }

    fn protocol(&self) -> String {
        format!(
            "cqg-webapi-{}.{}",
            cqg_proto::PROTOCOL_VERSION_MAJOR,
            cqg_proto::PROTOCOL_VERSION_MINOR
        )
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            history: true,
            bbo: true,
            depth: false,
            fractional_qty: false,
            always_open: false,
        }
    }

    fn supports(&self, instrument: &Instrument) -> bool {
        cqg_symbol(instrument.id).is_some()
    }

    fn new_session(
        &self,
        instrument: &Instrument,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        let symbol = cqg_symbol(instrument.id).ok_or_else(|| {
            SessionError::Unavailable(format!("{} is not mapped for CQG", instrument.symbol))
        })?;
        Ok(Box::new(CqgSession::new(
            symbol.to_owned(),
            instrument.clone(),
            self.logon.clone(),
            self.last_contract.clone(),
        )))
    }
}

/// Whether a recording header was written by a protocol this build can decode.
pub fn protocol_compatible(protocol: &str) -> bool {
    protocol.starts_with(&format!(
        "cqg-webapi-{}.",
        cqg_proto::PROTOCOL_VERSION_MAJOR
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_futures_only() {
        let p = CqgProvider::new(LogonParams::default());
        assert!(p.supports(&catalog::get(catalog::ES).unwrap()));
        assert!(!p.supports(&catalog::get(catalog::BTC_USD).unwrap()));
        assert!(
            p.new_session(&catalog::get(catalog::BTC_USD).unwrap())
                .is_err()
        );
        assert!(protocol_compatible(&p.protocol()));
        assert!(!protocol_compatible("cqg-webapi-3.1"));
    }
}
