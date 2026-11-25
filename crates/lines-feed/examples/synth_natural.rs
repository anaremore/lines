//! Two synthetic ES sessions with natural-looking price action, as a `lines-events-1` replay
//! (decision 0010: synthetic data only, not modelled on any real day). For screenshots and the
//! website; the fixed sessions of `synth_session` (`lines_feed::synth`) stay as they are, since
//! tests and docs depend on them.
//!
//! `cargo run -p lines-feed --example synth_natural -- recordings/natural.lfr [--seed N] [--depth] [--bars FILE]`
//!
//! Same trade dates and hours as `synth_session` (2026-11-04 and 2026-11-05: ETH from 23:00
//! UTC the evening before to 22:00 UTC, RTH 14:30-21:00 UTC, nothing in the 22:00-23:00 break).
//! Deterministic for a given seed (default 7). The model, one step per second:
//! - a fair value that drifts, with trend legs (a regime that switches between flat, up and down);
//! - price mean-reverting mildly around it, its volatility clustered (log-volatility AR(1),
//!   scaled by the time of day);
//! - news-like bursts: now and then the fair value moves 4-14 points over one to four minutes
//!   while volatility and activity jump and then decay;
//! - activity U-shaped over RTH (busy open and close, quiet lunch), thin overnight;
//! - trade sizes heavy-tailed (Pareto), aggressors leaning with the move;
//! - with `--depth`, a 10-level book on each side whose inside follows the trades: a buy lifts
//!   the ask, a sell hits the bid, so it is never crossed at a frame boundary.
//!
//! `--bars FILE` also writes 1-minute OHLCV bars as CSV (UTC ms, open, high, low, close,
//! volume; prices in points), to compare seeds without opening the app.

use std::f64::consts::TAU;
use std::io::Write as _;
use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, Side, UtcMillis};
use lines_feed::catalog;
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};

