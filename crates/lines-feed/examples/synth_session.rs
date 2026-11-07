//! Write two synthetic ES sessions as a `lines-events-1` replay (decision 0010: synthetic data
//! only); see `lines_feed::synth`.
//!
//! `cargo run -p lines-feed --example synth_session -- recordings/synth-es-2026-11-05.lfr [--depth] [--transit]`
//!
//! `--depth` adds a 10-level book around every trade (as the UI gallery uses); `--transit`
//! receives each frame 2-40 ms (now and then 150-400 ms) after its trade, for latency HUD
//! screenshots (roadmap 6b). The trades are the same either way.
//!
//! `--day N`: only trade date N (1 or 2) of the same trades, for a synthetic stored session:
//! day 1 into a local store, then day 2 replayed over it (`--store-dir`).

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings/synth-es-2026-11-05.lfr".into());
    let depth = std::env::args().any(|a| a == "--depth");
    let transit = std::env::args().any(|a| a == "--transit");
    let args: Vec<String> = std::env::args().collect();
    let day: Option<usize> = args
        .windows(2)
        .find(|w| w[0] == "--day")
        .and_then(|w| w[1].parse().ok());
    let path = std::path::Path::new(&out);
    let trades = match day {
        Some(d) => lines_feed::synth::write_es_day(path, d, depth, transit)?,
        None => lines_feed::synth::write_es_sessions(path, depth, transit)?,
    };
    println!("wrote {out}: {trades} trades, 2026-11-03T23:00:00.000Z .. 2026-11-05T22:00:00.000Z");
    Ok(())
}
