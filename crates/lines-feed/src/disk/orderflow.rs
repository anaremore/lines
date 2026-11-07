//! Order flow from the local store at feed startup (decision 0019).
//!
//! History bars carry no trades, so after a connect the volume profile, footprint, cumulative
//! delta, VWAP and the DOM's volume at price would start empty (or bar-derived). The store
//! often holds those trades already: [`read`] replays them, one pass over the segments, into
//! every consumer that counts trades (`ChartData::apply_stored`, the indicator engine's
//! `Input::Stored`).
//!
//! Which stored trades count:
//! - only inside the contract's **trade coverage** (`ContractDir::trade_coverage`): segments
//!   also hold trades outside it (re-delivered overlap, REST back-fill), which were never
//!   counted live and are not complete there;
//! - only from the **lookback** start (`from_ms`);
//! - a **re-delivery** is left out: segments are in arrival order, and when a trade's time
//!   goes back below the newest counted one (a reconnect re-sending trades already stored),
//!   trades are skipped until time passes that mark again;
//! - a segment that ends **corrupt or torn** stops there: its coverage after the last good
//!   block is withdrawn, so consumers stay approximate for the rest of that day.

use std::io;
use std::time::{Duration, Instant};

use lines_core::{Aggressor, CivilDate, Print, UtcMillis};

use super::segment::{End, SegmentReader};
use super::{ContractDir, Ranges, partition};
use crate::native::{self, NativeEvent};

/// Default lookback: the newest stored trade date and the one before (the current and the
/// previous session).
pub const DEFAULT_SESSIONS: u32 = 2;

/// One trade date's stored order flow, in arrival order.
#[derive(Clone, Debug, Default)]
pub struct Chunk {
    pub date: Option<CivilDate>,
    pub trades: Vec<Print>,
    /// Inclusive ms ranges inside this date where `trades` is every trade.
    pub covered: Vec<(i64, i64)>,
}

/// What a [`read`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadStats {
    pub dates: usize,
    pub blocks: u64,
    /// Trades decoded from the segments read.
    pub decoded: u64,
    /// Trades replayed (covered, inside the lookback, not re-delivered).
    pub replayed: u64,
    /// Trades left out as re-delivered.
    pub redelivered: u64,
    /// Segments that ended corrupt or torn (coverage withdrawn after them).
    pub cut: usize,
    pub elapsed: Duration,
}

/// Where the lookback starts: the open of the oldest of the newest `sessions` stored trade
/// dates at or before `reference` (the newest stored data). `None`: off, or nothing stored.
pub fn lookback_start(dir: &ContractDir, reference: UtcMillis, sessions: u32) -> Option<i64> {
    if sessions == 0 {
        return None;
    }
    let newest = partition::trade_date(&dir.product, reference);
    let dates: Vec<CivilDate> = dir.dates().into_iter().filter(|d| *d <= newest).collect();
    let first = dates.iter().rev().take(sessions as usize).last()?;
    Some(partition::trade_date_span(&dir.product, *first).0.0)
}

/// Read the stored trades from `from_ms` on, oldest trade date first, handing each date's
/// [`Chunk`] to `each` (one pass; nothing is kept between dates but the re-delivery mark).
pub fn read(dir: &ContractDir, from_ms: i64, mut each: impl FnMut(Chunk)) -> io::Result<ReadStats> {
    let started = Instant::now();
    let mut stats = ReadStats::default();
    let mut coverage = dir.trade_coverage();
    coverage.remove(i64::MIN, from_ms - 1);
    if coverage.is_empty() {
        return Ok(stats);
    }
    // Newest counted trade time, and whether a re-delivery is being skipped.
    let mut mark: Option<i64> = None;
    let mut redelivery = false;
    let mut events = Vec::new();
    for date in dir.dates() {
        let (open, close) = partition::trade_date_span(&dir.product, date);
        let (open, close) = (open.0, close.0 - 1);
        if coverage.holes(open, close) == [(open, close)] {
            continue; // nothing covered on this date (or before the lookback)
        }
        stats.dates += 1;
        let mut day = Ranges::default();
        for &(a, b) in coverage.ranges() {
            day.add(a.max(open), b.min(close));
        }
        let mut chunk = Chunk {
            date: Some(date),
            ..Chunk::default()
        };
        let mut r = SegmentReader::open(&dir.segment_path(date))?;
        // The newest time of a block read in full: a cut withdraws coverage after it.
        let mut good_to = open - 1;
        loop {
            let block = match r.next_block()? {
                Ok(b) => b,
                Err(End::Eof) => break,
                Err(End::Partial | End::Corrupt) => {
                    stats.cut += 1;
                    day.remove(good_to + 1, close);
                    chunk.trades.retain(|p| p.time.0 <= good_to);
                    break;
                }
            };
            stats.blocks += 1;
            events.clear();
            if native::decode(&block.events, &mut events).is_err() {
                // Undecodable but intact (a newer encoding): its trades are unknown.
                day.remove(good_to + 1, close);
                stats.cut += 1;
                break;
            }
            for e in &events {
                let NativeEvent::Trade {
                    time_us,
                    price,
                    qty,
                    aggressor,
                } = *e
                else {
                    continue;
                };
                stats.decoded += 1;
                let t = time_us.div_euclid(1000);
                good_to = good_to.max(t);
                if !day.contains(t) {
                    continue;
                }
                if let Some(m) = mark {
                    if t < m {
                        redelivery = true;
                        stats.redelivered += 1;
                        continue;
                    }
                    if redelivery {
                        if t == m {
                            stats.redelivered += 1;
                            continue;
                        }
                        redelivery = false;
                    }
                }
                mark = Some(mark.map_or(t, |m| m.max(t)));
                chunk.trades.push(Print {
                    time: UtcMillis(t),
                    price,
                    qty,
                    aggressor: match aggressor {
                        native::Aggressor::Buy => Some(Aggressor::Buy),
                        native::Aggressor::Sell => Some(Aggressor::Sell),
                        native::Aggressor::Unknown => None,
                    },
                });
            }
            good_to = good_to.max(block.time_us.div_euclid(1000));
        }
        stats.replayed += chunk.trades.len() as u64;
        chunk.covered = day.ranges().to_vec();
        each(chunk);
    }
    stats.elapsed = started.elapsed();
    Ok(stats)
}
