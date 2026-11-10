//! Databento (GLBX.MDP3) for Lines (decision 0033: a public source on the user's own key).
//!
//! - [`import`]: DBN files (`definition`, `trades`, `mbp-10`) → a Lines replay recording
//!   (`.lfr` v3, `lines-events-1`), exact and deterministic.
//! - [`api`]: the free cost check and the guarded download (historical HTTP API).
//! - [`key`]: the user's API key (`DATABENTO_API_KEY`, else the OS keychain; never logged).
//! - [`live`] (feature `live`): live market data, the `databento` source of the `lines` app
//!   (`LIVE-DESIGN.md`).
//!
//! Uses the `dbn` crate (Apache-2.0) only, not the `databento` client: see the README.

pub mod api;
pub mod book;
pub mod defs;
pub mod import;
pub mod key;
#[cfg(feature = "live")]
pub mod live;
pub mod price;
