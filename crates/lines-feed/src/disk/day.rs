//! Writing one whole past trade date into the store (decision 0030 §8, "Download days").
//!
//! A downloaded day becomes a normal stored day: a segment (`<date>.lseg`), its 1-minute bars
//! merged into the bar index, and its trade date's span claimed in the coverage (trades and
//! bars), so it replays and warm-starts like a day the live writer stored.
//!
//! - **All or nothing.** Blocks go to `<date>.lseg.part`; only [`DayWriter::commit`] renames
//!   it into place and then claims the coverage. Dropping the writer without committing (a
//!   cancel, a provider error, a crash) deletes the part file and claims nothing; a part file
//!   left by a crash is ignored by every reader (wrong extension) and replaced by the next try.
//! - **Never twice.** A date that already has a segment (stored live or downloaded earlier,
//!   complete or not) is refused before anything is asked for ([`day_state`]): a segment is
//!   never appended to or replaced by a download.
//! - **One writer per contract.** The writer holds the contract's `writer.lock` for the whole
//!   day, exactly like a live feed's writer. If a live feed owns the contract the day is
//!   refused (`Busy`), because the live writer keeps the coverage and bar index in memory and
//!   would overwrite what a download added (the estimate shows it as busy up front). A live
//!   feed that starts while a day is being downloaded finds the lock taken and tries again
//!   every 10 s of feed time (`StoreSink` in the feed); once the download ends it stores
//!   that connection's trade segments only (never the bar index or coverage), and a full
//!   writer from its next connection.
//! - **Finished trade dates only.** A trade date whose session hasn't closed is refused
//!   (`NotFinished`): the live writer may still write it.
//! - Events outside the trade date's span are not stored (counted in [`Written::outside`]).
//! - **Pinned.** A downloaded day is listed in the contract's `downloaded` file and retention
//!   never evicts it (a paid day is fetched once, decision 0030 §8); it still counts toward
//!   the store's size.

use std::fs::File;
use std::io;
use std::path::PathBuf;

use lines_core::{BarSeries, CivilDate, ContractKey, Product, Stitcher, UtcMillis};

use super::partition::trade_date_span;
use super::segment::{ENCODING, FlushPolicy, SegmentHeader, SegmentWriter};
use super::{ContractDir, bars};
use crate::native::{self, NativeEvent};

/// How long a day writer waits before its second try of a held lock.
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// The bar index's period (the chart store's: 1 minute).
const BAR_PERIOD_MS: i64 = UtcMillis::MINUTE;

/// What the store already holds for a trade date.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DayState {
    /// No segment: the day can be downloaded (if finished).
    Missing,
    /// A segment exists. `complete`: coverage claims the whole trade date.
    Stored { complete: bool },
    /// The product has no session on that date (a weekend or a closed holiday).
    NoSession,
    /// The session hasn't closed yet (at `now`).
    NotFinished,
    /// Another writer (a live feed) holds the contract now ([`crate::download::plan`] only;
    /// [`day_state`] never answers it).
    Busy,
}

/// Why a day can't be written.
#[derive(Debug)]
pub enum BeginError {
    /// The day is already stored, has no session, or isn't over.
    Not(DayState),
    /// Another writer (a live feed, here or in another Lines) holds this contract.
    Busy,
    Io(io::Error),
}

impl std::fmt::Display for BeginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BeginError::Not(DayState::Stored { .. }) => f.write_str("already stored"),
            BeginError::Not(DayState::NoSession) => f.write_str("no session on this date"),
            BeginError::Not(DayState::NotFinished) => f.write_str("the session isn't over"),
            BeginError::Not(DayState::Missing) => f.write_str("missing"),
            BeginError::Not(DayState::Busy) => {
                f.write_str("this contract is being recorded or downloaded")
            }
            BeginError::Busy => f.write_str("a live feed is storing this contract"),
            BeginError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for BeginError {
    fn from(e: io::Error) -> Self {
        BeginError::Io(e)
    }
}

/// The trade date's session span `[open, close)`, or `None` if it has no session.
pub fn session_span(product: &Product, date: CivilDate) -> Option<(UtcMillis, UtcMillis)> {
    match crate::catalog::sessions(product.id) {
        Some(cal) => cal
            .session(date, lines_core::sessions::SessionKind::Eth)
            .map(|s| (s.open, s.close)),
        None => Some(trade_date_span(product, date)),
    }
}

/// What the store holds for `date` (read-only; no lock).
pub fn day_state(dir: &ContractDir, date: CivilDate, now: UtcMillis) -> DayState {
    let Some((open, close)) = session_span(&dir.product, date) else {
        return DayState::NoSession;
    };
    if dir.segment_path(date).exists() {
        let cov = dir.coverage().trades;
        let complete = cov.holes(open.0, close.0 - 1).is_empty();
        return DayState::Stored { complete };
    }
    if close > now {
        return DayState::NotFinished;
    }
    DayState::Missing
}

