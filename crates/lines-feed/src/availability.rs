//! What a source can serve, decided up front so the UI can grey out what it can't (symbol
//! picker, roadmap 4b) instead of failing after the user picks it.
//!
//! [`availability`] does any I/O once (a replay reads its file header); [`Availability::check`]
//! is then pure and cheap enough to call per keystroke.
//!
//! `check` is at least as strict as `plan` (the feed's own validation in `spawn`): whatever it
//! allows, `plan` accepts (tested). It is stricter only where the failure would otherwise
//! surface after connecting: the CQG mock resolves only ES, and the feed itself still lets a
//! server refuse a symbol (that path is tested in `tests/feed.rs`).

use std::sync::Arc;

use anyhow::{Result, anyhow};
use lines_core::{ContractKey, Product};

use crate::catalog::{self, Target};
use crate::provider::Provider;
use crate::{Source, cqg, provider_for, transport};

/// A recording's fixed product and contract.
#[derive(Clone, Debug)]
struct Recorded {
    product: Product,
    /// `None` for pre-v3 recordings, which don't name their contract.
    contract: Option<ContractKey>,
}

/// What one source can serve. Cheap to query; build it once per source.
#[derive(Clone)]
pub struct Availability {
    provider: Arc<dyn Provider>,
    /// The CQG mock resolves only its one root symbol.
    mock: bool,
    recorded: Option<Recorded>,
}

impl std::fmt::Debug for Availability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Availability")
            .field("provider", &self.provider.id())
            .field("mock", &self.mock)
            .field("recorded", &self.recorded)
            .finish()
    }
}

/// Inspect a source (reads a replay's header; no network).
pub fn availability(source: &Source) -> Result<Availability> {
    let recorded = match source {
        Source::Replay { path, .. } => {
            let h = transport::read_header(path)?;
            let product = catalog::by_symbol(&h.product)
                .ok_or_else(|| anyhow!("recording is for unknown product {:?}", h.product))?;
            Some(Recorded {
                product,
                contract: h.contract,
            })
        }
        _ => None,
    };
    Ok(Availability {
        provider: provider_for(source)?,
        mock: matches!(source, Source::CqgMock(_)),
        recorded,
    })
}

