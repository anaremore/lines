//! Databento GLBX.MDP3 DBN files (`definition`, `trades`, `mbp-10`) → one Lines replay
//! recording (`.lfr` v3, provider `lines`, protocol `lines-events-1`) for one contract.
//!
//! - **Contracts:** instrument ids map to contracts through that UTC day's `definition` records
//!   ([`crate::defs`]); records without a definition are counted as unmapped. A definition for
//!   the target that disagrees with the catalog (tick, multiplier, currency, exchange, symbol)
//!   stops the import.
//! - **Exact:** prices convert exactly ([`crate::price`]); off-grid values are counted, never
//!   rounded. Quantities are whole contracts.
//! - **Order:** files are read in path order; records are merged by `(ts_recv, sequence)`,
//!   trades before book updates at equal keys, then file and record position. Each trade is one
//!   frame; each mbp-10 event end (`F_LAST`) is one atomic book batch ([`crate::book`]).
//!   Event times are `ts_event` (µs, floor); frame offsets follow `ts_recv` and never go back.
//! - **Trades:** from `trades` files when given, else from the mbp-10 records' own trades
//!   (action `T`). Aggressor: side `B` buy, `A` sell, `N` unknown.
//! - **Deterministic:** the output depends only on the input bytes and file names.
//!
//! Memory is bounded: one decoded record per input file.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use dbn::decode::{DbnMetadata, DecodeRecordRef, DynDecoder};
use dbn::{Mbp1Msg, Mbp10Msg, Schema, TradeMsg, VersionUpgradePolicy, rtype};
use lines_core::{ContractKey, Price, Product, Qty};
use lines_feed::catalog;
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, Header, Writer};

use crate::book::{BookDiff, BookStats};
use crate::defs::{DefOutcome, InstrumentMap};
use crate::price::PriceConv;

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// DBN files (`.dbn` or `.dbn.zst`), any of `definition`, `trades`, `mbp-10`.
    pub inputs: Vec<PathBuf>,
    pub contract: ContractKey,
    pub out: PathBuf,
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub files: usize,
    pub definitions: u64,
    pub trade_records: u64,
    pub mbp10_records: u64,
    pub mbp1_records: u64,
    /// Best bid/ask changes written (from mbp-1).
    pub quotes_written: u64,
    /// mbp-1 top-of-book prices refused (not exact or off the tick grid).
    pub quotes_rejected: u64,
    /// Market records whose instrument had no definition that day.
    pub unmapped: u64,
    /// Records of other contracts, spreads, options, products not in the catalog.
    pub other_instruments: u64,
    pub trades_written: u64,
    /// Trades refused: price not exact or off the tick grid, or size zero.
    pub trades_rejected: u64,
    pub trades_from_mbp10: bool,
    pub book: BookStats,
    /// mbp-10 records flagged `MAYBE_BAD_BOOK` by Databento.
    pub maybe_bad_book: u64,
    pub frames_written: u64,
    pub first_event_us: Option<i64>,
    pub last_event_us: Option<i64>,
}

impl fmt::Display for ImportReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.book;
        writeln!(f, "files {}  definitions {}", self.files, self.definitions)?;
        writeln!(
            f,
            "records: trades {}  mbp-10 {}  mbp-1 {}  unmapped {}  other instruments {}",
            self.trade_records,
            self.mbp10_records,
            self.mbp1_records,
            self.unmapped,
            self.other_instruments
        )?;
        if self.mbp1_records > 0 {
            writeln!(
                f,
                "top of book: changes written {}  rejected {}",
                self.quotes_written, self.quotes_rejected
            )?;
        }
        writeln!(
            f,
            "trades written {} (from {})  rejected {}",
            self.trades_written,
            if self.trades_from_mbp10 {
                "mbp-10"
            } else {
                "trades"
            },
            self.trades_rejected
        )?;
        writeln!(
            f,
            "book: batches {}  levels {}  clears {}  off-grid levels {}  duplicate levels {}  crossed batches {}  maybe-bad-book records {}",
            b.batches,
            b.levels_emitted,
            b.clears,
            b.off_grid_levels,
            b.duplicate_levels,
            b.crossed_batches,
            self.maybe_bad_book
        )?;
        write!(
            f,
            "frames {}  first event {}  last event {}",
            self.frames_written,
            fmt_us(self.first_event_us),
            fmt_us(self.last_event_us)
        )
    }
}

fn fmt_us(us: Option<i64>) -> String {
    match us {
        Some(us) => {
            let (y, mo, d, h, mi, s) = lines_core::UtcMillis(us.div_euclid(1000)).to_civil();
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        }
        None => "-".into(),
    }
}

