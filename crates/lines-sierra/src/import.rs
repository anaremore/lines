//! Import one symbol-day of Sierra Chart ticks + depth into a `lines-events` recording.
//!
//! Streams both files (bounded memory), converts every price exactly into catalog units,
//! rebuilds the order book batch by batch to validate it, merges ticks and depth batches by
//! time, and writes one frame per tick / per depth batch so batches stay atomic on replay.
//! The output depends only on the input files, so imports are reproducible byte for byte.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lines_core::{CivilDate, OrderBook, Price, PriceScale, Product, Qty, Side, UtcMillis};
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, Header, Writer};

use crate::format::{
    DepthCommand, DepthReader, DepthRecord, ScidReader, ScidRecord, TickKind, sc_to_unix_us,
    unix_us_to_sc,
};
use crate::symbol;

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Sierra `Data` folder (contains `*.scid` and `MarketDepthData/`).
    pub data_dir: PathBuf,
    /// Sierra symbol, e.g. `NQZ5.CME`.
    pub symbol: String,
    /// UTC day, `YYYY-MM-DD`.
    pub date: String,
    pub out: PathBuf,
    /// Override Sierra's price multiplier for the root.
    pub price_multiplier: Option<f64>,
}

/// Why a price was rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PriceReject {
    NotFinite,
    /// Not within f32 precision of a whole price unit (wrong multiplier or corrupt value).
    OffGrid,
    /// A whole unit, but not a multiple of the tick.
    OffTick,
}

/// Exact `f32` (× Sierra multiplier) → catalog price units.
#[derive(Copy, Clone, Debug)]
pub struct PriceConv {
    scale: PriceScale,
    tick: i64,
    multiplier: f64,
}

impl PriceConv {
    pub fn new(instrument: &Product, multiplier: f64) -> Self {
        PriceConv {
            scale: instrument.price.scale,
            tick: instrument.price.tick,
            multiplier,
        }
    }

    pub fn convert(&self, stored: f32) -> Result<Price, PriceReject> {
        if !stored.is_finite() {
            return Err(PriceReject::NotFinite);
        }
        let units =
            stored as f64 / self.multiplier * self.scale.den() as f64 / self.scale.num() as f64;
        let r = units.round();
        // The source is an f32: allow its rounding error (relative 2^-23), nothing more.
        let tolerance = (units.abs() * f32::EPSILON as f64).max(1e-9);
        if (units - r).abs() > tolerance || r.abs() > 9.0e15 {
            return Err(PriceReject::OffGrid);
        }
        let u = r as i64;
        if u % self.tick != 0 {
            return Err(PriceReject::OffTick);
        }
        Ok(Price(u))
    }
}

