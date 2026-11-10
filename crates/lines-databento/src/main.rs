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
    /// Whole past trade dates into the local store, as the app's "Download days…" does:
    /// the estimate per day (free), and with --download only the days whose estimate is
    /// within --max-usd (default 0: only days included in the plan). Stored days are never
    /// estimated or fetched again.
    Days {
        /// Lines contract (ESZ26) or Databento raw symbol (ESZ6).
        #[arg(long)]
        contract: String,
        /// First trade date (YYYY-MM-DD).
        #[arg(long)]
        from: String,
        /// Last trade date (default: --from).
        #[arg(long)]
        to: Option<String>,
        /// tob (trades + top of book: trades, mbp-1) or mbp-10 (full depth).
        #[arg(long, default_value = "tob")]
        contents: String,
        /// Fetch the days (else only the estimate).
        #[arg(long)]
        download: bool,
        /// Download only if the whole range is estimated at most this (US dollars; default 0:
        /// nothing that costs money). The total is printed first.
        #[arg(long, default_value_t = 0.0)]
        max_usd: f64,
        /// Store root (default: LINES_DATA_DIR, else the OS data directory).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
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
        Cmd::Days {
            contract,
            from,
            to,
            contents,
            download,
            max_usd,
            data_dir,
        } => days(contract, from, to, contents, download, max_usd, data_dir)?,
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

fn parse_day(s: &str) -> Result<lines_core::CivilDate> {
    let mut it = s.trim().split('-');
    let y = it.next().and_then(|v| v.parse::<u16>().ok());
    let m = it.next().and_then(|v| v.parse::<u8>().ok());
    let d = it.next().and_then(|v| v.parse::<u8>().ok());
    match (y, m, d, it.next()) {
        (Some(y), Some(m), Some(d), None) => lines_core::CivilDate::new(y, m, d),
        _ => None,
    }
    .with_context(|| format!("{s:?} is not a date (YYYY-MM-DD)"))
}

/// `days`: the public download engine with the Databento source, into the local store.
fn days(
    contract: String,
    from: String,
    to: Option<String>,
    contents: String,
    download: bool,
    max_usd: f64,
    data_dir: Option<PathBuf>,
) -> Result<()> {
    use lines_databento::days::{DEPTH, DatabentoDays, TOB};
    use lines_feed::disk::day::DayState;
    use lines_feed::download::{
        self, Cancel, DayError, Outcome, PastDays, Target, Update, size_text,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let contract = parse_contract(&contract)?;
    let product = catalog::get(contract.product).context("catalog product")?;
    let contents = match contents.as_str() {
        "tob" => TOB.id,
        "mbp-10" => DEPTH.id,
        other => bail!("--contents {other:?}: tob or mbp-10"),
    };
    let first = parse_day(&from)?;
    let last = match &to {
        Some(t) => parse_day(t)?,
        None => first,
    };
    let dates = download::dates(first, last).map_err(anyhow::Error::msg)?;
    let root = match data_dir {
        Some(d) => d,
        None => {
            lines_feed::disk::default_root().context("no data directory: set LINES_DATA_DIR")?
        }
    };
    let target = Target {
        root: root.clone(),
        source: "databento".into(),
        product: product.clone(),
        contract,
    };
    let source: Arc<dyn PastDays> = Arc::new(DatabentoDays);
    let now = lines_feed::now_utc();
    println!(
        "{} {} into {}",
        contract.label(&product),
        contents,
        target.dir().path.display()
    );
    for (d, state) in download::plan(&target, &dates, now) {
        match state {
            DayState::Missing => {}
            DayState::Stored { complete } => println!(
                "{d}  already stored{}",
                if complete { "" } else { " (partly)" }
            ),
            DayState::NoSession => println!("{d}  no session"),
            DayState::NotFinished => println!("{d}  session not over yet"),
            DayState::Busy => println!("{d}  busy: a live feed is recording this contract"),
        }
    }
    let job = download::estimate(
        source.clone(),
        target.clone(),
        contents,
        dates,
        now,
        Cancel::new(),
    );
    // --max-usd is a budget for the whole range, as `download` treats it.
    let limit = (max_usd.max(0.0) * 1e6).floor() as u64;
    let mut take = Vec::new();
    while let Some(u) = job.next(Duration::from_secs(600)) {
        match u {
            Update::Estimate { date, result } => match result {
                Ok(e) => {
                    println!("{date}  {}  {}", e.charge.text(), size_text(e.bytes));
                    take.push((date, e.charge));
                }
                Err(DayError::Refused(r)) => println!("{date}  Databento refused: {r}"),
                Err(DayError::Failed(m)) => println!("{date}  couldn't check the cost: {m}"),
            },
            Update::Finished => break,
            _ => {}
        }
    }
    job.join();
    let total: u64 = take.iter().map(|(_, c)| c.usd_micros()).sum();
    println!(
        "total  {} for {} day(s)",
        download::usd_text(total),
        take.len()
    );
    if !download || take.is_empty() {
        return Ok(());
    }
    if total > limit {
        bail!(
            "the days cost {}, more than --max-usd {max_usd}: nothing downloaded",
            download::usd_text(total)
        );
    }
    let mut started = Instant::now();
    let job = download::download(source, target, contents, take, now, Cancel::new());
    let mut last_print = Instant::now();
    let mut failed = 0usize;
    while let Some(u) = job.next(Duration::from_secs(3600)) {
        match u {
            Update::Started { date } => {
                started = Instant::now();
                println!("{date}  downloading");
            }
            Update::Progress { date, received } => {
                if last_print.elapsed() >= Duration::from_secs(10) {
                    last_print = Instant::now();
                    println!("{date}  {} received", size_text(received));
                }
            }
            Update::Day { date, outcome } => match outcome {
                Outcome::Stored(w) => println!(
                    "{date}  stored {} ({} blocks, {} trades, {} outside the session) in {:.1} s: {}",
                    size_text(w.bytes),
                    w.blocks,
                    w.trades,
                    w.outside,
                    started.elapsed().as_secs_f64(),
                    w.path.display()
                ),
                other => {
                    failed += 1;
                    let why = match other {
                        Outcome::Skipped(s) => format!("not downloaded ({s:?})"),
                        Outcome::Busy => "a live feed is recording this contract".into(),
                        Outcome::Refused(m) => format!("Databento refused: {m}"),
                        Outcome::PriceChanged(c) => {
                            format!("not downloaded: the price is now {}", c.text())
                        }
                        Outcome::Canceled => "canceled: nothing stored".into(),
                        Outcome::Failed(m) => format!("not stored: {m}"),
                        Outcome::Stored(_) => unreachable!(),
                    };
                    eprintln!("{date}  {why}");
                }
            },
            Update::Finished => break,
            Update::Estimate { .. } | Update::Plan { .. } | Update::Requested { .. } => {}
        }
    }
    job.join();
    if failed > 0 {
        bail!("{failed} day(s) not stored");
    }
    Ok(())
}