/// Roots whose `GLBX.MDP3` raw symbols carry a two-digit year, from the year given on: NG
/// always (`NGZ26`), CL from 2030 (`CLZ6` ... `CLZ9`, then `CLF30`), where its listings run
/// more than ten years out. Every other root carries one digit (`ESZ6`, `MCLF0` for 2030).
/// From Databento's historical `definition` files for every catalog root (trade date
/// 2026-11-06, every listed contract; pulled 2026-11-09):
/// the truth, so a root that changes convention is refused by [`crate::defs::classify`],
/// never guessed.
pub const TWO_DIGIT_YEAR_FROM: &[(&str, u16)] = &[("NG", 0), ("CL", 2030)];

/// Year digits in a root's raw symbol for a contract year (1 or 2).
pub fn year_digits(root: &str, year: u16) -> u32 {
    match TWO_DIGIT_YEAR_FROM.iter().find(|(r, _)| *r == root) {
        Some((_, from)) if year >= *from => 2,
        _ => 1,
    }
}

/// The raw symbol for `root`, a month and a year (`ESZ6`, `NGZ26`, `CLF30`).
pub fn raw_symbol_for(root: &str, month: char, year: u16) -> String {
    match year_digits(root, year) {
        2 => format!("{root}{month}{:02}", year % 100),
        _ => format!("{root}{month}{}", year % 10),
    }
}

/// CME's raw symbol for a contract (`ESZ6`, `NGZ26`), as Databento names it.
pub fn raw_symbol(contract: ContractKey) -> Option<String> {
    let product = catalog::get(contract.product)?;
    let (year, month) = contract.year_month()?;
    Some(raw_symbol_for(product.symbol, month.code(), year))
}

type Dec = DynDecoder<'static, BufReader<File>>;

fn open(path: &Path) -> Result<Dec> {
    DynDecoder::from_file(path, VersionUpgradePolicy::UpgradeToV3)
        .with_context(|| format!("opening {}", path.display()))
}

#[derive(Clone)]
enum Item {
    Trade(TradeMsg),
    Mbp(Box<Mbp10Msg>),
    Top(Mbp1Msg),
}

impl Item {
    fn ts_recv(&self) -> u64 {
        match self {
            Item::Trade(t) => t.ts_recv,
            Item::Mbp(m) => m.ts_recv,
            Item::Top(m) => m.ts_recv,
        }
    }
    fn sequence(&self) -> u32 {
        match self {
            Item::Trade(t) => t.sequence,
            Item::Mbp(m) => m.sequence,
            Item::Top(m) => m.sequence,
        }
    }
    /// Trades before book updates at an equal (ts_recv, sequence).
    fn rank(&self) -> u8 {
        match self {
            Item::Trade(_) => 0,
            Item::Mbp(_) | Item::Top(_) => 1,
        }
    }
}

struct Head {
    key: (u64, u32, u8, usize, u64),
    item: Item,
}

