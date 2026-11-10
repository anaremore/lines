//! Databento live market data for Lines (feature `live`, off by default; decision 0020).
//!
//! - [`DatabentoExtension`]: the `databento` source of the `lines` app (decision 0033). CME Globex
//!   (`GLBX.MDP3`) for the catalog's CME Group futures ([`PRODUCTS`]): trades with aggressor,
//!   the inside bid and ask (`mbp-1`; top-10 depth only with `LINES_DATABENTO_DEPTH=mbp-10` on a
//!   CME Plus plan), and an intraday replay of trades as back-fill.
//! - [`session::DatabentoSession`]: the I/O-free gateway protocol (recorded and replayed).
//! - [`transport::LiveTransport`]: the TCP connection; the only place the API key is used.
//!
//! The design and its review against Databento's docs: `LIVE-DESIGN.md`.

pub mod session;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use lines_core::{Capabilities, ContractKey, Product, ProductId};
use lines_feed::extension::{BoxFuture, DynTransport, SourceExtension, Timing};
use lines_feed::provider::{Provider, ProviderSession, SessionError};

pub use session::{BookPlan, DatabentoSession, Floors, ReplayLimit};

use crate::key::ApiKey;

/// Provider id stored in recordings.
pub const PROVIDER: &str = "databento";
/// The gateway's line protocol plus DBN records (the DBN version is in each stream's
/// metadata; this build decodes version 3).
pub const PROTOCOL: &str = "databento-live-raw+dbn";
/// `GLBX.MDP3`'s gateway: the dataset's name in lower case with dashes, port 13000.
pub const GATEWAY: &str = "glbx-mdp3.lsg.databento.com:13000";
/// Products this connector maps: every CME Group future in the catalog (`GLBX.MDP3` roots equal
/// the catalog symbols; raw symbols per `import::raw_symbol`).
pub const PRODUCTS: &[&str] = &[
    // Equity index.
    "ES", "NQ", "YM", "RTY", "MES", "MNQ", "MYM", "M2K", //
    // Rates.
    "ZT", "ZF", "ZN", "ZB", //
    // Energy.
    "CL", "MCL", "NG", //
    // Metals.
    "GC", "MGC", "SI", "HG", //
    // FX.
    "6E", "6J", "6B", //
    // Agriculture.
    "ZC", "ZS", "ZW",
];

/// The `client` field of the auth line.
pub fn client_name() -> String {
    format!("lines/{}", env!("CARGO_PKG_VERSION"))
}

#[derive(Clone, Debug, Default)]
pub struct DatabentoProvider {
    /// The earliest replay start Databento still holds per contract (from aged-out replays;
    /// a lower bound only, so feeds share it).
    pub floors: Floors,
    /// Which book schema feeds ask for, and what the gateway refused this run.
    pub book: Arc<BookPlan>,
    pub limit: ReplayLimit,
}

impl Provider for DatabentoProvider {
    fn id(&self) -> &'static str {
        PROVIDER
    }
    fn protocol(&self) -> String {
        PROTOCOL.into()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Back-fill is the trades replay (bars are built from its trades).
            history: true,
            bbo: true,
            // Whether a feed has depth comes from what it subscribed (the recorded echoes),
            // never this machine's settings: an `mbp-1` feed says so (`DepthUnavailable`).
            depth: true,
            fractional_qty: false,
            always_open: false,
        }
    }
    fn supports(&self, product: &Product) -> bool {
        PRODUCTS.contains(&product.symbol)
    }
    fn new_session(
        &self,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        Ok(Box::new(
            DatabentoSession::new(
                product,
                contract,
                self.floors.clone(),
                self.limit,
                &client_name(),
            )?
            .with_book_plan(self.book.clone()),
        ))
    }
}

/// `LINES_DATABENTO_REPLAY_HOURS`: hours (fractions allowed), 0 = none; unset or not a
/// non-negative number = the default (the whole replay window).
pub fn replay_limit(hours: Option<&str>) -> ReplayLimit {
    hours
        .and_then(|h| h.trim().parse::<f64>().ok())
        .filter(|h| h.is_finite() && *h >= 0.0)
        .map_or_else(ReplayLimit::default, |h| ReplayLimit {
            max_ms: (h * 3_600_000.0) as i64,
        })
}

/// Every mapped product is a catalog future, and every catalog future on a CME Group exchange
/// is mapped.
#[cfg(test)]
#[test]
fn products_are_the_catalogs_cme_futures() {
    use lines_feed::catalog;
    for sym in PRODUCTS {
        let p = catalog::by_symbol(sym).unwrap_or_else(|| panic!("{sym}"));
        assert!(!p.is_spot() && !p.is_equity(), "{sym}");
    }
    for p in catalog::all() {
        let cme = matches!(catalog::exchange(p.id), "CME" | "CBOT" | "NYMEX" | "COMEX");
        assert_eq!(cme, PRODUCTS.contains(&p.symbol), "{}", p.symbol);
    }
}

