//! Write two synthetic ES sessions as a `lines-events-1` replay (decision 0010: synthetic data
//! only); see `lines_feed::synth`.
//!
//! `cargo run -p lines-feed --example synth_session -- recordings/synth-es-2026-11-05.lfr [--depth] [--transit]`
//!
//! `--depth` adds a 10-level book around every trade (as the UI gallery uses); `--transit`
//! receives each frame 2-40 ms (now and then 150-400 ms) after its trade, for latency HUD
//! screenshots (roadmap 6b). The trades are the same either way.

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings/synth-es-2026-11-05.lfr".into());
    let depth = std::env::args().any(|a| a == "--depth");
    let transit = std::env::args().any(|a| a == "--transit");
    let trades = lines_feed::synth::write_es_sessions(std::path::Path::new(&out), depth, transit)?;
    println!("wrote {out}: {trades} trades, 2026-11-03T23:00:00.000Z .. 2026-11-05T22:00:00.000Z");
    Ok(())
}
