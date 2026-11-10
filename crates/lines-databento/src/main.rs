//! `lines-databento`: cost check, download and import of Databento GLBX.MDP3 history.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use lines_databento::api::{self, DATASET, Query, human_bytes};
use lines_databento::import::{ImportOptions, import, raw_symbol};
use lines_databento::key::ApiKey;
use lines_feed::catalog::{self, Target};

#[derive(Parser)]
#[command(
    version,
    about = "Databento GLBX.MDP3 history for Lines: cost check, download, import"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Range {
    /// Databento raw symbols (ESZ6,NQZ6), or parents (ES.FUT) with --stype-in parent.
    #[arg(long, value_delimiter = ',', required = true)]
    symbols: Vec<String>,
    /// Schemas: definition, trades, mbp-10.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "definition,trades,mbp-10"
    )]
    schemas: Vec<String>,
    /// Inclusive start (UTC): 2026-11-02 or 2026-11-02T13:30.
    #[arg(long)]
    start: String,
    /// Exclusive end (UTC).
    #[arg(long)]
    end: String,
    #[arg(long, default_value = "raw_symbol")]
    stype_in: String,
    #[arg(long, default_value = DATASET)]
    dataset: String,
}

impl Range {
    fn queries(&self) -> Vec<Query> {
        self.schemas
            .iter()
            .map(|schema| Query {
                dataset: self.dataset.clone(),
                schema: schema.trim().to_string(),
                symbols: self.symbols.iter().map(|s| s.trim().to_string()).collect(),
                stype_in: self.stype_in.clone(),
                start: self.start.clone(),
                end: self.end.clone(),
            })
            .collect()
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Price a request with Databento's free cost check. Downloads nothing, costs nothing.
    Cost {
        #[command(flatten)]
        range: Range,
    },
    /// Download (DBN + zstd) after the cost check, only if the total is within --max-usd.
    Download {
        #[command(flatten)]
        range: Range,
        /// Refuse if the cost check says more than this (US dollars).
        #[arg(long)]
        max_usd: f64,
        #[arg(long, default_value = "data/databento")]
        out_dir: PathBuf,
    },
    /// List the contracts and days the definition records in these files cover.
    Contracts { files: Vec<PathBuf> },
    /// Convert DBN files into one Lines replay recording for one contract.
    Import {
        /// Lines contract (ESZ26, ESZ2026) or Databento raw symbol (ESZ6).
        #[arg(long)]
        contract: String,
        /// Output `.lfr` (e.g. recordings/esz6-2026-11-02.lfr).
        #[arg(long)]
        out: PathBuf,
        /// DBN files (.dbn or .dbn.zst): definition, trades and/or mbp-10, any days.
        files: Vec<PathBuf>,
    },
}

/// `ESZ26`, `ESZ2026` (Lines labels) or `ESZ6` (CME/Databento, year from today: the first
/// matching year from last year on).
fn parse_contract(s: &str) -> Result<lines_core::ContractKey> {
    if let Some(Target::Contract(k)) = catalog::parse_target(s) {
        return Ok(k);
    }
    let s = s.trim().to_ascii_uppercase();
    if let Some(d) = s.chars().last().and_then(|c| c.to_digit(10)) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let this_year = lines_core::CivilDate::from_utc(lines_core::UtcMillis(now_ms)).year;
        let floor = this_year - 1;
        let mut y = floor - floor % 10 + d as u16;
        if y < floor {
            y += 10;
        }
        let label = format!("{}{:02}", &s[..s.len() - 1], y % 100);
        if let Some(Target::Contract(k)) = catalog::parse_target(&label) {
            return Ok(k);
        }
    }
    bail!("{s:?} is not a futures contract (e.g. ESZ26 or ESZ6)")
}

fn file_name(q: &Query) -> String {
    let safe = |s: &str| s.replace(['/', '\\', ':', '.'], "_");
    format!(
        "{}-{}-{}-{}-{}.dbn.zst",
        safe(&q.dataset),
        q.schema,
        safe(&q.symbols.join("_")),
        safe(&q.start),
        safe(&q.end)
    )
}

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Cost { range } => {
            let key = ApiKey::load()?;
            let mut total = (0.0, 0u64);
            for q in range.queries() {
                let e = api::estimate(&key, &q).with_context(|| format!("cost of {}", q.schema))?;
                println!(
                    "{:<11} {:>12}  ${:.4}",
                    q.schema,
                    human_bytes(e.bytes),
                    e.usd
                );
                total.0 += e.usd;
                total.1 += e.bytes;
            }
            println!(
                "{:<11} {:>12}  ${:.4}",
                "total",
                human_bytes(total.1),
                total.0
            );
        }
        Cmd::Download {
            range,
            max_usd,
            out_dir,
        } => {
            let key = ApiKey::load()?;
            // Price everything first; download nothing unless the whole request fits.
            let queries = range.queries();
            let mut total = 0.0;
            for q in &queries {
                total += api::estimate(&key, q)?.usd;
            }
            if total > max_usd {
                bail!(
                    "the request costs ${total:.4}, more than --max-usd {max_usd}: nothing downloaded"
                );
            }
            let mut left = max_usd;
            for q in &queries {
                let out = out_dir.join(file_name(q));
                let (e, n) = api::download(&key, q, left, &out)?;
                left -= e.usd;
                println!(
                    "{}  ${:.4}  {} written",
                    out.display(),
                    e.usd,
                    human_bytes(n)
                );
            }
        }
        Cmd::Contracts { files } => {
            let mut map = lines_databento::defs::InstrumentMap::default();
            for f in &files {
                lines_databento::import::read_definitions(f, &mut map)?;
            }
            for (k, days) in map.contracts() {
                let p = catalog::get(k.product).expect("catalog product");
                let days: Vec<String> = days
                    .iter()
                    .map(|d| format!("{:04}-{:02}-{:02}", d.year, d.month, d.day))
                    .collect();
                println!(
                    "{} ({})  {}",
                    k.label(&p),
                    raw_symbol(k).unwrap_or_default(),
                    days.join(" ")
                );
            }
            println!("{} definitions", map.definitions);
        }
        Cmd::Import {
            contract,
            out,
            files,
        } => {
            let contract = parse_contract(&contract)?;
            let report = import(&ImportOptions {
                inputs: files,
                contract,
                out: out.clone(),
            })?;
            println!("{report}");
            println!("wrote {}", out.display());
        }
    }
    Ok(())
}
