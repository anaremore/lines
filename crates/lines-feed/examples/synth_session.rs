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
//!
//! `--from ISO`: recorded only from that time on (the same trades after it): a replay that
//! starts mid-session, as a feed opened late does (a VWAP "from HH:MM"). Not with `--day`.

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
    let from = args
        .windows(2)
        .find(|w| w[0] == "--from")
        .map(|w| {
            lines_core::UtcMillis::parse_iso(&w[1])
                .ok_or_else(|| anyhow::anyhow!("--from {}: not an ISO time", w[1]))
        })
        .transpose()?;
    let path = std::path::Path::new(&out);
    let trades = match (day, from) {
        (Some(_), Some(_)) => anyhow::bail!("--from and --day: one or the other"),
        (Some(d), None) => lines_feed::synth::write_es_day(path, d, depth, transit)?,
        (None, Some(_)) => lines_feed::synth::write_es_sessions_from(path, depth, transit, from)?,
        (None, None) => lines_feed::synth::write_es_sessions(path, depth, transit)?,
    };
    let (start, end) = lines_feed::synth::span(day, from);
    println!("wrote {out}: {trades} trades, {start} .. {end}");
    Ok(())
}
