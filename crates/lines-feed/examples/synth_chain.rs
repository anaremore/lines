//! Write a synthetic SPY session with its option chain as a `lines-events-1` replay (decision
//! 0010: synthetic data only; decision 0028); see `lines_feed::synth_chain`.
//!
//! `cargo run -p lines-feed --example synth_chain -- recordings/synth-spy-chain-2026-11-09.lfr`

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings/synth-spy-chain-2026-11-09.lfr".into());
    let (trades, quotes) = lines_feed::synth_chain::write_spy_chain(std::path::Path::new(&out))?;
    println!(
        "wrote {out}: {trades} SPY trades and {quotes} option quotes, {} .. {}",
        lines_feed::synth_chain::OPEN,
        lines_feed::synth_chain::CLOSE
    );
    Ok(())
}
