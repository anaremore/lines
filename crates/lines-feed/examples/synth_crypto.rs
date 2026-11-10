//! Write a synthetic BTC/USD session as a `lines-events-1` replay (decision 0010: synthetic
//! data only); see `lines_feed::synth_crypto`.
//!
//! `cargo run -p lines-feed --example synth_crypto -- recordings/synth-btc-2026-11-09.lfr`

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings/synth-btc-2026-11-09.lfr".into());
    let n = lines_feed::synth_crypto::write_btc_session(std::path::Path::new(&out))?;
    println!(
        "wrote {out}: {n} BTC/USD trades, {} .. {}",
        lines_feed::synth_crypto::OPEN,
        lines_feed::synth_crypto::CLOSE
    );
    Ok(())
}
