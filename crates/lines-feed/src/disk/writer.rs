//! One contract's writer (decision 0019): owned by the contract's feed (the registry's feed
//! thread), fed every decoded batch through [`EventSink`].
//!
//! On the feed task it only converts events to `lines-events-1` (trades and best bid/ask;
//! depth only if enabled for the product) and tracks coverage; the file work (appends, the
//! fsync policy, the bar index, retention) runs on a helper thread, so a slow disk never
//! stalls decoding (decision 0001).
//!
//! **Coverage.** A connection claims nothing until the provider says its history is complete.
//! Then *bars* are known complete from where history was requested, and *trades* from the
//! newest history snapshot's `as_of` (earlier trades arrived as bars), both up to the last
//! frame. A gap closes the claim at the gap (a provider history gap is excluded); a
//! disconnect closes it. Claims are saved every 30 s and at the end of each connection.
//!
//! **Startup.** [`ContractWriter::open`] truncates torn tails of the newest segments, then
//! hands back the cached bars (the bar index, or rebuilt from segments) for the chart store
//! to show at once; [`EventSink::begin`] asks history to start at the earliest hole in the
//! bar coverage inside the window (else at the newest bar), so the provider fills what is
//! missing and `lines_core::stitch` merges it without double counting.

use std::fs::File;
use std::io;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use lines_core::{Bar, CivilDate, ContractKey, Product, UtcMillis};

use super::partition::trade_date;
use super::segment::{FlushPolicy, SegmentHeader, SegmentWriter};
use super::{ContractDir, Coverage, StoreConfig, bars, retention};
use crate::connection::EventSink;
use crate::native::{self, NativeEvent};
use crate::provider::MarketEvent;
use crate::store::ChartStore;

/// How often bars and coverage are saved while connected.
const SAVE_EVERY: Duration = Duration::from_secs(30);
/// How often retention runs.
const RETENTION_EVERY: Duration = Duration::from_secs(600);
/// Newest segments checked for a torn tail on open (the ones a crash could have left open).
const CHECK_NEWEST: usize = 2;

enum Io {
    Block {
        date: CivilDate,
        time_us: i64,
        events: Vec<u8>,
    },
    Bars(Vec<Bar>),
    Coverage(Coverage),
}

/// Per-connection coverage state.
#[derive(Debug, Default)]
struct Run {
    /// Where bars are claimed from once history is complete.
    bars_from: i64,
    /// Where trades are claimed from (after the newest history `as_of`).
    trades_from: Option<i64>,
    as_of: Option<i64>,
    complete: bool,
    /// Newest frame time seen.
    head: Option<i64>,
    /// History gaps reported on this connection (excluded from bar claims).
    gaps: Vec<(i64, i64)>,
}

pub struct ContractWriter {
    dir: ContractDir,
    product: Product,
    depth: bool,
    has_history: bool,
    period_ms: i64,
    store: Arc<ChartStore>,
    tx: Option<mpsc::Sender<Io>>,
    io: Option<thread::JoinHandle<()>>,
    coverage: Coverage,
    run: Run,
    /// The newest trade time stored by an earlier connection (or claimed complete when the
    /// store opened). A later connection's trade at or before it is a re-delivery of one
    /// already stored (a reconnect re-sending, or a history snapshot older than the store):
    /// it is not stored again, so the segments hold every trade once. Late trades inside one
    /// connection (a REST fill of missing trades) are newer than this mark's run and kept.
    stored_mark: Option<i64>,
    /// The newest trade time stored by this connection.
    run_newest: Option<i64>,
    buf: Vec<u8>,
    last_save: Instant,
    _lock: File,
}

/// What opening found.
#[derive(Debug, Default)]
pub struct Opened {
    /// The cached bars to show before the feed connects.
    pub bars: Vec<Bar>,
    /// The bars were rebuilt from segments (the index was missing or damaged).
    pub rebuilt: bool,
    /// Bytes cut from torn segment tails.
    pub truncated_bytes: u64,
}