/// Counts reported after an import (also the basis of the validation tests).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub instrument: String,
    /// Canonical contract label, e.g. `NQZ25`.
    pub contract: String,
    pub depth_file_found: bool,
    pub depth_version: u32,
    pub scid_version: u16,
    // Depth.
    pub depth_records: u64,
    pub depth_batches: u64,
    pub depth_unterminated_batches: u64,
    pub depth_commands: BTreeMap<String, u64>,
    pub depth_unknown_commands: u64,
    pub depth_rejected_prices: BTreeMap<String, u64>,
    /// Records whose time is earlier than the previous record's.
    pub depth_time_backwards: u64,
    pub depth_reserved_nonzero: u64,
    pub depth_records_with_orders: u64,
    pub deletes_of_missing_level: u64,
    pub modifies_of_missing_level: u64,
    pub adds_of_existing_level: u64,
    /// Complete batches after which the rebuilt book had bid >= ask.
    pub book_crossed_after_batch: u64,
    pub max_bid_levels: usize,
    pub max_ask_levels: usize,
    // Ticks.
    pub tick_records_in_day: u64,
    pub trades_written: u64,
    pub tick_rejected_prices: BTreeMap<String, u64>,
    pub tick_bar_records_skipped: u64,
    pub tick_zero_volume: u64,
    pub tick_time_backwards: u64,
    pub tick_multi_trade_records: u64,
    /// Trades by role in Sierra's trade unbundling (see `format::TickKind`).
    pub trades_standalone: u64,
    pub trades_first_sub: u64,
    pub trades_middle_sub: u64,
    pub trades_last_sub: u64,
    /// First fills of an aggressor order (standalone trades and first sub-trades) checked
    /// against the rebuilt book when they print: they must be within [bid, ask].
    pub touch_checked: u64,
    pub touch_within: u64,
    pub touch_outside: u64,
    /// Later fills of a sweep: these legitimately print through the touch before the depth
    /// update arrives, so they are only counted.
    pub sweep_fills_checked: u64,
    pub sweep_fills_through_touch: u64,
    /// Trades within the bid/ask Sierra stored on the tick record (high = ask, low = bid).
    pub trades_with_record_bbo: u64,
    pub trades_within_record_bbo: u64,
    /// Of `book_crossed_after_batch`: bid == ask (locked) rather than bid > ask.
    pub book_locked_after_batch: u64,
    /// Distance (in ticks, capped at 10) of first fills that printed outside the book.
    pub touch_outside_ticks: BTreeMap<i64, u64>,
    /// Minutes with the most outside-the-book first fills.
    pub touch_outside_top_minutes: Vec<(UtcMillis, u64)>,
    /// Depth level updates more than 5% from the mid price (stale or bad levels).
    pub far_level_updates: u64,
    pub far_level_examples: Vec<(UtcMillis, i64)>,
    /// First few crossed-after-batch occurrences (time, bid, ask) for diagnosis.
    pub crossed_examples: Vec<(UtcMillis, i64, i64)>,
    // Output.
    pub frames_written: u64,
    pub first_event: Option<UtcMillis>,
    pub last_event: Option<UtcMillis>,
    pub min_price: Option<Price>,
    pub max_price: Option<Price>,
}

impl ImportReport {
    fn reject(map: &mut BTreeMap<String, u64>, r: PriceReject) {
        *map.entry(format!("{r:?}")).or_default() += 1;
    }

    fn see_price(&mut self, p: Price) {
        self.min_price = Some(self.min_price.map_or(p, |m| m.min(p)));
        self.max_price = Some(self.max_price.map_or(p, |m| m.max(p)));
    }
}