/// What a committed day holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    pub bytes: u64,
    pub blocks: u64,
    pub trades: u64,
    /// Events outside the trade date's span (not stored).
    pub outside: u64,
}

/// One trade date being written (see the module docs).
pub struct DayWriter {
    dir: ContractDir,
    date: CivilDate,
    span: (UtcMillis, UtcMillis),
    part: PathBuf,
    writer: Option<SegmentWriter>,
    series: BarSeries,
    stitch: Stitcher,
    buf: Vec<u8>,
    written: Written,
    committed: bool,
    /// Tests: make one commit step fail ("coverage" or "rename").
    #[cfg(test)]
    fail: Option<&'static str>,
    _lock: File,
}

impl DayWriter {
    /// Take the contract's writer lock and start `date`'s part file. Refuses a day that is
    /// stored, has no session or isn't over, and a contract a live feed is writing.
    pub fn begin(
        root: &std::path::Path,
        source: &str,
        product: &Product,
        contract: Option<ContractKey>,
        date: CivilDate,
        now: UtcMillis,
    ) -> Result<DayWriter, BeginError> {
        Self::begin_with(root, source, product, contract, date, now, &mut || {
            std::thread::sleep(LOCK_RETRY)
        })
    }

    /// [`DayWriter::begin`], with `wait` run between the two tries of the lock (tests free it
    /// there instead of sleeping).
    pub fn begin_with(
        root: &std::path::Path,
        source: &str,
        product: &Product,
        contract: Option<ContractKey>,
        date: CivilDate,
        now: UtcMillis,
        wait: &mut dyn FnMut(),
    ) -> Result<DayWriter, BeginError> {
        let dir = ContractDir::new(root, source, product, contract);
        match day_state(&dir, date, now) {
            DayState::Missing => {}
            other => return Err(BeginError::Not(other)),
        }
        std::fs::create_dir_all(&dir.path)?;
        let lock = File::create(dir.lock_path())?;
        // Once more after a moment: an estimate's lock probe (`download::plan`) holds it for
        // an instant.
        if lock.try_lock().is_err() {
            wait();
            if lock.try_lock().is_err() {
                return Err(BeginError::Busy);
            }
        }
        // Checked again under the lock: a writer that just finished may have stored it.
        match day_state(&dir, date, now) {
            DayState::Missing => {}
            other => return Err(BeginError::Not(other)),
        }
        let span = session_span(product, date).expect("checked by day_state");
        let part = dir.path.join(format!("{date}.{}.part", super::SEGMENT_EXT));
        let _ = std::fs::remove_file(&part); // a crashed try's leftovers
        let header = SegmentHeader {
            created_wall_ms: crate::now_utc().0,
            source: dir.source.clone(),
            encoding: ENCODING.into(),
            product: product.symbol.into(),
            contract,
            date,
        };
        let policy = FlushPolicy {
            max_buffer: 1 << 20,
            ..FlushPolicy::default()
        };
        let (writer, _) = SegmentWriter::open(&part, &header, policy)?;
        Ok(DayWriter {
            dir,
            date,
            span,
            written: Written {
                path: part.clone(),
                ..Written::default()
            },
            part,
            writer: Some(writer),
            series: BarSeries::new(BAR_PERIOD_MS, 4 * 1440),
            stitch: Stitcher::new(BAR_PERIOD_MS, 0),
            buf: Vec::with_capacity(4096),
            committed: false,
            #[cfg(test)]
            fail: None,
            _lock: lock,
        })
    }

    pub fn date(&self) -> CivilDate {
        self.date
    }

    /// The trade date's span `[open, close)`.
    pub fn span(&self) -> (UtcMillis, UtcMillis) {
        self.span
    }

    /// The contract's directory (scratch files may go in it, never with a segment's name).
    pub fn dir(&self) -> &ContractDir {
        &self.dir
    }