impl ContractWriter {
    /// Take the contract's writer lock, repair torn tails and load the cached bars.
    /// Fails with `WouldBlock` if another process is writing this contract.
    pub fn open(
        cfg: &StoreConfig,
        product: &Product,
        contract: Option<ContractKey>,
        store: Arc<ChartStore>,
    ) -> io::Result<(ContractWriter, Opened)> {
        let dir = ContractDir::new(&cfg.root, &cfg.source, product, contract);
        std::fs::create_dir_all(&dir.path)?;
        let lock = File::create(dir.lock_path())?;
        lock.try_lock().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("another Lines is writing {}", dir.path.display()),
            )
        })?;
        let (period_ms, max_bars, has_history) = {
            let d = store.read();
            (
                d.series.period_ms(),
                d.series.max_len(),
                d.capabilities.history,
            )
        };
        let mut coverage = dir.coverage();
        let mut opened = Opened::default();
        // Torn tails: only the newest segments can have been open when the app last stopped.
        let dates = dir.dates();
        for &date in dates.iter().rev().take(CHECK_NEWEST) {
            let path = dir.segment_path(date);
            let header = segment_header(&dir, product, date);
            match SegmentWriter::open(&path, &header, cfg.flush) {
                Ok((w, rep)) => {
                    w.close()?;
                    if rep.truncated_bytes > 0 {
                        tracing::warn!(
                            "{}: cut a torn tail of {} bytes after {} valid blocks",
                            path.display(),
                            rep.truncated_bytes,
                            rep.blocks
                        );
                        opened.truncated_bytes += rep.truncated_bytes;
                        // Trades after the last valid block are gone: stop claiming them.
                        let (open, close) = super::partition::trade_date_span(product, date);
                        let day_end = close.0 - 1;
                        let from = rep.last_time_us.map_or(open.0, |t| t.div_euclid(1000) + 1);
                        coverage.trades.remove(from, day_end);
                    }
                }
                Err(e) => tracing::error!("{}: {e}; not appending to it", path.display()),
            }
        }
        if opened.truncated_bytes > 0 {
            coverage.save(&dir.coverage_path())?;
        }
        match bars::load(&dir.bars_path(), period_ms) {
            Some(b) => opened.bars = b,
            None if !dates.is_empty() => {
                opened.bars = dir.rebuild_bars(period_ms, max_bars)?;
                opened.rebuilt = true;
                bars::save(&dir.bars_path(), period_ms, &opened.bars)?;
            }
            None => {}
        }

        let (tx, rx) = mpsc::channel();
        let io = {
            let (dir, product, cfg) = (dir.clone(), product.clone(), cfg.clone());
            thread::Builder::new()
                .name("lines-store".into())
                .spawn(move || io_thread(rx, dir, product, cfg, period_ms))?
        };
        Ok((
            ContractWriter {
                depth: cfg.stores_depth(product),
                product: product.clone(),
                has_history,
                period_ms,
                store,
                tx: Some(tx),
                io: Some(io),
                stored_mark: coverage.trades.ranges().last().map(|r| r.1),
                coverage,
                run: Run::default(),
                run_newest: None,
                buf: Vec::with_capacity(4096),
                last_save: Instant::now(),
                dir,
                _lock: lock,
            },
            opened,
        ))
    }

    pub fn dir(&self) -> &ContractDir {
        &self.dir
    }

    pub fn coverage(&self) -> &Coverage {
        &self.coverage
    }

    fn send(&self, msg: Io) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }

    /// Fold the current run's claims into the coverage and save it.
    fn commit(&mut self) {
        let r = &self.run;
        if let (true, Some(head)) = (r.complete, r.head) {
            let mut bars = super::Ranges::default();
            bars.add(r.bars_from, head);
            for &(a, b) in &r.gaps {
                bars.remove(a, b);
            }
            for &(a, b) in bars.ranges() {
                self.coverage.bars.add(a, b);
            }
            if let Some(t) = r.trades_from {
                self.coverage.trades.add(t, head);
                for &(a, b) in &r.gaps {
                    self.coverage.trades.remove(a, b);
                }
            }
        }
        self.send(Io::Coverage(self.coverage.clone()));
    }

    /// A connection ended: its trades are now "stored by an earlier connection".
    fn close_run_mark(&mut self) {
        if let Some(n) = self.run_newest.take() {
            self.stored_mark = Some(self.stored_mark.map_or(n, |m| m.max(n)));
        }
    }

    fn save_bars(&self) {
        let bars = self.store.read().series.bars().to_vec();
        self.send(Io::Bars(bars));
    }
}