impl fmt::Display for ImportReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "instrument            {} ({})",
            self.instrument, self.contract
        )?;
        writeln!(
            f,
            "events                {:?} .. {:?}",
            self.first_event.map(|t| t.to_string()),
            self.last_event.map(|t| t.to_string())
        )?;
        writeln!(f, "frames written        {}", self.frames_written)?;
        writeln!(
            f,
            "depth                 found={} version={} records={} batches={} unterminated={}",
            self.depth_file_found,
            self.depth_version,
            self.depth_records,
            self.depth_batches,
            self.depth_unterminated_batches
        )?;
        writeln!(
            f,
            "depth commands        {:?} unknown={}",
            self.depth_commands, self.depth_unknown_commands
        )?;
        writeln!(f, "depth rejected prices {:?}", self.depth_rejected_prices)?;
        writeln!(
            f,
            "depth time backwards  {}   reserved!=0 {}   num_orders>0 {}",
            self.depth_time_backwards, self.depth_reserved_nonzero, self.depth_records_with_orders
        )?;
        writeln!(
            f,
            "level anomalies       delete-missing={} modify-missing={} add-existing={}",
            self.deletes_of_missing_level,
            self.modifies_of_missing_level,
            self.adds_of_existing_level
        )?;
        writeln!(
            f,
            "book                  crossed-after-batch={} max-levels bid={} ask={}",
            self.book_crossed_after_batch, self.max_bid_levels, self.max_ask_levels
        )?;
        writeln!(
            f,
            "ticks                 version={} in-day={} trades={} bar-records-skipped={} zero-volume={} multi-trade={} time-backwards={}",
            self.scid_version,
            self.tick_records_in_day,
            self.trades_written,
            self.tick_bar_records_skipped,
            self.tick_zero_volume,
            self.tick_multi_trade_records,
            self.tick_time_backwards
        )?;
        writeln!(f, "tick rejected prices  {:?}", self.tick_rejected_prices)?;
        writeln!(
            f,
            "trade roles           standalone={} first-sub={} middle-sub={} last-sub={}",
            self.trades_standalone,
            self.trades_first_sub,
            self.trades_middle_sub,
            self.trades_last_sub
        )?;
        writeln!(
            f,
            "first fills vs book   checked={} within={} outside={}",
            self.touch_checked, self.touch_within, self.touch_outside
        )?;
        writeln!(
            f,
            "sweep fills vs book   checked={} through-touch={}",
            self.sweep_fills_checked, self.sweep_fills_through_touch
        )?;
        writeln!(
            f,
            "book locked (bid=ask) {} of the crossed",
            self.book_locked_after_batch
        )?;
        writeln!(
            f,
            "outside distance      {:?} (ticks, 10 = 10+)",
            self.touch_outside_ticks
        )?;
        writeln!(
            f,
            "outside top minutes   {:?}",
            self.touch_outside_top_minutes
                .iter()
                .map(|(t, n)| format!("{t} x{n}"))
                .collect::<Vec<_>>()
        )?;
        writeln!(
            f,
            "far level updates     {} e.g. {:?}",
            self.far_level_updates,
            self.far_level_examples
                .iter()
                .map(|(t, p)| format!("{t} @{p}"))
                .collect::<Vec<_>>()
        )?;
        writeln!(
            f,
            "crossed examples      {:?}",
            self.crossed_examples
                .iter()
                .map(|(t, b, a)| format!("{t} bid={b} ask={a}"))
                .collect::<Vec<_>>()
        )?;
        writeln!(
            f,
            "trades vs record bbo  checked={} within={}",
            self.trades_with_record_bbo, self.trades_within_record_bbo
        )?;
        write!(
            f,
            "price range (units)   {:?} .. {:?}",
            self.min_price.map(|p| p.0),
            self.max_price.map(|p| p.0)
        )
    }
}

pub fn depth_path(data_dir: &Path, symbol: &str, date: &str) -> PathBuf {
    data_dir
        .join("MarketDepthData")
        .join(format!("{symbol}.{date}.depth"))
}

pub fn scid_path(data_dir: &Path, symbol: &str) -> PathBuf {
    data_dir.join(format!("{symbol}.scid"))
}

/// Merge order of a trade and a depth batch (Unix µs). Ties go to the trade: the print
/// happens, then the book reflects it.
///
/// Depth records carry whole milliseconds; Sierra makes trade times unique by adding 1 µs
/// per trade within a millisecond, so the first trade of a millisecond sorts before that
/// millisecond's depth and later ones after it. The true order inside a millisecond is not
/// in the files. Merging at whole milliseconds (every trade before its millisecond's depth)
/// was measured on NQU5 2025-09-11 and is worse: 87.5% of first fills inside the book
/// against 97.8%, because later fills of a millisecond then miss the levels the earlier
/// ones uncovered. (It matches the bid/ask Sierra stores on trade records more often, 72%
/// against 61%; that bid/ask is Sierra's quote from before the millisecond, and 14% of
/// all trades print outside it.)
pub fn trade_before_batch(trade_us: i64, batch_us: i64) -> bool {
    trade_us <= batch_us
}

/// One depth batch converted to native events (time = first record, clamped monotonic).
struct Batch {
    time_us: i64,
    events: Vec<NativeEvent>,
    complete: bool,
    /// Indices of `events` that came from "add level" commands (anomaly counting only).
    adds: Vec<usize>,
}

