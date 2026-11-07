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

pub struct SegmentReplay {
    files: VecDeque<PathBuf>,
    reader: Option<SegmentReader>,
    speed: f64,
    stop_at: Option<UtcMillis>,
    started: Instant,
    first_us: Option<i64>,
    pending: Option<(i64, Frame)>,
    clock: Option<UtcMillis>,
}

impl SegmentReplay {
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
        loop {
            if self.reader.is_none() {
                let Some(p) = self.files.pop_front() else {
                    return Ok(None);
                };
                self.reader = Some(
                    SegmentReader::open(&p)
                        .with_context(|| format!("open stored segment {}", p.display()))?,
                );
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
                    return Ok(Some((b.time_us, Frame::binary(b.events))));
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
        if self.speed > 0.0
            && let Some(first) = self.first_us
        {
            let since = Duration::from_micros((t - first).max(0) as u64);
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