fn segment_header(dir: &ContractDir, product: &Product, date: CivilDate) -> SegmentHeader {
    SegmentHeader {
        created_wall_ms: crate::now_utc().0,
        source: dir.source.clone(),
        encoding: super::segment::ENCODING.into(),
        product: product.symbol.into(),
        contract: dir.contract,
        date,
    }
}

impl EventSink for ContractWriter {
    fn begin(
        &mut self,
        newest_bar: Option<UtcMillis>,
        window_start: UtcMillis,
    ) -> Option<UtcMillis> {
        self.close_run_mark();
        let from = newest_bar.map(|newest| {
            // The earliest hole in the bar coverage inside the window, else the newest bar.
            let hole = self
                .coverage
                .bars
                .holes(window_start.0, newest.0)
                .first()
                .map(|&(a, _)| UtcMillis(a).floor_to(self.period_ms));
            hole.map_or(newest, |h| h.min(newest))
        });
        self.run = Run {
            bars_from: from.unwrap_or(window_start).0,
            ..Run::default()
        };
        if !self.has_history {
            // Nothing to wait for: the stream is the whole truth from its first frame.
            self.run.complete = true;
        }
        from
    }

    fn frame(&mut self, events: &[MarketEvent], now: UtcMillis) {
        let now_us = now.0.saturating_mul(1000);
        self.buf.clear();
        let mut block_us = None::<i64>;
        for e in events {
            let ne = match *e {
                MarketEvent::Trade { time, .. }
                    if self.stored_mark.is_some_and(|m| time.0 <= m) =>
                {
                    None // stored already, by an earlier connection
                }
                MarketEvent::Trade {
                    time,
                    price,
                    qty,
                    aggressor,
                } => {
                    self.run_newest = Some(self.run_newest.map_or(time.0, |n| n.max(time.0)));
                    let t = time.0.saturating_mul(1000);
                    block_us = Some(block_us.map_or(t, |b| b.max(t)));
                    Some(NativeEvent::Trade {
                        time_us: t,
                        price,
                        qty,
                        aggressor: match aggressor {
                            Some(lines_core::Aggressor::Buy) => native::Aggressor::Buy,
                            Some(lines_core::Aggressor::Sell) => native::Aggressor::Sell,
                            None => native::Aggressor::Unknown,
                        },
                    })
                }
                MarketEvent::Bid(p) => Some(NativeEvent::Bbo {
                    time_us: now_us,
                    bid: Some(p),
                    ask: None,
                }),
                MarketEvent::Ask(p) => Some(NativeEvent::Bbo {
                    time_us: now_us,
                    bid: None,
                    ask: Some(p),
                }),
                MarketEvent::BookClear if self.depth => {
                    Some(NativeEvent::BookClear { time_us: now_us })
                }
                MarketEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                } if self.depth => Some(NativeEvent::BookLevel {
                    time_us: now_us,
                    side,
                    price,
                    qty,
                    orders,
                }),
                MarketEvent::History { as_of: Some(t), .. } => {
                    self.run.as_of = Some(self.run.as_of.map_or(t.0, |a| a.max(t.0)));
                    None
                }
                MarketEvent::HistoryComplete => {
                    self.run.complete = true;
                    None
                }
                MarketEvent::HistoryGap { from, to } => {
                    self.run.gaps.push((from.0.min(to.0), from.0.max(to.0)));
                    None
                }
                MarketEvent::Gap { .. } => {
                    // Trades were dropped just before this frame: claim up to the previous
                    // frame, and start again after this one.
                    self.commit();
                    self.run.bars_from = now.0 + 1;
                    self.run.trades_from = Some(now.0 + 1);
                    self.run.head = None;
                    None
                }
                _ => None,
            };
            if let Some(ne) = ne {
                native::encode(&ne, &mut self.buf);
            }
        }
        if self.run.complete {
            if self.run.trades_from.is_none() {
                // Trades are stored from just after the newest snapshot (or from now).
                self.run.trades_from = Some(self.run.as_of.map_or(now.0, |a| a + 1));
            }
            // Complete up to the newest of the feed time and the newest trade.
            let t = block_us.map_or(now.0, |b| b.div_euclid(1000).max(now.0));
            self.run.head = Some(self.run.head.map_or(t, |h| h.max(t)));
        }
        if !self.buf.is_empty() {
            // Block time: the newest trade's exchange time, else the feed time.
            let time_us = block_us.unwrap_or(now_us);
            let date = trade_date(&self.product, UtcMillis(time_us.div_euclid(1000)));
            self.send(Io::Block {
                date,
                time_us,
                events: self.buf.clone(),
            });
        }
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.last_save = Instant::now();
            self.commit();
            self.save_bars();
        }
    }

    fn end(&mut self, _now: UtcMillis) {
        self.commit();
        self.save_bars();
        self.run = Run::default();
        self.close_run_mark();
    }
}