struct Importer {
    conv: PriceConv,
    qty_decimals: u8,
    book: OrderBook,
    report: ImportReport,
    has_depth: bool,
    last_depth_rec_time: i64,
    last_depth_batch_time: i64,
    last_tick_time: i64,
    /// Inside a Sierra unbundled-trade bracket (between first and last sub-trade).
    in_bundle: bool,
    current_trade_minute: i64,
    outside_minutes: BTreeMap<i64, u64>,
}

/// Role of a trade within one aggressor order's fills.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Role {
    Standalone,
    First,
    Middle,
    Last,
}

impl Importer {
    fn qty(&self, q: u32) -> Qty {
        Qty(q as i64 * 10i64.pow(self.qty_decimals as u32))
    }

    /// Convert and apply one raw depth batch to the validation book.
    fn depth_batch(&mut self, recs: &[DepthRecord], complete: bool) -> Batch {
        let r = &mut self.report;
        r.depth_records += recs.len() as u64;
        r.depth_batches += 1;
        if !complete {
            r.depth_unterminated_batches += 1;
        }
        let mut events = Vec::with_capacity(recs.len());
        let mut adds = Vec::new();
        for rec in recs {
            if rec.time < self.last_depth_rec_time {
                self.report.depth_time_backwards += 1;
            }
            self.last_depth_rec_time = rec.time;
            if rec.reserved != 0 {
                self.report.depth_reserved_nonzero += 1;
            }
            if rec.num_orders > 0 {
                self.report.depth_records_with_orders += 1;
            }
            *self
                .report
                .depth_commands
                .entry(format!("{:?}", rec.command))
                .or_default() += 1;
            let time_us = sc_to_unix_us(rec.time);
            let (side, kind) = match rec.command {
                DepthCommand::ClearBook => {
                    events.push(NativeEvent::BookClear { time_us });
                    continue;
                }
                DepthCommand::AddBid => (Side::Bid, 0),
                DepthCommand::AddAsk => (Side::Ask, 0),
                DepthCommand::ModifyBid => (Side::Bid, 1),
                DepthCommand::ModifyAsk => (Side::Ask, 1),
                DepthCommand::DeleteBid => (Side::Bid, 2),
                DepthCommand::DeleteAsk => (Side::Ask, 2),
                DepthCommand::Unknown(_) => {
                    self.report.depth_unknown_commands += 1;
                    continue;
                }
            };
            let price = match self.conv.convert(rec.price) {
                Ok(p) => p,
                Err(e) => {
                    ImportReport::reject(&mut self.report.depth_rejected_prices, e);
                    continue;
                }
            };
            let qty = if kind == 2 {
                Qty::ZERO
            } else {
                self.qty(rec.quantity)
            };
            if kind != 2 {
                self.report.see_price(price);
            }
            if kind == 0 {
                adds.push(events.len());
            }
            events.push(NativeEvent::BookLevel {
                time_us,
                side,
                price,
                qty,
                orders: rec.num_orders as u32,
            });
        }
        let first = recs
            .first()
            .map_or(self.last_depth_batch_time, |r| sc_to_unix_us(r.time));
        let time_us = first.max(self.last_depth_batch_time);
        self.last_depth_batch_time = time_us;
        Batch {
            time_us,
            events,
            complete,
            adds,
        }
    }

