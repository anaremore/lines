//! Replay a stored contract (a contract directory: every trade date in order) or one stored
//! day (a `.lseg` file) like a recording (decision 0019: one tool path). Blocks are
//! `lines-events-1` batches, so the `lines` provider's session decodes them exactly as it
//! decodes imported recordings; timing follows block times, scaled by `speed`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lines_core::UtcMillis;
use tokio::time::Instant;

use super::segment::{End, SegmentHeader, SegmentReader};
use crate::provider::Frame;
use crate::recording::{FORMAT_VERSION, Header};
use crate::transport::Transport;

fn files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_dir() {
        let mut v: Vec<(lines_core::CivilDate, PathBuf)> = std::fs::read_dir(path)
            .with_context(|| format!("read {}", path.display()))?
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != super::SEGMENT_EXT {
                    return None;
                }
                Some((super::parse_date(p.file_stem()?.to_str()?)?, p))
            })
            .collect();
        v.sort();
        if v.is_empty() {
            bail!("{} holds no stored segments", path.display());
        }
        Ok(v.into_iter().map(|(_, p)| p).collect())
    } else {
        Ok(vec![path.to_path_buf()])
    }
}

/// The recording-style header of a stored contract or day: provider `lines`.
pub fn header(path: &Path) -> Result<Header> {
    let files = files(path)?;
    let mut r = SegmentReader::open(&files[0])
        .with_context(|| format!("open stored segment {}", files[0].display()))?;
    let h: SegmentHeader = r.header.clone();
    let start = match r.next_block()? {
        Ok(b) => b.time_us.div_euclid(1000),
        Err(_) => h.created_wall_ms,
    };
    Ok(Header {
        start_wall_ms: start,
        provider: crate::native::PROVIDER.into(),
        protocol: h.encoding,
        product: h.product,
        contract: h.contract,
        version: FORMAT_VERSION,
    })
}

/// Silence between one stored day's last block and the next day's first beyond which the
/// book is not carried over (µs).
const DAY_GAP_US: i64 = 60_000_000;

pub struct SegmentReplay {
    files: VecDeque<PathBuf>,
    reader: Option<SegmentReader>,
    speed: f64,
    stop_at: Option<UtcMillis>,
    started: Instant,
    first_us: Option<i64>,
    pending: Option<(i64, Frame)>,
    clock: Option<UtcMillis>,
    /// A day after the first was just opened: its first block may follow a gap.
    new_day: bool,
    /// Days opened so far.
    opened: usize,
    /// The newest block's time handed out (µs).
    last_us: Option<i64>,
    /// A new day's first block, held back while the book clear before it goes out.
    held: Option<(i64, Frame)>,
    /// The replay bar's control: it paces frames instead of `speed`.
    control: Option<std::sync::Arc<crate::replay_control::ReplayControl>>,
}

impl SegmentReplay {
    /// Paced by the replay bar's `control` (if any) instead of the fixed speed.
    pub fn controlled(
        mut self,
        control: Option<std::sync::Arc<crate::replay_control::ReplayControl>>,
    ) -> Self {
        if let (Some(c), Some(t)) = (&control, self.clock) {
            c.set_first(t);
        }
        self.control = control;
        self
    }

    pub fn open(path: &Path, speed: f64, stop_at: Option<UtcMillis>) -> Result<SegmentReplay> {
        let mut r = SegmentReplay {
            files: files(path)?.into(),
            reader: None,
            speed,
            stop_at,
            started: Instant::now(),
            first_us: None,
            pending: None,
            clock: None,
            new_day: false,
            opened: 0,
            last_us: None,
            held: None,
            control: None,
        };
        // Like a recording: before any frame, the clock reads the first stored time.
        r.pending = r.next_frame()?;
        r.clock = r
            .pending
            .as_ref()
            .map(|(t, _)| UtcMillis(t.div_euclid(1000)));
        r.first_us = r.pending.as_ref().map(|(t, _)| *t);
        Ok(r)
    }

    /// The next stored frame in order, the one `open` read ahead first (no pacing): for
    /// readers that don't go through [`Transport::recv`].
    pub(crate) fn next_unpaced(&mut self) -> Result<Option<(i64, Frame)>> {
        match self.pending.take() {
            Some(f) => Ok(Some(f)),
            None => self.next_frame(),
        }
    }