/// `--help` names every mapped product (as a whole word).
#[cfg(test)]
#[test]
fn long_help_lists_every_product() {
    let ext = DatabentoExtension {
        gateway: GATEWAY.into(),
        provider: Arc::default(),
    };
    let help = ext.long_help().unwrap();
    let words: Vec<&str> = help.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    for sym in PRODUCTS {
        assert!(words.contains(sym), "{sym} missing from --help");
    }
}

#[cfg(test)]
#[test]
fn replay_hours_parse() {
    assert_eq!(replay_limit(None), ReplayLimit::default());
    assert_eq!(replay_limit(Some("1")).max_ms, 3_600_000);
    assert_eq!(replay_limit(Some(" 0.5 ")).max_ms, 1_800_000);
    assert_eq!(replay_limit(Some("0")).max_ms, 0);
    assert_eq!(replay_limit(Some("-2")), ReplayLimit::default());
    assert_eq!(replay_limit(Some("lots")), ReplayLimit::default());
}

/// `LINES_DATABENTO_DEPTH`: `mbp-10` asks for top-10 depth (Databento's CME Plus plan); anything
/// else, or unset, is the inside bid and ask (`mbp-1`).
pub fn wants_depth(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("mbp-10"))
}

#[cfg(test)]
#[test]
fn depth_is_only_on_request() {
    assert!(!wants_depth(None));
    assert!(!wants_depth(Some("")));
    assert!(!wants_depth(Some("mbp-1")));
    assert!(wants_depth(Some(" MBP-10 ")));
}

/// `--source databento`.
#[derive(Clone, Debug)]
pub struct DatabentoExtension {
    pub gateway: String,
    pub provider: Arc<DatabentoProvider>,
}

impl DatabentoExtension {
    /// `LINES_DATABENTO_GATEWAY` overrides the gateway (`host:port`);
    /// `LINES_DATABENTO_REPLAY_HOURS` caps the trades replay (default 24, 0 = none);
    /// `LINES_DATABENTO_DEPTH=mbp-10` asks for top-10 depth.
    pub fn from_env() -> Self {
        let gateway = std::env::var("LINES_DATABENTO_GATEWAY").unwrap_or_else(|_| GATEWAY.into());
        let limit = replay_limit(
            std::env::var("LINES_DATABENTO_REPLAY_HOURS")
                .ok()
                .as_deref(),
        );
        DatabentoExtension {
            gateway,
            provider: Arc::new(DatabentoProvider {
                floors: Floors::default(),
                book: BookPlan::new(wants_depth(
                    std::env::var("LINES_DATABENTO_DEPTH").ok().as_deref(),
                )),
                limit,
            }),
        }
    }
}

impl SourceExtension for DatabentoExtension {
    fn name(&self) -> &'static str {
        "databento"
    }
    fn help(&self) -> &'static str {
        "Databento live CME Globex (GLBX.MDP3): the catalog's CME Group futures (equity index, rates, energy, metals, FX, grains); trades, top of book, replay back-fill."
    }
    fn long_help(&self) -> Option<&'static str> {
        Some(
            "Databento: products ES NQ YM RTY MES MNQ MYM M2K, ZT ZF ZN ZB, CL MCL NG, \
             GC MGC SI HG, 6E 6J 6B, ZC ZS ZW (--instrument CL follows the front month, CLZ26 is \
             one contract). Uses your own Databento API key: DATABENTO_API_KEY, else the OS \
             keychain entry `lines-databento` (the key as its password), else the file named by \
             LINES_DATABENTO_KEY_FILE; with none it is not set up and makes no requests. Live data is billed by Databento. Each contract \
             shown is one connection (at most 8). LINES_DATABENTO_REPLAY_HOURS caps the trades \
             replay on connect (default 24, 0 = none); LINES_DATABENTO_GATEWAY overrides the \
             gateway. The DOM shows the inside bid and ask only: top-10 depth needs Databento's \
             CME Plus plan, then LINES_DATABENTO_DEPTH=mbp-10.",
        )
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn default_product(&self) -> ProductId {
        lines_feed::catalog::ES
    }
    fn check(&self, _product: &Product) -> Result<(), String> {
        // Looked up once (the UI calls this per keystroke): a cached answer after the first.
        // The error names where the key was looked for, never the key.
        crate::key::status().clone()
    }
    fn connect(
        &self,
        _product: &Product,
        _contract: Option<ContractKey>,
    ) -> BoxFuture<'static, anyhow::Result<Box<dyn DynTransport>>> {
        let gateway = self.gateway.clone();
        Box::pin(async move {
            // The keychain read may start a process (up to 5 s): never on a runtime worker.
            let key = tokio::task::spawn_blocking(ApiKey::load).await??;
            let t = transport::LiveTransport::connect(&gateway, key).await?;
            Ok(Box::new(t) as Box<dyn DynTransport>)
        })
    }
    fn timing(&self) -> Timing {
        Timing {
            // The gateway sends heartbeats (every 10 s, requested at logon); we send none.
            keepalive_every: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(35),
            stale_after: Duration::from_secs(15),
        }
    }
}
