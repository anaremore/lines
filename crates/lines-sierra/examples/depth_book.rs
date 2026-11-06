//! Dev diagnostic: rebuild a Sierra `.depth` book as the importer does and check it against
//! the `.scid` trades and against Sierra's own periodic book snapshots.
//!
//! `cargo run --release -p lines-sierra --example depth_book -- <Data dir> <SYMBOL> <YYYY-MM-DD> [HH:MM:SS[.mmm] ...]`
//!
//! Prints the book (top 15 levels) at each requested UTC instant with the trades around it,
//! then:
//! - first fills (standalone trades and first sub-trades) inside the book at emit time,
//!   and how many of the rest a different order *within their millisecond* would explain
//!   (depth times are whole milliseconds, so that order is not in the files);
//! - how often the book equals the bid/ask Sierra stored on each trade record, with the
//!   book read at trade time + offset (shows which clock the stored bid/ask follows);
//! - Sierra's clear + full-book snapshots (every ~10 min) against the incremental book;
//! - the book's shape per hour: mean size of the 10 best levels, how many of the 10 tick
//!   rows from the touch hold a level, spread.
//!
//! Output is printed only; nothing is written (decision 0010).

use std::collections::BTreeMap;
use std::path::Path;

use lines_core::{OrderBook, Price, Qty, Side, UtcMillis};
use lines_sierra::format::{
    DepthCommand, DepthReader, DepthRecord, ScidReader, ScidRecord, TickKind, sc_to_unix_us,
    unix_us_to_sc,
};
use lines_sierra::import::{PriceConv, depth_path, scid_path, trade_before_batch};

/// Levels / tick rows from the touch used for the shape and snapshot checks.
const NEAR: usize = 10;
/// Trades printed either side of each requested instant.
const TRADES_AROUND: usize = 6;

#[derive(Default)]
struct Hour {
    samples: u64,
    mean_qty: f64,
    rows_filled: f64,
    spread_ticks: i64,
}

/// Apply one record exactly as the importer does: add/modify carry the level's new total,
/// delete removes it.
fn apply(book: &mut OrderBook, conv: &PriceConv, rec: &DepthRecord) {
    let (side, delete) = match rec.command {
        DepthCommand::ClearBook => return book.clear(),
        DepthCommand::AddBid | DepthCommand::ModifyBid => (Side::Bid, false),
        DepthCommand::AddAsk | DepthCommand::ModifyAsk => (Side::Ask, false),
        DepthCommand::DeleteBid => (Side::Bid, true),
        DepthCommand::DeleteAsk => (Side::Ask, true),
        DepthCommand::Unknown(_) => return,
    };
    let Ok(p) = conv.convert(rec.price) else {
        return;
    };
    let q = if delete { 0 } else { rec.quantity as i64 };
    book.set(side, p, Qty(q), rec.num_orders as u32);
}

fn bbo(book: &OrderBook) -> Option<(Price, Price)> {
    Some((book.best_bid()?.0, book.best_ask()?.0))
}

fn print_book(book: &OrderBook, tick: i64) {
    let fmt = |p: Price| format!("{:.2}", p.0 as f64 / 100.0);
    let asks: Vec<_> = book.levels(Side::Ask).take(15).collect();
    for (p, l) in asks.iter().rev() {
        println!(
            "                {:>10}  {:>4} ({} orders)",
            fmt(*p),
            l.qty.0,
            l.orders
        );
    }
    let spread = bbo(book).map(|(b, a)| (a.0 - b.0) / tick);
    println!("                ---------- spread {spread:?} ticks");
    for (p, l) in book.levels(Side::Bid).take(15) {
        println!("  {:>4} ({:>2} orders)  {:>10}", l.qty.0, l.orders, fmt(p));
    }
}

/// Mean size of the `NEAR` best levels (both sides) and the fraction of the `NEAR` tick rows
/// from each touch that hold a level.
fn shape(book: &OrderBook, tick: i64) -> Option<(f64, f64)> {
    let (bb, ba) = bbo(book)?;
    let (mut qty, mut n, mut filled) = (0i64, 0i64, 0i64);
    for side in [Side::Bid, Side::Ask] {
        for (p, l) in book.levels(side).take(NEAR) {
            qty += l.qty.0;
            n += 1;
            let dist = match side {
                Side::Bid => bb.0 - p.0,
                Side::Ask => p.0 - ba.0,
            } / tick;
            if dist < NEAR as i64 {
                filled += 1;
            }
        }
    }
    Some((
        qty as f64 / n.max(1) as f64,
        filled as f64 / (2 * NEAR) as f64,
    ))
}