    /// The next stored block as a frame, with its time in microseconds (no pacing).
    pub(crate) fn next_frame(&mut self) -> Result<Option<(i64, Frame)>> {
        if let Some(f) = self.held.take() {
            return Ok(Some(f));
        }
        loop {
            if self.reader.is_none() {
                let Some(p) = self.files.pop_front() else {
                    return Ok(None);
                };
                self.reader = Some(
                    SegmentReader::open(&p)
                        .with_context(|| format!("open stored segment {}", p.display()))?,
                );
                self.new_day = self.opened > 0;
                self.opened += 1;
            }
            let r = self.reader.as_mut().expect("set above");
            match r.next_block()? {
                Ok(b) => {
                    if self
                        .stop_at
                        .is_some_and(|t| b.time_us.div_euclid(1000) > t.0)
                    {
                        self.files.clear();
                        self.reader = None;
                        return Ok(None);
                    }
                    let gap = self
                        .last_us
                        .is_some_and(|l| b.time_us.saturating_sub(l) > DAY_GAP_US);
                    self.last_us = Some(b.time_us);
                    let frame = (b.time_us, Frame::binary(b.events));
                    if std::mem::take(&mut self.new_day) && gap {
                        // A day after a gap (the daily halt, or one stored apart: quotes
                        // before, imported trades only after) never shows the book the day
                        // before ended with. Its own frame, so the day's first quotes apply
                        // after it; a day running on without a gap (a 24-hour market's
                        // midnight) keeps its book.
                        self.held = Some(frame);
                        let mut clear = Vec::with_capacity(9);
                        crate::native::encode(
                            &crate::native::NativeEvent::BookClear { time_us: b.time_us },
                            &mut clear,
                        );
                        return Ok(Some((b.time_us, Frame::binary(clear))));
                    }
                    return Ok(Some(frame));
                }
                Err(End::Corrupt) => {
                    tracing::warn!("stored segment damaged; continuing with the next day");
                    self.reader = None;
                }
                // The end of this day (a partial block is a writer still appending).
                Err(End::Eof | End::Partial) => self.reader = None,
            }
        }
    }
}

impl Transport for SegmentReplay {
    async fn send(&mut self, _frame: Frame) -> Result<()> {
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Frame>> {
        if self.pending.is_none() {
            self.pending = self.next_frame()?;
        }
        let Some((t, _)) = self.pending.as_ref() else {
            return Ok(None);
        };
        if let Some(c) = &self.control {
            let wall = t.div_euclid(1000);
            if c.pace(wall).await == crate::replay_control::Pace::Rewind {
                return Ok(None);
            }
        } else if self.speed > 0.0
            && let Some(first) = self.first_us
        {
            let since = Duration::from_micros(t.saturating_sub(first).max(0) as u64);
            tokio::time::sleep_until(self.started + since.div_f64(self.speed)).await;
        }
        Ok(self.pending.take().map(|(t, f)| {
            self.clock = Some(UtcMillis(t.div_euclid(1000)));
            f
        }))
    }

    fn clock(&self) -> Option<UtcMillis> {
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::segment::{FlushPolicy, SegmentHeader, SegmentWriter};
    use crate::native::{self, NativeEvent};
    use lines_core::{CivilDate, ContractKey, Month, Price, ProductId, Qty};

    fn day(dir: &Path, d: u8, events: &[NativeEvent]) {
        let date = CivilDate::new(2026, 11, d).unwrap();
        let h = SegmentHeader {
            created_wall_ms: 1,
            source: "test".into(),
            encoding: crate::native::PROTOCOL.into(),
            product: "ES".into(),
            contract: Some(ContractKey::monthly(
                ProductId(1),
                2026,
                Month::new(12).unwrap(),
            )),
            date,
        };
        let path = dir.join(format!("{date}.{}", super::super::SEGMENT_EXT));
        let (mut w, _) = SegmentWriter::open(&path, &h, FlushPolicy::default()).unwrap();
        for e in events {
            let mut b = Vec::new();
            native::encode(e, &mut b);
            w.append(e.time_us(), &b).unwrap();
        }
        w.close().unwrap();
    }

    fn decoded(f: &Frame) -> Vec<NativeEvent> {
        let mut v = Vec::new();
        native::decode(&f.data, &mut v).unwrap();
        v
    }

    /// A day after a gap starts with a book clear of its own, so a day holding only trades (an
    /// imported one) never shows the quotes its previous day ended with, and a day's first
    /// quotes still apply; a day following on without a gap keeps its book.
    #[test]
    fn each_later_day_starts_with_no_book() {
        let root = lines_core::scratch::Scratch::new("replay-new-day");
        let bbo = NativeEvent::Bbo {
            time_us: 1_000,
            bid: Some(Price(500_000)),
            ask: Some(Price(500_025)),
        };
        let trade = |time_us| NativeEvent::Trade {
            time_us,
            price: Price(500_000),
            qty: Qty(1),
            aggressor: native::Aggressor::Buy,
        };
        day(&root, 5, &[bbo, trade(2_000)]);
        let bbo2 = NativeEvent::Bbo {
            time_us: 90_000_000_000,
            bid: Some(Price(500_100)),
            ask: Some(Price(500_125)),
        };
        day(&root, 6, &[bbo2, trade(90_000_000_001)]);
        // A day following on within a minute (a 24-hour market's midnight): no clear.
        day(&root, 7, &[trade(90_000_000_002)]);
        let mut r = SegmentReplay::open(&root, 0.0, None).unwrap();
        let mut got = Vec::new();
        while let Some((_, f)) = r.next_unpaced().unwrap() {
            got.push(decoded(&f));
        }
        assert_eq!(
            got,
            vec![
                vec![bbo],
                vec![trade(2_000)],
                vec![NativeEvent::BookClear {
                    time_us: 90_000_000_000
                }],
                vec![bbo2],
                vec![trade(90_000_000_001)],
                vec![trade(90_000_000_002)],
            ]
        );
    }
}