    /// Apply a batch to the validation book when it is *emitted* (after every earlier trade
    /// was checked against the book), counting level anomalies and crossed books.
    fn apply_batch(&mut self, b: &Batch) {
        for (i, e) in b.events.iter().enumerate() {
            match *e {
                NativeEvent::BookClear { .. } => self.book.clear(),
                NativeEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                    time_us,
                } => {
                    if qty.0 != 0
                        && let (Some((bid, _)), Some((ask, _))) =
                            (self.book.best_bid(), self.book.best_ask())
                    {
                        let mid = (bid.0 + ask.0) / 2;
                        if (price.0 - mid).abs() * 20 > mid.abs() {
                            self.report.far_level_updates += 1;
                            if self.report.far_level_examples.len() < 5 {
                                self.report
                                    .far_level_examples
                                    .push((UtcMillis(time_us.div_euclid(1000)), price.0));
                            }
                        }
                    }
                    let exists = self.book.levels(side).any(|(p, _)| p == price);
                    let is_add = b.adds.contains(&i);
                    if qty.0 == 0 && !exists {
                        self.report.deletes_of_missing_level += 1;
                    } else if is_add && exists {
                        self.report.adds_of_existing_level += 1;
                    } else if qty.0 != 0 && !is_add && !exists {
                        self.report.modifies_of_missing_level += 1;
                    }
                    self.book.set(side, price, qty, orders);
                }
                _ => {}
            }
        }
        if b.complete && self.book.is_crossed() {
            self.report.book_crossed_after_batch += 1;
            if let (Some((bid, _)), Some((ask, _))) = (self.book.best_bid(), self.book.best_ask())
                && bid == ask
            {
                self.report.book_locked_after_batch += 1;
            }
            if self.report.crossed_examples.len() < 5
                && let (Some((bid, _)), Some((ask, _))) =
                    (self.book.best_bid(), self.book.best_ask())
            {
                self.report.crossed_examples.push((
                    UtcMillis(b.time_us.div_euclid(1000)),
                    bid.0,
                    ask.0,
                ));
            }
        }
        self.report.max_bid_levels = self.report.max_bid_levels.max(self.book.len(Side::Bid));
        self.report.max_ask_levels = self.report.max_ask_levels.max(self.book.len(Side::Ask));
    }

    /// Check a trade against the rebuilt book *at emit time* (all earlier batches applied).
    fn check_trade_against_book(&mut self, price: Price, role: Role) {
        let (Some((b, _)), Some((a, _))) = (self.book.best_bid(), self.book.best_ask()) else {
            return;
        };
        let within = b <= price && price <= a;
        match role {
            Role::Standalone | Role::First => {
                self.report.touch_checked += 1;
                if within {
                    self.report.touch_within += 1;
                } else {
                    self.report.touch_outside += 1;
                    let dist = if price < b {
                        b.0 - price.0
                    } else {
                        price.0 - a.0
                    } / self.conv.tick;
                    *self
                        .report
                        .touch_outside_ticks
                        .entry(dist.min(10))
                        .or_default() += 1;
                    *self
                        .outside_minutes
                        .entry(self.current_trade_minute)
                        .or_default() += 1;
                }
            }
            Role::Middle | Role::Last => {
                self.report.sweep_fills_checked += 1;
                if !within {
                    self.report.sweep_fills_through_touch += 1;
                }
            }
        }
    }

    /// Validate a tick record; `None` = skipped (counted).
    fn tick(&mut self, rec: &ScidRecord) -> Option<(i64, Vec<NativeEvent>, Role)> {
        let r = &mut self.report;
        r.tick_records_in_day += 1;
        if rec.time < self.last_tick_time {
            r.tick_time_backwards += 1;
        }
        self.last_tick_time = rec.time;
        let kind = rec.kind();
        // Track the unbundling bracket even for records we end up skipping.
        let role = match kind {
            TickKind::Bar => {
                r.tick_bar_records_skipped += 1;
                return None;
            }
            TickKind::FirstSubTrade => {
                self.in_bundle = true;
                Role::First
            }
            TickKind::LastSubTrade => {
                self.in_bundle = false;
                Role::Last
            }
            TickKind::Plain if self.in_bundle => Role::Middle,
            TickKind::Plain => Role::Standalone,
        };
        if rec.total_volume == 0 {
            r.tick_zero_volume += 1;
            return None;
        }
        if rec.num_trades > 1 {
            r.tick_multi_trade_records += 1;
        }
        let price = match self.conv.convert(rec.close) {
            Ok(p) => p,
            Err(e) => {
                ImportReport::reject(&mut self.report.tick_rejected_prices, e);
                return None;
            }
        };
        self.report.see_price(price);
        let time_us = sc_to_unix_us(rec.time);
        let aggressor = match (rec.ask_volume > 0, rec.bid_volume > 0) {
            (true, false) => Aggressor::Buy,
            (false, true) => Aggressor::Sell,
            _ => Aggressor::Unknown,
        };
        let mut events = vec![NativeEvent::Trade {
            time_us,
            price,
            qty: self.qty(rec.total_volume),
            aggressor,
        }];

        match role {
            Role::Standalone => self.report.trades_standalone += 1,
            Role::First => self.report.trades_first_sub += 1,
            Role::Middle => self.report.trades_middle_sub += 1,
            Role::Last => self.report.trades_last_sub += 1,
        }
        // Sierra's own bid/ask at the trade: high = ask, low = bid.
        let record_bbo = Some((
            self.conv.convert(rec.low).ok(),
            self.conv.convert(rec.high).ok(),
        ));
        if let Some((Some(b), Some(a))) = record_bbo {
            self.report.trades_with_record_bbo += 1;
            if b <= price && price <= a {
                self.report.trades_within_record_bbo += 1;
            }
        }
        if !self.has_depth
            && let Some((bid, ask)) = record_bbo
        {
            // No depth file: carry Sierra's recorded bid/ask so the replay still has a BBO.
            events.push(NativeEvent::Bbo { time_us, bid, ask });
        }
        Some((time_us, events, role))
    }
}