/// xorshift64*: small, deterministic, good enough for synthetic data.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // splitmix64 of the seed, so nearby seeds start far apart (and never at 0).
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        Rng((z ^ (z >> 31)) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// Uniform in (0, 1).
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    /// Standard normal (Box-Muller).
    fn normal(&mut self) -> f64 {
        (-2.0 * self.unit().ln()).sqrt() * (TAU * self.unit()).cos()
    }
    /// Poisson with mean `lambda` (small lambda: Knuth's method).
    fn poisson(&mut self, lambda: f64) -> u32 {
        let l = (-lambda).exp();
        let (mut k, mut p) = (0, 1.0);
        loop {
            p *= self.unit();
            if p <= l {
                return k;
            }
            k += 1;
        }
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

/// One trade date: UTC ETH open, RTH open/close, ETH close.
const DAYS: [[&str; 4]; 2] = [
    [
        "2026-11-03T23:00:00",
        "2026-11-04T14:30:00",
        "2026-11-04T21:00:00",
        "2026-11-04T22:00:00",
    ],
    [
        "2026-11-04T23:00:00",
        "2026-11-05T14:30:00",
        "2026-11-05T21:00:00",
        "2026-11-05T22:00:00",
    ],
];

/// Levels per side in the book.
const BOOK_LEVELS: i64 = 10;

/// Price in ticks (0.25 points) when the first session opens.
const START_TICKS: f64 = 6015.0 * 4.0;

/// Time of day as (volatility scale, trades per second) for `t`; `rth` = (open, close).
fn time_of_day(t: i64, rth: (i64, i64)) -> (f64, f64) {
    let (open, close) = rth;
    if t < open || t >= close {
        // Overnight: quiet, a little busier in the hour before the RTH open (Europe, data).
        let to_open = (open - t) as f64 / 3_600_000.0;
        let pre = if (0.0..1.5).contains(&to_open) {
            1.0 + 0.8 * (1.5 - to_open) / 1.5
        } else {
            1.0
        };
        return (0.7 * pre, 0.22 * pre);
    }
    // RTH: a U over the session, x in 0..1; busiest at the open, then the close.
    let x = (t - open) as f64 / (close - open) as f64;
    let u = 3.2 * (-x / 0.07).exp() + 1.4 * (-(1.0 - x) / 0.08).exp() + 0.55;
    (u.sqrt(), 1.9 * u)
}

/// Trend regime: points per hour the fair value drifts.
fn new_trend(rng: &mut Rng) -> (f64, i64) {
    let drift = match rng.unit() {
        u if u < 0.45 => 0.0,
        u if u < 0.72 => 3.0 + 6.0 * rng.unit(),
        _ => -(3.0 + 6.0 * rng.unit()),
    };
    // Lasts 25 minutes to 2 hours.
    (drift, 1_500 + (rng.unit() * 5_700.0) as i64)
}

/// Trade size: heavy-tailed (Pareto, alpha 1.5), mostly 1-3 lots, now and then a block.
fn trade_size(rng: &mut Rng, rth: bool) -> i64 {
    let q = (1.0 / rng.unit().powf(1.0 / 1.5)).floor() as i64;
    let q = q.clamp(1, if rth { 250 } else { 60 });
    if rth && rng.chance(0.25) { q + 1 } else { q }
}

struct Book {
    bid: i64,
    ask: i64,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "recordings/synth-natural.lfr".into());
    let opt = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let seed: u64 = opt("--seed").map_or(Ok(7), |s| s.parse())?;
    let depth = args.iter().any(|a| a == "--depth");
    let mut bars_out = opt("--bars").map(std::fs::File::create).transpose()?;

    let es = catalog::get(catalog::ES).expect("ES");
    let contract = ContractKey::monthly(es.id, 2026, Month::new(12).expect("month"));
    let tick = es.price.tick; // 25 units = 0.25 points
    let ms = |s: &str| UtcMillis::parse_iso(s).expect("time").0;
    let start = ms(DAYS[0][0]);
    let header = Header {
        start_wall_ms: start,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: es.symbol.into(),
        contract: Some(contract),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(std::path::Path::new(&out), &header)?;

    let mut rng = Rng::new(seed);
    // Book sizes draw from their own generator, so `--depth` leaves the trades unchanged.
    let mut book_rng = Rng::new(seed ^ 0xb00c);
    let mut price = START_TICKS; // continuous mid, in ticks
    let mut fair = START_TICKS;
    let mut log_vol = 0.0f64; // AR(1) around 0
    let mut trend = new_trend(&mut rng);
    // A news burst: remaining fair-value move (ticks) and its seconds left; extra volatility.
    let (mut news_move, mut news_left, mut news_heat) = (0.0f64, 0i64, 0.0f64);
    let mut book = Book {
        bid: START_TICKS as i64 - 1,
        ask: START_TICKS as i64,
    };
    let mut buf = Vec::new();
    let mut trades = 0u64;
    let mut bar: Option<(i64, [i64; 4], i64)> = None; // minute, OHLC (ticks), volume
    let mut offsets = Vec::new();

    for day in &DAYS {
        let (open, rth_open, rth_close, close) = (ms(day[0]), ms(day[1]), ms(day[2]), ms(day[3]));
        let rth = (rth_open, rth_close);
        let mut t = open;
        while t < close {
            let in_rth = (rth_open..rth_close).contains(&t);
            let (vol_scale, rate) = time_of_day(t, rth);

            // Regimes.
            trend.1 -= 1;
            if trend.1 <= 0 {
                trend = new_trend(&mut rng);
            }
            // News: about 3 a day in RTH, 1 overnight; the RTH open often gets one.
            let p_news = if in_rth {
                3.0 / 23_400.0
            } else {
                1.0 / 61_200.0
            };
            let opening = in_rth && t - rth_open == 5 * 60_000;
            if news_left == 0 && (rng.chance(p_news) || (opening && rng.chance(0.6))) {
                let size = (16.0 + 40.0 * rng.unit()) * if rng.chance(0.5) { 1.0 } else { -1.0 };
                news_move = size;
                news_left = 60 + (rng.unit() * 180.0) as i64;
                news_heat = 0.5 + 0.4 * rng.unit();
            }
            let mut news_step = 0.0;
            if news_left > 0 {
                news_step = news_move / news_left as f64;
                news_move -= news_step;
                news_left -= 1;
            }
            news_heat *= 0.997; // half-life about 4 minutes

            // Fair value: trend drift, its own slow noise, and the news move.
            let drift = trend.0 * 4.0 / 3_600.0 * if in_rth { 1.0 } else { 0.4 };
            fair += drift + 0.22 * rng.normal() + news_step;
            // Volatility clustering: log-vol AR(1), half-life about 10 minutes.
            log_vol = (0.99885 * log_vol + 0.045 * rng.normal()).clamp(-0.8, 0.9);
            let sigma = 0.55 * vol_scale * (log_vol + news_heat).exp();
            // Mild mean reversion to the fair value, faster during news (price chases it).
            let kappa = if news_left > 0 {
                1.0 / 25.0
            } else {
                1.0 / 900.0
            };
            let before = price;
            price += kappa * (fair - price) + sigma * rng.normal();
            let push = (price - before) / sigma.max(0.05);

            // Trades this second, spread over its milliseconds in order.
            let n = rng.poisson(rate * (log_vol * 0.6 + news_heat).exp());
            offsets.clear();
            offsets.extend((0..n).map(|_| (rng.unit() * 1000.0) as i64));
            offsets.sort_unstable();
            offsets.dedup();
            let target = price.round() as i64;
            for &off in &offsets {
                // The inside moves a tick at a time towards the price.
                if target >= book.ask {
                    book.ask += 1;
                    book.bid = book.ask - 1;
                } else if target < book.bid {
                    book.bid -= 1;
                    book.ask = book.bid + 1;
                }
                // Aggressors lean with the move.
                let p_buy = 0.5 + 0.3 * (push * 0.8).tanh();
                let (aggressor, at) = if rng.chance(p_buy) {
                    (Aggressor::Buy, book.ask)
                } else {
                    (Aggressor::Sell, book.bid)
                };
                let qty = trade_size(&mut rng, in_rth);
                let ts = t + off;
                buf.clear();
                let time_us = ts * 1000;
                if depth {
                    native::encode(&NativeEvent::BookClear { time_us }, &mut buf);
                    for k in 0..BOOK_LEVELS {
                        for (side, p) in [(Side::Bid, book.bid - k), (Side::Ask, book.ask + k)] {
                            // Thicker away from the inside, with some noise.
                            let base = 6 + 4 * k + (book_rng.unit() * (8 + 3 * k) as f64) as i64;
                            native::encode(
                                &NativeEvent::BookLevel {
                                    time_us,
                                    side,
                                    price: Price(p * tick),
                                    qty: Qty(if in_rth { base * 2 } else { base }),
                                    orders: 1,
                                },
                                &mut buf,
                            );
                        }
                    }
                }
                native::encode(
                    &NativeEvent::Trade {
                        time_us,
                        price: Price(at * tick),
                        qty: Qty(qty),
                        aggressor,
                    },
                    &mut buf,
                );
                w.write(
                    Direction::FromServer,
                    Duration::from_millis((ts - start) as u64),
                    &Frame::binary(buf.clone()),
                )?;
                trades += 1;
                let minute = ts / 60_000 * 60_000;
                match &mut bar {
                    Some((m, ohlc, v)) if *m == minute => {
                        ohlc[1] = ohlc[1].max(at);
                        ohlc[2] = ohlc[2].min(at);
                        ohlc[3] = at;
                        *v += qty;
                    }
                    _ => {
                        if let (Some(f), Some((m, o, v))) = (&mut bars_out, bar) {
                            write_bar(f, m, o, v)?;
                        }
                        bar = Some((minute, [at, at, at, at], qty));
                    }
                }
            }
            t += 1_000;
        }
        // The break: the price rests at the last trade.
        price = (book.bid + book.ask) as f64 / 2.0;
        fair = 0.7 * fair + 0.3 * price;
    }
    if let (Some(f), Some((m, o, v))) = (&mut bars_out, bar) {
        write_bar(f, m, o, v)?;
    }
    w.finish()?;
    let last = book.ask as f64 * 0.25;
    println!(
        "wrote {out}: seed {seed}, {trades} trades, {} .. {}, last {last:.2}",
        UtcMillis(start),
        UtcMillis(ms(DAYS[1][3]))
    );
    Ok(())
}

fn write_bar(f: &mut std::fs::File, minute: i64, o: [i64; 4], v: i64) -> std::io::Result<()> {
    let p = |x: i64| x as f64 * 0.25;
    writeln!(
        f,
        "{minute},{:.2},{:.2},{:.2},{:.2},{v}",
        p(o[0]),
        p(o[1]),
        p(o[2]),
        p(o[3])
    )
}
