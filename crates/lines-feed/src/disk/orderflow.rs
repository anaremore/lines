//! Order flow from the local store at feed startup (decision 0019).
//!
//! History bars carry no trades, so after a connect the volume profile, footprint, cumulative
//! delta, VWAP and the DOM's volume at price would start empty (or bar-derived). The store
//! often holds those trades already: [`read`] replays them, one pass over the segments, into
//! every consumer that counts trades (`ChartData::apply_stored`, the indicator engine's
//! `Input::Stored`).
//!
//! Which stored trades count:
//! - the segments hold exactly the trades live counted (the writer stores the trades
//!   `ChartData::apply` counted in each frame), late ones included, a reconnect's re-sent
//!   ones not, so a stored session replays to what live built;
//! - only inside the contract's **trade coverage** (`ContractDir::trade_coverage`): outside it
//!   (an outage, a connection whose history never completed) the stored trades are not known
//!   to be every trade;
//! - only from the **lookback** start (`from_ms`);
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
    /// Trades replayed (covered, inside the lookback).
    pub replayed: u64,
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
    let first = dates.get(dates.len().saturating_sub(sessions as usize))?;
    Some(partition::trade_date_span(&dir.product, *first).0.0)
}

/// Read the stored trades from `from_ms` on, oldest trade date first, handing each date's
/// [`Chunk`] to `each` (one pass; nothing is kept between dates).
pub fn read(dir: &ContractDir, from_ms: i64, mut each: impl FnMut(Chunk)) -> io::Result<ReadStats> {
    let started = Instant::now();
    let mut stats = ReadStats::default();
    let mut coverage = dir.trade_coverage();
    coverage.remove(i64::MIN, from_ms.saturating_sub(1));
    if coverage.is_empty() {
        return Ok(stats);
    }
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
        let mut r = match SegmentReader::open(&dir.segment_path(date)) {
            Ok(r) => r,
            // Evicted meanwhile (retention runs on the writer's thread): nothing to replay.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::disk::Coverage;
    use crate::disk::segment::{ENCODING, FlushPolicy, SegmentHeader, SegmentWriter};
    use lines_core::{ContractKey, Month, Price, Qty};

    fn dir(name: &str) -> ContractDir {
        let root =
            std::env::temp_dir().join(format!("lines-orderflow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let es = catalog::get(catalog::ES).unwrap();
        let k = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        ContractDir::new(&root, "test", &es, Some(k))
    }

    /// Monday 2026-11-10's ES session: 2026-11-09T23:00Z to 2026-11-10T22:00Z.
    fn monday() -> (CivilDate, i64) {
        let d = CivilDate::new(2026, 11, 10).unwrap();
        (d, UtcMillis::parse_iso("2026-11-09T23:00:00Z").unwrap().0)
    }

    fn write_day(dir: &ContractDir, date: CivilDate, blocks: &[&[i64]]) {
        let header = SegmentHeader {
            created_wall_ms: 0,
            source: dir.source.clone(),
            encoding: ENCODING.into(),
            product: dir.product.symbol.into(),
            contract: dir.contract,
            date,
        };
        let (mut w, _) =
            SegmentWriter::open(&dir.segment_path(date), &header, FlushPolicy::default()).unwrap();
        for times in blocks {
            let mut buf = Vec::new();
            for &t in *times {
                native::encode(
                    &NativeEvent::Trade {
                        time_us: t * 1000,
                        price: Price(600_000),
                        qty: Qty(1),
                        aggressor: native::Aggressor::Buy,
                    },
                    &mut buf,
                );
            }
            w.append(times.iter().max().unwrap() * 1000, &buf).unwrap();
        }
        w.close().unwrap();
    }

    fn cover(dir: &ContractDir, from: i64, to: i64) {
        let mut c = Coverage::default();
        c.trades.add(from, to);
        c.save(&dir.coverage_path()).unwrap();
    }

    /// Every covered stored trade replays, in arrival order, late ones included (a REST fill
    /// of trades older than newer streamed ones, as live counted them).
    #[test]
    fn late_trades_inside_the_coverage_replay() {
        let d = dir("late");
        let (date, open) = monday();
        let t = |s: i64| open + s * 1000;
        write_day(&d, date, &[&[t(3), t(4)], &[t(1), t(2)], &[t(4), t(5)]]);
        cover(&d, open, open + 3_600_000);
        let mut got = Vec::new();
        let s = read(&d, i64::MIN, |c| {
            got.extend(c.trades.iter().map(|p| p.time.0))
        })
        .unwrap();
        assert_eq!(got, vec![t(3), t(4), t(1), t(2), t(4), t(5)]);
        assert_eq!((s.decoded, s.replayed, s.cut), (6, 6, 0));
    }

    /// Only covered trades replay: outside the claims (and before the lookback) they were
    /// never complete.
    #[test]
    fn only_covered_trades_inside_the_lookback_replay() {
        let d = dir("covered");
        let (date, open) = monday();
        let t = |s: i64| open + s * 1000;
        write_day(&d, date, &[&[t(1), t(2), t(3), t(10), t(11)]]);
        cover(&d, t(2), t(10));
        let mut chunks = Vec::new();
        read(&d, t(3), |c| chunks.push(c)).unwrap();
        assert_eq!(chunks.len(), 1);
        let times: Vec<i64> = chunks[0].trades.iter().map(|p| p.time.0).collect();
        assert_eq!(times, vec![t(3), t(10)]);
        assert_eq!(chunks[0].covered, vec![(t(3), t(10))]);
        assert_eq!(chunks[0].trades[0].aggressor, Some(Aggressor::Buy));
    }

    #[test]
    fn the_lookback_keeps_the_newest_stored_sessions() {
        let d = dir("lookback");
        let dates: Vec<CivilDate> = [9, 10, 11]
            .into_iter()
            .map(|day| CivilDate::new(2026, 11, day).unwrap())
            .collect();
        for &date in &dates {
            write_day(&d, date, &[]);
        }
        let open = |date| partition::trade_date_span(&d.product, date).0.0;
        let newest = UtcMillis(open(dates[2]) + 3_600_000);
        assert_eq!(lookback_start(&d, newest, 1), Some(open(dates[2])));
        assert_eq!(lookback_start(&d, newest, 2), Some(open(dates[1])));
        assert_eq!(lookback_start(&d, newest, 9), Some(open(dates[0])));
        assert_eq!(lookback_start(&d, newest, 0), None, "off");
        // The reference is the newest stored data: later dates don't count.
        let older = UtcMillis(open(dates[1]) + 1);
        assert_eq!(lookback_start(&d, older, 1), Some(open(dates[1])));
    }
}