/// Run an import. Returns the report; the output file is complete only on `Ok`.
pub fn import(opts: &ImportOptions) -> Result<ImportReport> {
    let sym = symbol::parse(&opts.symbol)
        .with_context(|| format!("cannot parse Sierra symbol {:?}", opts.symbol))?;
    let (instrument, default_mult) = symbol::map_root(&sym.root)
        .with_context(|| format!("no catalog product for root {:?}", sym.root))?;
    let multiplier = opts.price_multiplier.unwrap_or(default_mult);
    let day_start = UtcMillis::parse_iso(&format!("{}T00:00:00", opts.date))
        .with_context(|| format!("bad --date {:?} (want YYYY-MM-DD)", opts.date))?;
    let (day_start_us, day_end_us) = (day_start.0 * 1000, (day_start.0 + 86_400_000) * 1000);

    let scid_file = scid_path(&opts.data_dir, &opts.symbol);
    let mut scid =
        ScidReader::open(&scid_file).with_context(|| format!("open {}", scid_file.display()))?;
    // Contract from the file name; a 1-digit year is resolved from the newest data in the
    // file (or the imported day, if later).
    let last_data_year = scid
        .last_time()?
        .map(|t| CivilDate::from_utc(UtcMillis(sc_to_unix_us(t).div_euclid(1000))).year)
        .unwrap_or(0)
        .max(CivilDate::from_utc(day_start).year);
    let contract = symbol::contract_for(&sym, &instrument, last_data_year).with_context(|| {
        format!(
            "cannot derive a {} contract from {:?}",
            instrument.symbol, opts.symbol
        )
    })?;
    scid.seek_to_time(unix_us_to_sc(day_start_us))?;
    let depth_file = depth_path(&opts.data_dir, &opts.symbol, &opts.date);
    let mut depth = match DepthReader::open(&depth_file) {
        Ok(d) => Some(d),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "no depth file {}; importing ticks only",
                depth_file.display()
            );
            None
        }
        Err(e) => return Err(e).with_context(|| format!("open {}", depth_file.display())),
    };

    let mut imp = Importer {
        conv: PriceConv::new(&instrument, multiplier),
        qty_decimals: instrument.qty.decimals,
        book: OrderBook::default(),
        report: ImportReport {
            instrument: instrument.symbol.to_owned(),
            contract: contract.label(&instrument).as_str().to_owned(),
            depth_file_found: depth.is_some(),
            depth_version: depth.as_ref().map_or(0, |d| d.version),
            scid_version: scid.version,
            ..Default::default()
        },
        has_depth: depth.is_some(),
        last_depth_rec_time: i64::MIN,
        last_depth_batch_time: i64::MIN,
        last_tick_time: i64::MIN,
        in_bundle: false,
        current_trade_minute: 0,
        outside_minutes: BTreeMap::new(),
    };

    let mut raw = Vec::new();
    let mut next_batch = |imp: &mut Importer| -> Result<Option<Batch>> {
        let Some(d) = depth.as_mut() else {
            return Ok(None);
        };
        Ok(d.next_batch(&mut raw)?
            .map(|complete| imp.depth_batch(&raw, complete)))
    };
    let mut next_tick = |imp: &mut Importer| -> Result<Option<(i64, Vec<NativeEvent>, Role)>> {
        while let Some(rec) = scid.next_record()? {
            if sc_to_unix_us(rec.time) >= day_end_us {
                return Ok(None);
            }
            if let Some(t) = imp.tick(&rec) {
                return Ok(Some(t));
            }
        }
        Ok(None)
    };

    let mut batch = next_batch(&mut imp)?;
    let mut tick = next_tick(&mut imp)?;
    let first_us = match (&batch, &tick) {
        (Some(b), Some((t, _, _))) => b.time_us.min(*t),
        (Some(b), None) => b.time_us,
        (None, Some((t, _, _))) => *t,
        (None, None) => bail!("no ticks or depth for {} on {}", opts.symbol, opts.date),
    };
    let header = Header {
        start_wall_ms: first_us.div_euclid(1000),
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: instrument.symbol.into(),
        contract: Some(contract),
        version: lines_feed::recording::FORMAT_VERSION,
    };
    let mut writer = Writer::create(&opts.out, &header)
        .with_context(|| format!("create {}", opts.out.display()))?;
    let mut buf = Vec::with_capacity(4096);
    let mut last_emit_us = first_us;
    let mut emit =
        |time_us: i64, events: &[NativeEvent], report: &mut ImportReport| -> Result<()> {
            buf.clear();
            for e in events {
                native::encode(e, &mut buf);
            }
            // Frame offsets must not go backwards; events keep their own exact times.
            last_emit_us = last_emit_us.max(time_us);
            let since = Duration::from_micros((last_emit_us - first_us) as u64);
            writer.write(Direction::FromServer, since, &Frame::binary(buf.clone()))?;
            report.frames_written += 1;
            let t = UtcMillis(time_us.div_euclid(1000));
            report.first_event.get_or_insert(t);
            report.last_event = Some(t);
            Ok(())
        };

    loop {
        let take_tick = match (&tick, &batch) {
            (Some((t, _, _)), Some(b)) => trade_before_batch(*t, b.time_us),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_tick {
            let (t, ev, role) = tick.take().expect("checked");
            imp.current_trade_minute = t.div_euclid(60_000_000);
            if imp.has_depth
                && let Some(NativeEvent::Trade { price, .. }) = ev.first()
            {
                imp.check_trade_against_book(*price, role);
            }
            imp.report.trades_written += 1;
            emit(t, &ev, &mut imp.report)?;
            tick = next_tick(&mut imp)?;
        } else {
            let b = batch.take().expect("checked");
            imp.apply_batch(&b);
            if !b.events.is_empty() || !b.complete {
                emit(b.time_us, &b.events, &mut imp.report)?;
            }
            batch = next_batch(&mut imp)?;
        }
    }
    writer.finish()?;
    let mut top: Vec<_> = imp
        .outside_minutes
        .iter()
        .map(|(m, n)| (UtcMillis(m * 60_000), *n))
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(5);
    imp.report.touch_outside_top_minutes = top;
    Ok(imp.report)
}