impl PartialEq for Head {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key
    }
}
impl Eq for Head {}
impl PartialOrd for Head {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Head {
    fn cmp(&self, o: &Self) -> Ordering {
        self.key.cmp(&o.key)
    }
}

struct Source {
    dec: Dec,
    index: usize,
    read: u64,
}

impl Source {
    /// The next trade or mbp-10 record (other record types are skipped).
    fn next(&mut self, report: &mut ImportReport) -> Result<Option<Head>> {
        loop {
            let Some(rec) = self.dec.decode_record_ref()? else {
                return Ok(None);
            };
            let item = match rec.header().rtype {
                rtype::MBP_0 => match rec.get::<TradeMsg>() {
                    Some(t) => {
                        report.trade_records += 1;
                        Item::Trade(t.clone())
                    }
                    None => continue,
                },
                rtype::MBP_10 => match rec.get::<Mbp10Msg>() {
                    Some(m) => {
                        report.mbp10_records += 1;
                        Item::Mbp(Box::new(m.clone()))
                    }
                    None => continue,
                },
                rtype::MBP_1 => match rec.get::<Mbp1Msg>() {
                    Some(m) => {
                        report.mbp1_records += 1;
                        Item::Top(m.clone())
                    }
                    None => continue,
                },
                _ => continue,
            };
            self.read += 1;
            return Ok(Some(Head {
                key: (
                    item.ts_recv(),
                    item.sequence(),
                    item.rank(),
                    self.index,
                    self.read,
                ),
                item,
            }));
        }
    }
}

/// Add every definition record in `path` to `map`.
pub fn read_definitions(path: &Path, map: &mut InstrumentMap) -> Result<()> {
    let mut dec = open(path)?;
    while let Some(rec) = dec.decode_record_ref()? {
        if rec.header().rtype == rtype::INSTRUMENT_DEF
            && let Some(d) = rec.get::<dbn::InstrumentDefMsg>()
        {
            map.add(d);
        }
    }
    Ok(())
}

fn aggressor(side: std::ffi::c_char) -> Aggressor {
    match side as u8 {
        b'B' => Aggressor::Buy,
        b'A' => Aggressor::Sell,
        _ => Aggressor::Unknown,
    }
}

/// Import into `opts.out` (written to a temporary file, renamed when complete).
pub fn import(opts: &ImportOptions) -> Result<ImportReport> {
    let product: Product =
        catalog::get(opts.contract.product).context("contract's product is not in the catalog")?;
    let raw = raw_symbol(opts.contract).context("not a monthly futures contract")?;
    let tmp = opts.out.with_extension("lfr.part");
    let mut writer: Option<Writer<std::io::BufWriter<File>>> = None;
    let mut first_us = 0i64;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let report = convert(&opts.inputs, opts.contract, |recv_us, events| {
        if writer.is_none() {
            first_us = recv_us;
            let header = Header {
                start_wall_ms: recv_us.div_euclid(1000),
                provider: native::PROVIDER.into(),
                protocol: native::PROTOCOL.into(),
                product: product.symbol.into(),
                contract: Some(opts.contract),
                version: lines_feed::recording::FORMAT_VERSION,
            };
            writer = Some(
                Writer::create(&tmp, &header)
                    .with_context(|| format!("creating {}", tmp.display()))?,
            );
        }
        buf.clear();
        for e in events {
            native::encode(e, &mut buf);
        }
        let span = u64::try_from(recv_us - first_us).unwrap_or(0);
        writer.as_mut().expect("created").write(
            Direction::FromServer,
            Duration::from_micros(span),
            &Frame::binary(buf.clone()),
        )?;
        Ok(())
    })?;
    let Some(w) = writer else {
        bail!("no {raw} records in the inputs (wrong symbol, or only definitions?)");
    };
    w.finish()?;
    std::fs::rename(&tmp, &opts.out)
        .with_context(|| format!("moving {} to {}", tmp.display(), opts.out.display()))?;
    Ok(report)
}

/// Convert DBN files (`definition`, `trades`, `mbp-1`, `mbp-10`) for one contract into Lines
/// events, calling `emit(recv_us, batch)` once per frame in order (`recv_us` never goes back).
/// An error from `emit` stops the conversion and is returned. Definitions that disagree with
/// the catalog, or no definition for the contract, stop it before anything is emitted.
///
/// Top of book (`mbp-1`): at each event end, the best bid and ask that changed price, as one
/// `Bbo` event (an empty side keeps the last price: Lines' top of book has no "no bid").
pub fn convert(
    inputs: &[PathBuf],
    contract: ContractKey,
    mut emit: impl FnMut(i64, &[NativeEvent]) -> Result<()>,
) -> Result<ImportReport> {
    let product: Product =
        catalog::get(contract.product).context("contract's product is not in the catalog")?;
    let raw = raw_symbol(contract).context("not a monthly futures contract")?;
    let conv = PriceConv::new(&product);
    let mut inputs = inputs.to_vec();
    inputs.sort();
    inputs.dedup();
    let mut report = ImportReport {
        files: inputs.len(),
        ..Default::default()
    };

    // Pass 1: definitions. Files whose metadata names another schema are not read here.
    let mut map = InstrumentMap::default();
    let mut has_trades_files = false;
    let mut market_files = Vec::new();
    for path in &inputs {
        let mut dec = open(path)?;
        let schema = dec.metadata().schema;
        match schema {
            Some(Schema::Definition) | None => {
                while let Some(rec) = dec.decode_record_ref()? {
                    if rec.header().rtype == rtype::INSTRUMENT_DEF
                        && let Some(d) = rec.get::<dbn::InstrumentDefMsg>()
                    {
                        map.add(d);
                    }
                }
                if schema.is_none() {
                    market_files.push(path.clone());
                }
            }
            Some(Schema::Trades) => {
                has_trades_files = true;
                market_files.push(path.clone());
            }
            Some(Schema::Mbp10 | Schema::Mbp1) => market_files.push(path.clone()),
            Some(other) => bail!(
                "{}: schema {other} is not imported (definition, trades, mbp-1, mbp-10)",
                path.display()
            ),
        }
    }
    report.definitions = map.definitions;
    let bad = map.bad_metadata_for(&raw);
    if !bad.is_empty() {
        bail!(
            "Databento's definition disagrees with the catalog (BadMetadata): {}",
            bad.join("; ")
        );
    }
    if !map.contracts().contains_key(&contract) {
        bail!(
            "no definition for {raw} in the inputs: add the `definition` files for the same days"
        );
    }
    report.trades_from_mbp10 = !has_trades_files;

    // Pass 2: merge the market files.
    let mut heap: BinaryHeap<Reverse<Head>> = BinaryHeap::new();
    let mut sources = Vec::new();
    for (i, path) in market_files.iter().enumerate() {
        let mut s = Source {
            dec: open(path)?,
            index: i,
            read: 0,
        };
        if let Some(h) = s.next(&mut report)? {
            heap.push(Reverse(h));
        }
        sources.push(s);
    }

    let mut last_frame_us = i64::MIN;
    let mut book = BookDiff::new();
    let mut top: (Option<Price>, Option<Price>) = (None, None);
    let mut events: Vec<NativeEvent> = Vec::with_capacity(32);

    let mut out = |recv_us: i64, events: &[NativeEvent], report: &mut ImportReport| {
        for e in events {
            let t = e.time_us();
            report.first_event_us = Some(report.first_event_us.map_or(t, |f| f.min(t)));
            report.last_event_us = Some(report.last_event_us.map_or(t, |l| l.max(t)));
        }
        last_frame_us = last_frame_us.max(recv_us);
        report.frames_written += 1;
        emit(last_frame_us, events)
    };

    while let Some(Reverse(head)) = heap.pop() {
        let src = head.key.3;
        if let Some(next) = sources[src].next(&mut report)? {
            heap.push(Reverse(next));
        }
        let (ts_recv, instrument_id) = match &head.item {
            Item::Trade(t) => (t.ts_recv, t.hd.instrument_id),
            Item::Mbp(m) => (m.ts_recv, m.hd.instrument_id),
            Item::Top(m) => (m.ts_recv, m.hd.instrument_id),
        };
        match map.lookup(ts_recv, instrument_id) {
            None => {
                report.unmapped += 1;
                continue;
            }
            Some(DefOutcome::Contract(k)) if *k == contract => {}
            Some(_) => {
                report.other_instruments += 1;
                continue;
            }
        }
        let recv_us = (ts_recv / 1000) as i64;
        let trade =
            |price: i64, size: u32, side, ts_event: u64, report: &mut ImportReport| match conv
                .price(price)
            {
                Ok(p) if size > 0 => {
                    report.trades_written += 1;
                    Some(NativeEvent::Trade {
                        time_us: (ts_event / 1000) as i64,
                        price: p,
                        qty: Qty(size as i64),
                        aggressor: aggressor(side),
                    })
                }
                _ => {
                    report.trades_rejected += 1;
                    None
                }
            };
        match head.item {
            Item::Trade(t) => {
                if let Some(e) = trade(t.price, t.size, t.side, t.hd.ts_event, &mut report) {
                    out(recv_us, &[e], &mut report)?;
                }
            }
            Item::Mbp(m) => {
                if m.flags.raw() & dbn::flags::MAYBE_BAD_BOOK != 0 {
                    report.maybe_bad_book += 1;
                }
                match m.action as u8 {
                    b'R' => book.clear(),
                    b'T' if !has_trades_files => {
                        if let Some(e) = trade(m.price, m.size, m.side, m.hd.ts_event, &mut report)
                        {
                            out(recv_us, &[e], &mut report)?;
                        }
                    }
                    _ => {}
                }
                if m.flags.is_last() {
                    events.clear();
                    book.apply(
                        (m.hd.ts_event / 1000) as i64,
                        &m.levels,
                        &conv,
                        &mut report.book,
                        &mut events,
                    );
                    if !events.is_empty() {
                        out(recv_us, &events, &mut report)?;
                    }
                }
            }
            Item::Top(m) => {
                if m.flags.raw() & dbn::flags::MAYBE_BAD_BOOK != 0 {
                    report.maybe_bad_book += 1;
                }
                if m.action as u8 == b'T'
                    && !has_trades_files
                    && let Some(e) = trade(m.price, m.size, m.side, m.hd.ts_event, &mut report)
                {
                    out(recv_us, &[e], &mut report)?;
                }
                if m.flags.is_last() {
                    let l = &m.levels[0];
                    let side = |px: i64, sz: u32, report: &mut ImportReport| {
                        if px == dbn::UNDEF_PRICE || sz == 0 {
                            return None;
                        }
                        match conv.price(px) {
                            Ok(p) => Some(p),
                            Err(_) => {
                                report.quotes_rejected += 1;
                                None
                            }
                        }
                    };
                    let bid = side(l.bid_px, l.bid_sz, &mut report);
                    let ask = side(l.ask_px, l.ask_sz, &mut report);
                    let bid = bid.filter(|b| top.0 != Some(*b));
                    let ask = ask.filter(|a| top.1 != Some(*a));
                    if bid.is_some() || ask.is_some() {
                        top = (bid.or(top.0), ask.or(top.1));
                        report.quotes_written += 1;
                        out(
                            recv_us,
                            &[NativeEvent::Bbo {
                                time_us: (m.hd.ts_event / 1000) as i64,
                                bid,
                                ask,
                            }],
                            &mut report,
                        )?;
                    }
                }
            }
        }
    }
    Ok(report)
}