    /// Append one atomic batch, stamped `time_us`. Events outside the trade date are dropped
    /// and counted; an empty batch writes nothing.
    pub fn block(&mut self, time_us: i64, events: &[NativeEvent]) -> io::Result<()> {
        let (open_us, close_us) = (self.span.0.0 * 1000, self.span.1.0 * 1000);
        self.buf.clear();
        for e in events {
            let t = e.time_us();
            if t < open_us || t >= close_us {
                self.written.outside += 1;
                continue;
            }
            if let NativeEvent::Trade {
                time_us,
                price,
                qty,
                ..
            } = *e
            {
                self.written.trades += 1;
                self.stitch.trade(
                    &mut self.series,
                    UtcMillis(time_us.div_euclid(1000)),
                    price,
                    qty,
                );
            }
            native::encode(e, &mut self.buf);
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        let block_us = time_us.clamp(open_us, close_us - 1);
        let w = self.writer.as_mut().expect("open until commit");
        w.append(block_us, &self.buf)?;
        w.tick()?;
        self.written.blocks += 1;
        Ok(())
    }

    /// Make the day a stored day, in an order whose every crash window is harmless:
    /// 1. pin it (retention never evicts a downloaded day; a pin without a segment means
    ///    nothing, a stored day is one with a segment);
    /// 2. claim its session's *trades* (read only together with the segments that exist,
    ///    `ContractDir::trade_coverage`, so a claim without a segment counts nothing);
    /// 3. rename the part file into place: now it is stored;
    /// 4. merge its bars into the bar index and claim its *bars* (a bars claim is only made
    ///    once the index holds them; if the index can't be saved it is removed and rebuilt from
    ///    the segments at the next start, and no bars claim is made).
    ///
    /// A failure in 1-3 takes back what was done (pin, trades claim) and stores nothing.
    pub fn commit(mut self) -> io::Result<Written> {
        let w = self.writer.take().expect("open until commit");
        w.close()?;
        if self.written.blocks == 0 || (self.written.trades == 0 && self.written.outside > 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "No market data in this session.",
            ));
        }
        let path = self.dir.segment_path(self.date);
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} appeared while downloading", path.display()),
            ));
        }
        // The size on disk, read before anything can fail on the way to a stored day.
        let bytes = std::fs::metadata(&self.part).map_or(0, |m| m.len());
        let pinned_before = pinned(&self.dir.path).contains(&self.date);
        pin(&self.dir, self.date)?;
        let undo_pin = |dir: &ContractDir, date| {
            if !pinned_before {
                let _ = unpin(dir, date);
            }
        };
        let (open, close) = self.span;
        let cov_before = self.dir.coverage();
        let mut cov = cov_before.clone();
        cov.trades.add(open.0, close.0 - 1);
        if let Err(e) = self
            .step("coverage")
            .and_then(|()| cov.save(&self.dir.coverage_path()))
        {
            undo_pin(&self.dir, self.date);
            return Err(e);
        }
        if let Err(e) = self
            .step("rename")
            .and_then(|()| std::fs::rename(&self.part, &path))
        {
            let _ = cov_before.save(&self.dir.coverage_path());
            undo_pin(&self.dir, self.date);
            return Err(e);
        }
        self.committed = true;
        let day_bars = self.series.bars().to_vec();
        let merged = match bars::load(&self.dir.bars_path(), BAR_PERIOD_MS) {
            Some(mut all) => {
                all.retain(|b| b.time < open || b.time >= close);
                all.extend(day_bars);
                all.sort_by_key(|b| b.time);
                match bars::save(&self.dir.bars_path(), BAR_PERIOD_MS, &all) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(
                            "{}: {e}; rebuilt at the next start",
                            self.dir.bars_path().display()
                        );
                        let _ = std::fs::remove_file(self.dir.bars_path());
                        false
                    }
                }
            }
            // No index: the next start rebuilds it from the segments (claiming bars where the
            // trades are).
            None => false,
        };
        if merged {
            cov.bars.add(open.0, close.0 - 1);
            if let Err(e) = cov.save(&self.dir.coverage_path()) {
                tracing::warn!(
                    "{}: {e}; the day's bars aren't claimed",
                    self.dir.path.display()
                );
            }
        }
        self.written.bytes = bytes;
        self.written.path = path;
        Ok(std::mem::take(&mut self.written))
    }

    /// Tests force a step to fail; otherwise nothing.
    fn step(&self, name: &str) -> io::Result<()> {
        #[cfg(test)]
        if self.fail == Some(name) {
            return Err(io::Error::other(format!("{name} failed (test)")));
        }
        let _ = name;
        Ok(())
    }
}

/// The file listing a contract's downloaded trade dates (one `YYYY-MM-DD` per line).
pub const PINNED: &str = "downloaded";

/// Trade dates downloaded into `dir` (never evicted by retention).
pub fn pinned(dir: &std::path::Path) -> Vec<CivilDate> {
    std::fs::read_to_string(dir.join(PINNED))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| super::parse_date(l.trim()))
        .collect()
}

fn unpin(dir: &ContractDir, date: CivilDate) -> io::Result<()> {
    let days: Vec<CivilDate> = pinned(&dir.path)
        .into_iter()
        .filter(|d| *d != date)
        .collect();
    let text: String = days.iter().map(|d| format!("{d}\n")).collect();
    super::write_atomic(&dir.path.join(PINNED), text.as_bytes())
}

fn pin(dir: &ContractDir, date: CivilDate) -> io::Result<()> {
    let mut days = pinned(&dir.path);
    if days.contains(&date) {
        return Ok(());
    }
    days.push(date);
    days.sort();
    let text: String = days.iter().map(|d| format!("{d}\n")).collect();
    super::write_atomic(&dir.path.join(PINNED), text.as_bytes())
}

