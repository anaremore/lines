//! `lines-import-sierra` — personal dev tool (see crate docs). Example:
//! `cargo run -p lines-sierra -- --symbol NQZ5.CME --date 2025-09-11 --out recordings/nqz5-2025-09-11.lfr`

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use lines_sierra::{ImportOptions, import};

/// Import one Sierra Chart symbol-day (ticks + depth) into a Lines replay file.
#[derive(Parser, Debug)]
struct Cli {
    /// Sierra `Data` folder. Defaults to $LINES_SIERRA_DATA.
    #[arg(long, env = "LINES_SIERRA_DATA")]
    data: PathBuf,
    /// Sierra symbol, e.g. NQZ5.CME.
    #[arg(long)]
    symbol: String,
    /// UTC day, YYYY-MM-DD.
    #[arg(long)]
    date: String,
    /// Output .lfr (keep it under recordings/, which is gitignored).
    #[arg(long)]
    out: PathBuf,
    /// Override Sierra's price multiplier (default per root; NQ observed = 100).
    #[arg(long)]
    price_multiplier: Option<f64>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let started = std::time::Instant::now();
    let report = import(&ImportOptions {
        data_dir: cli.data,
        symbol: cli.symbol,
        date: cli.date,
        out: cli.out.clone(),
        price_multiplier: cli.price_multiplier,
    })
    .context("import failed")?;
    println!("{report}");
    println!(
        "wrote {} in {:.1}s",
        cli.out.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