impl Drop for ContractWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(h) = self.io.take() {
            let _ = h.join();
        }
    }
}

fn io_thread(
    rx: mpsc::Receiver<Io>,
    dir: ContractDir,
    product: Product,
    cfg: StoreConfig,
    period_ms: i64,
) {
    let policy: FlushPolicy = cfg.flush;
    let mut current: Option<(CivilDate, std::path::PathBuf, SegmentWriter)> = None;
    let mut last_retention = Instant::now();
    retention::enforce(&cfg.root, cfg.budget_bytes);
    let close = |cur: &mut Option<(CivilDate, std::path::PathBuf, SegmentWriter)>| {
        if let Some((_, path, w)) = cur.take() {
            if let Err(e) = w.close() {
                tracing::error!("{}: {e}", path.display());
            }
            retention::mark_active(&path, false);
        }
    };
    loop {
        match rx.recv_timeout(policy.write_every) {
            Ok(Io::Block {
                date,
                time_us,
                events,
            }) => {
                if current.as_ref().is_none_or(|(d, _, _)| *d != date) {
                    close(&mut current);
                    let path = dir.segment_path(date);
                    retention::mark_active(&path, true);
                    match SegmentWriter::open(&path, &segment_header(&dir, &product, date), policy)
                    {
                        Ok((w, rep)) => {
                            if rep.truncated_bytes > 0 {
                                tracing::warn!(
                                    "{}: cut a torn tail of {} bytes",
                                    path.display(),
                                    rep.truncated_bytes
                                );
                            }
                            current = Some((date, path, w));
                        }
                        Err(e) => {
                            retention::mark_active(&path, false);
                            tracing::error!(
                                "{}: {e}; data for {date} is not stored",
                                path.display()
                            );
                        }
                    }
                }
                if let Some((_, path, w)) = current.as_mut()
                    && let Err(e) = w.append(time_us, &events)
                {
                    tracing::error!("{}: {e}", path.display());
                }
            }
            Ok(Io::Bars(b)) => {
                if let Err(e) = bars::save(&dir.bars_path(), period_ms, &b) {
                    tracing::error!("{}: {e}", dir.bars_path().display());
                }
            }
            Ok(Io::Coverage(c)) => {
                if let Err(e) = c.save(&dir.coverage_path()) {
                    tracing::error!("{}: {e}", dir.coverage_path().display());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some((_, path, w)) = current.as_mut()
            && let Err(e) = w.tick()
        {
            tracing::error!("{}: {e}", path.display());
        }
        if last_retention.elapsed() >= RETENTION_EVERY {
            last_retention = Instant::now();
            retention::enforce(&cfg.root, cfg.budget_bytes);
        }
    }
    close(&mut current);
}
