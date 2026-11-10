//! The local market-data store (roadmap 5a, decision 0019).
//!
//! Layout under the data directory (`LINES_DATA_DIR`, else the OS's local data directory):
//! ```text
//! store-v1/<source>/<contract>/
//!     <YYYY-MM-DD>.lseg   segments: lines-events-1 batches in CRC blocks, one per trade date
//!     bars                bar index (cache; rebuilt from segments if lost or damaged)
//!     coverage            ranges known complete (bars, trades)
//!     writer.lock         held by the one process writing this contract
//! ```
//! `<source>` keeps data from different suppliers apart (`coinbase`, `cqg`, a mock never
//! mixes with a real feed); `<contract>` is `ES_2026-12` or `BTC-USD`.
//!
//! - [`segment`]: the file format, torn-tail truncation, concurrent readers.
//! - [`writer`]: one writer per contract, owned by that contract's feed (the registry's feed
//!   thread); the file I/O itself runs on a helper thread so a slow disk never stalls decoding.
//! - [`coverage`], [`bars`], [`retention`], [`partition`]: gap tracking, the bar cache, the
//!   size budget, and the trade-date partition key.
//! - [`replay`]: a stored contract (or one day of it) replays like a recording, through the
//!   `lines` provider.
//! - [`day`]: one whole past trade date written at once ("Download days", decision 0030 §8):
//!   a part file renamed into place on completion, under the contract's writer lock.
//! - [`orderflow`]: at feed startup, the stored trades of the covered ranges replay into the
//!   order-flow consumers (profile, footprint, VAP, tape, the indicator engine).
//!
//! Stored data may be licensed (decision 0010): it stays in the local data directory, is never
//! synced or uploaded, and must never be committed.

pub mod bars;
pub mod coverage;
mod crc;
pub mod day;
pub mod orderflow;
pub mod partition;
pub mod replay;
pub mod retention;
pub mod segment;
pub mod writer;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use lines_core::{Bar, CivilDate, ContractKey, Delivery, Product, Stitcher, UtcMillis};

pub use coverage::{Coverage, Ranges};
pub use segment::FlushPolicy;
pub use writer::ContractWriter;

pub const SEGMENT_EXT: &str = "lseg";
/// Default size budget for stored segments.
pub const DEFAULT_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// The store root (contains `store-v1/`).
    pub root: PathBuf,
    /// Who supplies the data; one directory per source.
    pub source: String,
    /// Catalog symbols whose order-book depth is stored too (off by default: it is large).
    pub depth_products: Vec<String>,
    /// Segments are evicted oldest trade date first beyond this many bytes.
    pub budget_bytes: u64,
    pub flush: FlushPolicy,
}

impl StoreConfig {
    pub fn new(root: PathBuf, source: &str) -> StoreConfig {
        StoreConfig {
            root,
            source: source.into(),
            depth_products: Vec::new(),
            budget_bytes: DEFAULT_BUDGET_BYTES,
            flush: FlushPolicy::default(),
        }
    }

    pub fn stores_depth(&self, product: &Product) -> bool {
        self.depth_products
            .iter()
            .any(|s| s.eq_ignore_ascii_case(product.symbol))
    }
}

/// `LINES_DATA_DIR`, else the OS's local (never roaming) data directory: `%LOCALAPPDATA%\Lines`
/// on Windows, `~/Library/Application Support/Lines` on macOS, `$XDG_DATA_HOME/lines` (or
/// `~/.local/share/lines`) elsewhere.
pub fn default_root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("LINES_DATA_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|d| d.join("Lines"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/Lines"))
    } else {
        env("XDG_DATA_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".local/share")))
            .map(|d| d.join("lines"))
    }
}

/// Replace `path`'s contents atomically (write a temporary file, fsync, rename).
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, path)
}

fn contract_key_dir(product: &Product, contract: Option<ContractKey>) -> String {
    let sym: String = product
        .symbol
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    match contract.map(|k| k.delivery) {
        Some(Delivery::Monthly { year, month }) => {
            format!("{sym}_{year:04}-{:02}", month.number())
        }
        Some(Delivery::Spot | Delivery::Equity) => sym,
        // `SPY_2026-12-18_C45050`: expiry, right and the exact strike in price units.
        Some(Delivery::Option(o)) => format!(
            "{sym}_{:04}-{:02}-{:02}_{}{}",
            o.expiry.year,
            o.expiry.month,
            o.expiry.day,
            o.right.letter(),
            o.strike.0
        ),
        None => format!("{sym}_unknown"),
    }
}