impl Drop for DayWriter {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.writer.take());
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use lines_core::{Month, Price, Qty};

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lines-day-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn es() -> (Product, Option<ContractKey>) {
        let es = catalog::get(catalog::ES).unwrap();
        let k = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        (es, Some(k))
    }

    #[test]
    fn states_and_spans() {
        let r = root("states");
        let (es, k) = es();
        let dir = ContractDir::new(&r, "test", &es, k);
        let thu = CivilDate::new(2026, 11, 5).unwrap();
        let sat = CivilDate::new(2026, 11, 7).unwrap();
        let after = UtcMillis::parse_iso("2026-11-06T00:00:00Z").unwrap();
        let during = UtcMillis::parse_iso("2026-11-05T15:00:00Z").unwrap();
        assert_eq!(day_state(&dir, thu, after), DayState::Missing);
        assert_eq!(day_state(&dir, thu, during), DayState::NotFinished);
        assert_eq!(day_state(&dir, sat, after), DayState::NoSession);
        let mut w = DayWriter::begin(&r, "test", &es, k, thu, after).unwrap();
        let (open, _) = w.span();
        let t = open.0 * 1000 + 5_000_000;
        w.block(
            t,
            &[NativeEvent::Trade {
                time_us: t,
                price: Price(600_000),
                qty: Qty(2),
                aggressor: native::Aggressor::Buy,
            }],
        )
        .unwrap();
        let written = w.commit().unwrap();
        assert_eq!(written.trades, 1);
        assert_eq!(
            written.bytes,
            std::fs::metadata(&written.path).unwrap().len()
        );
        assert_eq!(pinned(&dir.path), vec![thu]);
        assert_eq!(
            day_state(&dir, thu, after),
            DayState::Stored { complete: true }
        );
        assert!(matches!(
            DayWriter::begin(&r, "test", &es, k, thu, after),
            Err(BeginError::Not(DayState::Stored { .. }))
        ));
        let _ = std::fs::remove_dir_all(&r);
    }

    /// A commit whose coverage save or rename fails stores nothing: no segment, no pin, no
    /// claim; the part file goes with the writer.
    #[test]
    fn a_failed_commit_takes_everything_back() {
        for step in ["coverage", "rename"] {
            let r = root(&format!("fail-{step}"));
            let (es, k) = es();
            let dir = ContractDir::new(&r, "test", &es, k);
            let thu = CivilDate::new(2026, 11, 5).unwrap();
            let after = UtcMillis::parse_iso("2026-11-06T00:00:00Z").unwrap();
            let mut w = DayWriter::begin(&r, "test", &es, k, thu, after).unwrap();
            let t = w.span().0.0 * 1000 + 5_000_000;
            w.block(
                t,
                &[NativeEvent::Trade {
                    time_us: t,
                    price: Price(600_000),
                    qty: Qty(2),
                    aggressor: native::Aggressor::Buy,
                }],
            )
            .unwrap();
            w.fail = Some(step);
            let part = w.part.clone();
            assert!(w.commit().is_err(), "{step}");
            assert!(!dir.segment_path(thu).exists(), "{step}");
            assert!(!part.exists(), "{step}: the part file goes");
            assert!(pinned(&dir.path).is_empty(), "{step}: unpinned");
            assert!(dir.coverage().trades.is_empty(), "{step}: no claim");
            assert_eq!(day_state(&dir, thu, after), DayState::Missing, "{step}");
            let _ = std::fs::remove_dir_all(&r);
        }
    }

    /// A lock held for an instant (the estimate's probe) doesn't refuse the day: the second
    /// try, after `wait`, takes it; a lock still held then is Busy.
    #[test]
    fn a_briefly_held_lock_is_tried_again() {
        let r = root("retry");
        let (es, k) = es();
        let dir = ContractDir::new(&r, "test", &es, k);
        std::fs::create_dir_all(&dir.path).unwrap();
        let thu = CivilDate::new(2026, 11, 5).unwrap();
        let after = UtcMillis::parse_iso("2026-11-06T00:00:00Z").unwrap();
        let probe = File::create(dir.lock_path()).unwrap();
        probe.try_lock().unwrap();
        let mut held = Some(probe);
        let w = DayWriter::begin_with(&r, "test", &es, k, thu, after, &mut || {
            held.take();
        });
        assert!(w.is_ok(), "taken at the second try");
        drop(w);
        let probe = File::create(dir.lock_path()).unwrap();
        probe.try_lock().unwrap();
        assert!(matches!(
            DayWriter::begin_with(&r, "test", &es, k, thu, after, &mut || {}),
            Err(BeginError::Busy)
        ));
        drop(probe);
        let _ = std::fs::remove_dir_all(&r);
    }
}
