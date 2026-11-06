//! CQG WebAPI provider (protobuf over WSS). The live connection is untested until
//! credentials exist; the in-process [`mock`] speaks the same protocol.

pub mod mock;
pub mod session;

use lines_core::{Capabilities, ContractKey, Delivery, Product};

use crate::catalog;
use crate::provider::{self, Provider, ProviderSession, SessionError};
pub use session::{CqgSession, LogonParams};

/// Default CQG demo endpoint (requires CQG-issued demo credentials).
#[cfg(feature = "vendor-cqg")]
pub const DEMO_URL: &str = "wss://demoapi.cqg.com:443";

/// CQG contract symbol by rule (decision 0013): root + month code + last digit of the year,
/// e.g. ES Dec 2026 → `EPZ6`. CQG resolves the single year digit to the nearest listed
/// contract, which is unambiguous because listings span far less than ten years.
pub(crate) fn contract_symbol(root: &str, key: ContractKey) -> Option<String> {
    match key.delivery {
        Delivery::Monthly { year, month } => Some(format!("{root}{}{}", month.code(), year % 10)),
        Delivery::Spot => None,
    }
}

pub struct CqgProvider {
    logon: LogonParams,
}

impl CqgProvider {
    pub fn new(logon: LogonParams) -> Self {
        CqgProvider { logon }
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
            depth: true,
            fractional_qty: false,
            always_open: false,
        }
    }

    fn supports(&self, product: &Product) -> bool {
        catalog::roots(product.id).cqg.is_some() && !product.is_spot()
    }

    fn new_session(
        &self,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        let root = catalog::roots(product.id).cqg.ok_or_else(|| {
            SessionError::Unavailable(format!("{} is not mapped for CQG", product.symbol))
        })?;
        let expect = match contract {
            Some(k) => {
                let sym = contract_symbol(root, k).ok_or_else(|| {
                    SessionError::Unavailable(format!("{} has no dated contracts", product.symbol))
                })?;
                Some((k, sym))
            }
            None => None,
        };
        Ok(Box::new(CqgSession::new(
            root,
            expect,
            product.clone(),
            self.logon.clone(),
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
            !p.supports(&catalog::get(catalog::MNQ).unwrap()),
            "unverified CQG root"
        );
        let btc = catalog::get(catalog::BTC_USD).unwrap();
        assert!(
            p.new_session(&btc, Some(ContractKey::spot(btc.id)))
                .is_err()
        );
        assert!(protocol_compatible(&p.protocol()));
        assert!(!protocol_compatible("cqg-webapi-3.1"));
    }

    #[test]
    fn contract_symbol_rule() {
        let z = lines_core::Month::new(12).unwrap();
        let h = lines_core::Month::new(3).unwrap();
        assert_eq!(
            contract_symbol("EP", ContractKey::monthly(catalog::ES, 2026, z)).as_deref(),
            Some("EPZ6")
        );
        assert_eq!(
            contract_symbol("ENQ", ContractKey::monthly(catalog::NQ, 2030, h)).as_deref(),
            Some("ENQH0")
        );
        assert_eq!(contract_symbol("EP", ContractKey::spot(catalog::ES)), None);
    }
}