/// One contract's directory in the store.
#[derive(Clone, Debug)]
pub struct ContractDir {
    pub path: PathBuf,
    pub product: Product,
    pub contract: Option<ContractKey>,
    pub source: String,
}

impl ContractDir {
    pub fn new(
        root: &Path,
        source: &str,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> ContractDir {
        ContractDir {
            path: root
                .join("store-v1")
                .join(source)
                .join(contract_key_dir(product, contract)),
            product: product.clone(),
            contract,
            source: source.into(),
        }
    }

    pub fn segment_path(&self, date: CivilDate) -> PathBuf {
        self.path.join(format!("{date}.{SEGMENT_EXT}"))
    }

    pub fn bars_path(&self) -> PathBuf {
        self.path.join("bars")
    }

    pub fn coverage_path(&self) -> PathBuf {
        self.path.join("coverage")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.path.join("writer.lock")
    }

    /// Trade dates with a segment, oldest first.
    pub fn dates(&self) -> Vec<CivilDate> {
        let mut v: Vec<CivilDate> = std::fs::read_dir(&self.path)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != SEGMENT_EXT {
                    return None;
                }
                parse_date(p.file_stem()?.to_str()?)
            })
            .collect();
        v.sort();
        v
    }

    pub fn coverage(&self) -> Coverage {
        Coverage::load(&self.coverage_path())
    }

    /// Ranges whose trades are all in segment files that still exist (claims for evicted
    /// trade dates no longer count).
    pub fn trade_coverage(&self) -> Ranges {
        let claimed = self.coverage().trades;
        let mut present = Ranges::default();
        for d in self.dates() {
            let (open, close) = partition::trade_date_span(&self.product, d);
            present.add(open.0, close.0 - 1);
        }
        let mut out = Ranges::default();
        for &(a, b) in claimed.ranges() {
            for &(pa, pb) in present.ranges() {
                out.add(a.max(pa), b.min(pb));
            }
        }
        out
    }

    /// Rebuild bars from the stored trades (the bar index's source of truth).
    pub fn rebuild_bars(&self, period_ms: i64, max_bars: usize) -> io::Result<Vec<Bar>> {
        let mut series = lines_core::BarSeries::new(period_ms, max_bars);
        let mut stitch = Stitcher::new(period_ms, 0);
        let mut events = Vec::new();
        for d in self.dates() {
            let mut r = segment::SegmentReader::open(&self.segment_path(d))?;
            while let Ok(b) = r.next_block()? {
                events.clear();
                if crate::native::decode(&b.events, &mut events).is_err() {
                    continue;
                }
                for e in &events {
                    if let crate::native::NativeEvent::Trade {
                        time_us,
                        price,
                        qty,
                        ..
                    } = *e
                    {
                        stitch.trade(&mut series, UtcMillis(time_us.div_euclid(1000)), price, qty);
                    }
                }
            }
        }
        Ok(series.bars().to_vec())
    }
}

pub(crate) fn parse_date(s: &str) -> Option<CivilDate> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    CivilDate::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

/// Whether `path` is a stored contract directory or segment (replayed via [`replay`]).
pub fn is_stored(path: &Path) -> bool {
    if path.is_dir() {
        return path
            .read_dir()
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().extension().is_some_and(|x| x == SEGMENT_EXT));
    }
    let mut magic = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
        .is_ok()
        && &magic == segment::MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn directories_name_the_contract_and_keep_sources_apart() {
        let es = catalog::get(catalog::ES).unwrap();
        let k = ContractKey::monthly(es.id, 2026, lines_core::Month::new(12).unwrap());
        let d = ContractDir::new(Path::new("/r"), "cqg", &es, Some(k));
        assert!(d.path.ends_with("store-v1/cqg/ES_2026-12"));
        let btc = catalog::get(catalog::BTC_USD).unwrap();
        let d = ContractDir::new(
            Path::new("/r"),
            "coinbase",
            &btc,
            Some(ContractKey::spot(btc.id)),
        );
        assert!(d.path.ends_with("store-v1/coinbase/BTC-USD"));
        assert_eq!(
            d.segment_path(CivilDate::new(2026, 11, 6).unwrap())
                .file_name()
                .unwrap(),
            "2026-11-06.lseg"
        );
        assert_eq!(parse_date("2026-11-06"), CivilDate::new(2026, 11, 6));
        assert_eq!(parse_date("2026-11-06-x"), None);
    }
}