impl Availability {
    /// The provider id shown in the UI (e.g. `cqg`, `coinbase`).
    pub fn provider_id(&self) -> &'static str {
        self.provider.id()
    }

    /// `Ok` if this source can serve `target`, else a short reason for the UI
    /// (e.g. "coinbase: spot crypto only").
    pub fn check(&self, target: &Target) -> Result<(), String> {
        let (id, contract) = match target {
            Target::Product(id) => (*id, None),
            Target::Contract(k) => (k.product, Some(*k)),
        };
        let product = catalog::get(id).ok_or_else(|| format!("unknown product {id}"))?;
        if let Some(r) = &self.recorded {
            if r.product.id != id {
                return Err(format!("recording is {} only", r.product.symbol));
            }
            return match (contract, r.contract) {
                (Some(want), Some(have)) if want != have => Err(format!(
                    "recording holds {}",
                    have.label(&r.product).as_str()
                )),
                (Some(_), None) => Err("recording's contract is unknown".into()),
                _ => Ok(()),
            };
        }
        if !self.provider.supports(&product) {
            // A crypto market belongs to one exchange: name the source that has it.
            let home = match catalog::exchange(id) {
                "Coinbase" => Some(("Coinbase", "coinbase")),
                "Kraken" | "Kraken Futures" => Some(("Kraken", "kraken")),
                _ => None,
            };
            return Err(match (self.provider.id(), home) {
                (p, Some((name, source))) if p != source => {
                    format!("{name} market: use --source {source}")
                }
                (crate::provider::COINBASE, _) => "coinbase: spot crypto only".into(),
                (crate::provider::KRAKEN, _) => "kraken: crypto only".into(),
                (p, _) if product.is_spot() => format!("{p}: futures only"),
                (p, _) => format!("not mapped for {p}"),
            });
        }
        if self.mock && catalog::roots(id).cqg != Some(cqg::mock::MOCK_SYMBOL) {
            return Err("mock serves ES only".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cqg::mock::MockConfig;
    use crate::{FeedConfig, plan};
    use lines_core::{CivilDate, Month};

    fn targets() -> Vec<Target> {
        let today = CivilDate::new(2026, 11, 6).unwrap();
        let mut out = Vec::new();
        for p in catalog::all() {
            out.push(Target::Product(p.id));
            out.extend(p.contracts_from(today).take(2).map(Target::Contract));
        }
        out
    }

    fn sources() -> Vec<Source> {
        vec![
            Source::CqgMock(MockConfig::default()),
            Source::Coinbase {
                url: "wss://example.invalid".into(),
            },
            Source::Kraken {
                spot_url: "wss://example.invalid".into(),
                futures_url: "wss://example.invalid".into(),
            },
        ]
    }

    #[test]
    fn never_offers_what_the_feed_would_refuse() {
        let mut stricter = 0;
        for source in sources() {
            let a = availability(&source).unwrap();
            for t in targets() {
                let mut cfg = FeedConfig::new(source.clone());
                cfg.target = Some(t.clone());
                let (shown, starts) = (a.check(&t).is_ok(), plan(&cfg).is_ok());
                assert!(
                    !shown || starts,
                    "{t:?} on {} offered but refused",
                    a.provider_id()
                );
                if starts && !shown {
                    // Only the mock's server-side limit: CQG maps it, the mock can't resolve it.
                    assert_eq!(a.check(&t), Err("mock serves ES only".into()), "{t:?}");
                    stricter += 1;
                }
            }
        }
        assert!(stricter > 0, "the mock case is exercised");
    }

    #[test]
    fn reasons_are_specific() {
        let mock = availability(&Source::CqgMock(MockConfig::default())).unwrap();
        assert_eq!(mock.check(&Target::Product(catalog::ES)), Ok(()));
        assert_eq!(
            mock.check(&Target::Product(catalog::NQ)),
            Err("mock serves ES only".into())
        );
        assert_eq!(
            mock.check(&Target::Product(catalog::BTC_USD)),
            Err("Coinbase market: use --source coinbase".into())
        );
        let cb = availability(&Source::Coinbase { url: String::new() }).unwrap();
        assert_eq!(cb.check(&Target::Product(catalog::BTC_USD)), Ok(()));
        assert_eq!(
            cb.check(&Target::Product(catalog::ES)),
            Err("coinbase: spot crypto only".into())
        );
        assert_eq!(
            cb.check(&Target::Product(catalog::BTC_PERP_KR)),
            Err("Kraken market: use --source kraken".into())
        );
        let kr = availability(&Source::Kraken {
            spot_url: String::new(),
            futures_url: String::new(),
        })
        .unwrap();
        for id in [
            catalog::BTC_USD_KR,
            catalog::ETH_USD_KR,
            catalog::BTC_PERP_KR,
            catalog::ETH_PERP_KR,
        ] {
            assert_eq!(kr.check(&Target::Product(id)), Ok(()));
        }
        assert_eq!(
            kr.check(&Target::Product(catalog::BTC_USD)),
            Err("Coinbase market: use --source coinbase".into())
        );
        assert_eq!(
            kr.check(&Target::Product(catalog::ES)),
            Err("kraken: crypto only".into())
        );
    }

    #[test]
    fn recordings_serve_only_their_product_and_contract() {
        let es = catalog::get(catalog::ES).unwrap();
        let z26 = ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap());
        let h27 = ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap());
        let a = Availability {
            provider: Arc::new(crate::native::NativeProvider),
            mock: false,
            recorded: Some(Recorded {
                product: es.clone(),
                contract: Some(z26),
            }),
        };
        assert_eq!(a.check(&Target::Product(catalog::ES)), Ok(()));
        assert_eq!(a.check(&Target::Contract(z26)), Ok(()));
        assert_eq!(
            a.check(&Target::Contract(h27)),
            Err("recording holds ESZ26".into())
        );
        assert_eq!(
            a.check(&Target::Product(catalog::NQ)),
            Err("recording is ES only".into())
        );
        let v2 = Availability {
            recorded: Some(Recorded {
                product: es,
                contract: None,
            }),
            ..a
        };
        assert_eq!(v2.check(&Target::Product(catalog::ES)), Ok(()));
        assert_eq!(
            v2.check(&Target::Contract(z26)),
            Err("recording's contract is unknown".into())
        );
    }
}