fn pct(a: u64, b: u64) -> f64 {
    100.0 * a as f64 / b.max(1) as f64
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        anyhow::bail!("usage: depth_book <Data dir> <SYMBOL> <YYYY-MM-DD> [HH:MM:SS[.mmm] ...]");
    }
    let (data, symbol, date) = (Path::new(&args[1]), &args[2], &args[3]);
    let root = lines_sierra::symbol::parse(symbol)
        .ok_or_else(|| anyhow::anyhow!("bad symbol {symbol}"))?
        .root;
    let (product, mult) = lines_sierra::symbol::map_root(&root)
        .ok_or_else(|| anyhow::anyhow!("unmapped root {root}"))?;
    let conv = PriceConv::new(&product, mult);
    let tick = product.price.tick;
    let day_us = UtcMillis::parse_iso(&format!("{date}T00:00:00"))
        .ok_or_else(|| anyhow::anyhow!("bad date {date}"))?
        .0
        * 1000;
    let mut at: Vec<i64> = args[4..]
        .iter()
        .map(|s| {
            UtcMillis::parse_iso(&format!("{date}T{s}"))
                .map(|t| t.0 * 1000)
                .ok_or_else(|| anyhow::anyhow!("bad time {s}"))
        })
        .collect::<Result<_, _>>()?;
    at.sort();

    // The day's trades (at most a few hundred thousand records).
    let mut scid = ScidReader::open(&scid_path(data, symbol))?;
    scid.seek_to_time(unix_us_to_sc(day_us))?;
    let mut trades: Vec<ScidRecord> = Vec::new();
    while let Some(r) = scid.next_record()? {
        if sc_to_unix_us(r.time) >= day_us + 86_400_000_000 {
            break;
        }
        if r.is_tick() && r.total_volume > 0 {
            trades.push(r);
        }
    }
    let volume: u64 = trades.iter().map(|r| r.total_volume as u64).sum();
    println!("{symbol} {date}: {} trades, volume {volume}", trades.len());

    let mut depth = DepthReader::open(&depth_path(data, symbol, date))?;
    let mut book = OrderBook::default();
    let mut batch = Vec::new();
    let (mut ti, mut ai) = (0usize, 0usize);
    let mut in_bundle = false;
    let mut next_sample = day_us;
    let mut hours: BTreeMap<i64, Hour> = BTreeMap::new();
    // BBO after each batch: (batch time, bid, ask).
    let mut history: Vec<(i64, Price, Price)> = Vec::new();
    let (mut first_in, mut outside) = (0u64, Vec::<(i64, Price, i64)>::new());
    let mut pre_clear: Option<OrderBook> = None;
    let mut snapshot_diffs: BTreeMap<usize, u64> = BTreeMap::new();

    while let Some(_complete) = depth.next_batch(&mut batch)? {
        let bt = sc_to_unix_us(batch[0].time);
        while ti < trades.len() && trade_before_batch(sc_to_unix_us(trades[ti].time), bt) {
            let r = &trades[ti];
            ti += 1;
            let first = match r.kind() {
                TickKind::FirstSubTrade => {
                    in_bundle = true;
                    true
                }
                TickKind::LastSubTrade => {
                    in_bundle = false;
                    false
                }
                _ => !in_bundle,
            };
            let (Some((b, a)), Ok(p), true) = (bbo(&book), conv.convert(r.close), first) else {
                continue;
            };
            if b <= p && p <= a {
                first_in += 1;
            } else {
                let dist = if p < b { b.0 - p.0 } else { p.0 - a.0 } / tick;
                outside.push((sc_to_unix_us(r.time), p, dist));
            }
        }
        while next_sample <= bt {
            if let (Some((m, f)), Some((b, a))) = (shape(&book, tick), bbo(&book)) {
                let h = hours
                    .entry((next_sample - day_us) / 3_600_000_000)
                    .or_default();
                h.samples += 1;
                h.mean_qty += m;
                h.rows_filled += f;
                h.spread_ticks += (a.0 - b.0) / tick;
            }
            next_sample += 60_000_000;
        }
        while ai < at.len() && at[ai] <= bt {
            let t = at[ai];
            ai += 1;
            println!("\n=== book at {} ===", UtcMillis(t / 1000));
            print_book(&book, tick);
            let i = trades.partition_point(|r| sc_to_unix_us(r.time) < t);
            println!(
                "  trades around (time, price, qty, aggressor, Sierra's stored bid/ask, role):"
            );
            let lo = i.saturating_sub(TRADES_AROUND);
            for r in &trades[lo..(i + TRADES_AROUND).min(trades.len())] {
                println!(
                    "    {} {} {:>9.2} x{:<3} {} bid {:.2} ask {:.2} {:?}",
                    if sc_to_unix_us(r.time) < t { "-" } else { "+" },
                    UtcMillis(sc_to_unix_us(r.time) / 1000),
                    r.close / 100.0,
                    r.total_volume,
                    if r.ask_volume > 0 { "B" } else { "S" },
                    r.low / 100.0,
                    r.high / 100.0,
                    r.kind()
                );
            }
        }
        for rec in &batch {
            if rec.command == DepthCommand::ClearBook && pre_clear.is_none() && !book.is_empty() {
                pre_clear = Some(book.clone());
            }
            apply(&mut book, &conv, rec);
        }
        if let Some((b, a)) = bbo(&book) {
            history.push((bt, b, a));
            if let Some(pre) = pre_clear.take() {
                // Sizes only: snapshot records may omit order counts.
                let differ = [Side::Bid, Side::Ask]
                    .iter()
                    .flat_map(|&s| pre.levels(s).take(NEAR).map(move |l| (s, l)))
                    .filter(|(s, (p, l))| {
                        book.levels(*s)
                            .find(|(q, _)| q == p)
                            .is_none_or(|(_, m)| m.qty != l.qty)
                    })
                    .count();
                *snapshot_diffs.entry(differ).or_default() += 1;
            }
        }
    }

    let checked = first_in + outside.len() as u64;
    println!(
        "\nfirst fills inside the book at emit time: {first_in} / {checked} ({:.1}%)",
        pct(first_in, checked)
    );
    let mut dist: BTreeMap<i64, u64> = BTreeMap::new();
    let mut minutes: BTreeMap<i64, u64> = BTreeMap::new();
    let mut intra_ms = 0u64;
    for &(t, p, d) in &outside {
        *dist.entry(d.min(10)).or_default() += 1;
        *minutes.entry(t.div_euclid(60_000_000)).or_default() += 1;
        // Inside the book before the trade's millisecond or after any batch within it?
        let ms = t.div_euclid(1000) * 1000;
        let start = history.partition_point(|h| h.0 < ms).saturating_sub(1);
        let end = history.partition_point(|h| h.0 < ms + 1000);
        if history[start..end]
            .iter()
            .any(|&(_, b, a)| b <= p && p <= a)
        {
            intra_ms += 1;
        }
    }
    println!("  outside, by distance in ticks (10 = 10+): {dist:?}");
    println!(
        "  of the outside, inside the book at some batch boundary of their millisecond: {intra_ms} ({:.1}% of first fills unexplained)",
        pct(outside.len() as u64 - intra_ms, checked)
    );
    let mut top: Vec<_> = minutes.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!(
        "  minutes with the most: {:?}",
        top.iter()
            .take(5)
            .map(|(m, n)| format!("{} x{n}", UtcMillis(m * 60_000)))
            .collect::<Vec<_>>()
    );

    println!("\nbook bid/ask == Sierra's stored bid/ask, book read at trade time + offset:");
    for off_ms in [-1000i64, -100, -10, -2, -1, 0, 1, 10, 100, 1000] {
        let (mut n, mut same) = (0u64, 0u64);
        for r in &trades {
            let (Ok(rb), Ok(ra)) = (conv.convert(r.low), conv.convert(r.high)) else {
                continue;
            };
            let i = history.partition_point(|h| h.0 <= sc_to_unix_us(r.time) + off_ms * 1000);
            if let Some(&(_, b, a)) = i.checked_sub(1).map(|i| &history[i]) {
                n += 1;
                same += u64::from(b == rb && a == ra);
            }
        }
        println!("  {off_ms:>6} ms  {:>5.1}%", pct(same, n));
    }

    println!(
        "\nsnapshots: of the {NEAR} best levels per side before a clear, how many differ after the snapshot: {:?} (differing levels: snapshots)",
        snapshot_diffs
    );

    println!(
        "\nhour  mean qty, {NEAR} best levels  tick rows filled within {NEAR} of touch  spread (ticks)"
    );
    for (h, s) in &hours {
        let n = s.samples as f64;
        println!(
            "{h:>4}  {:>26.1}  {:>37.0}%  {:>14.2}",
            s.mean_qty / n,
            100.0 * s.rows_filled / n,
            s.spread_ticks as f64 / n
        );
    }
    Ok(())
}
