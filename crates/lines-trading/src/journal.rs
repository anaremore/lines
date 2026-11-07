//! The automatic trade journal (roadmap 8c, phase 1): trades built from the fill stream, with
//! exact P&L, MAE/MFE, tags and notes, statistics ([`stats`]) and an append-only file
//! ([`store`]).
//!
//! **A trade** is one round trip per account and contract, flat → position → flat, the same
//! definition the guardrails count ([`RoundTrip`]): partial fills, adds and scale-outs stay in
//! one trade; a reversal (long → short in one fill) closes one trade and opens the next, the
//! fill's quantity and fee split between them (the fee pro rata, the remainder to the closing
//! side, so nothing is lost or invented).
//!
//! - **Exact money.** Realized P&L comes from the journal's own FIFO lots ([`Position`], the
//!   core's algorithm), so it is exact and also right for fills the core learned of by
//!   reconciliation (which carry no P&L of their own). Fees are the fills' fees.
//! - **Never double-counted.** Fills are keyed by their `trade_id`; a fill seen again (a
//!   reconciled fill re-reported, a replayed stream after a restart) is ignored.
//! - **MAE/MFE** (maximum adverse/favorable excursion): the worst and best the open trade's
//!   P&L (realized so far plus open, before fees) got while it was open, from the same market
//!   batches the paper broker sees (trade and last prices) and the fills themselves; plus the
//!   price range seen, for excursions in ticks from the average entry.
//! - **Source of each fill**: the source of the command that created its order (DOM, chart,
//!   hotkey, rule, …), learned from the command results in the same stream.
//! - **Trade date**: the session's trade date of the entry (`disk::partition::trade_date`).
//! - **Open trades** live only in memory. After a restart a position the journal didn't see
//!   open is seeded from the core's position report and its trade is marked `incomplete`.
//!
//! Pure: no I/O, no clock. The service feeds it ([`JournalObserver`]); the app reads it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use lines_core::{CivilDate, ContractKey, Price, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::command::Source;
use crate::event::{Event, FillEvent, PositionView};
use crate::guardrails::RoundTrip;
use crate::market::MarketInput;
use crate::position::Position;
use crate::service::Observer;
use crate::types::{
    AccountId, ContractSpec, Money, OrderId, Side, contract_serde, price_serde, qty_serde,
    time_serde,
};

pub mod stats;
pub mod store;

/// One fill as part of a trade (a reversal's fill appears in both trades, split).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeFill {
    /// The broker's execution id (`recon-…` for reconciled fills).
    pub trade_id: String,
    #[serde(with = "time_serde")]
    pub at: UtcMillis,
    pub side: Side,
    #[serde(with = "price_serde")]
    pub price: Price,
    #[serde(with = "qty_serde")]
    pub qty: Qty,
    pub fee: Money,
    /// Where the order came from; `None` if the journal didn't see its command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reconciled: bool,
}

/// A round trip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    /// Stable: the first fill's `trade_id` (a reversal's second trade adds `/r`).
    pub id: String,
    pub account: AccountId,
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    /// The position's side: `Buy` = long.
    pub side: Side,
    #[serde(with = "time_serde")]
    pub entry_time: UtcMillis,
    #[serde(with = "time_serde")]
    pub exit_time: UtcMillis,
    #[serde(with = "date_serde")]
    pub trade_date: CivilDate,
    /// Largest open quantity.
    pub max_qty: i64,
    /// Opening fills: quantity and Σ price × quantity (exact; average = value / qty).
    pub entry_qty: i64,
    pub entry_value: i128,
    /// Closing fills, likewise.
    pub exit_qty: i64,
    pub exit_value: i128,
    /// Realized P&L before fees (exact).
    pub realized: Money,
    pub fees: Money,
    /// Worst and best P&L while open (realized + open, before fees): `mae <= 0 <= mfe`.
    pub mae: Money,
    pub mfe: Money,
    /// Price range seen while open (fills and market prices).
    #[serde(with = "price_serde")]
    pub low: Price,
    #[serde(with = "price_serde")]
    pub high: Price,
    pub fills: Vec<TradeFill>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Entry/exit chart screenshots (roadmap 8c phase 2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<PathBuf>,
    /// Opened before the journal saw it (seeded from a position after a restart): the entry
    /// side is approximate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

impl Trade {
    /// Realized P&L after fees.
    pub fn net(&self) -> Money {
        self.realized - self.fees
    }

    /// Average entry price (price units; display).
    pub fn avg_entry(&self) -> Option<f64> {
        (self.entry_qty > 0).then(|| self.entry_value as f64 / self.entry_qty as f64)
    }

    pub fn avg_exit(&self) -> Option<f64> {
        (self.exit_qty > 0).then(|| self.exit_value as f64 / self.exit_qty as f64)
    }

    fn dir(&self) -> f64 {
        self.side.sign() as f64
    }

    /// Ticks captured per contract: (average exit − average entry) in the trade's direction.
    pub fn ticks(&self, tick: i64) -> Option<f64> {
        Some((self.avg_exit()? - self.avg_entry()?) * self.dir() / tick.max(1) as f64)
    }

    /// Ticks the price went against the average entry while open (≥ 0).
    pub fn mae_ticks(&self, tick: i64) -> Option<f64> {
        let e = self.avg_entry()?;
        let worst = if self.side == Side::Buy {
            e - self.low.0 as f64
        } else {
            self.high.0 as f64 - e
        };
        Some((worst / tick.max(1) as f64).max(0.0))
    }

    /// Ticks the price went in favor of the average entry while open (≥ 0).
    pub fn mfe_ticks(&self, tick: i64) -> Option<f64> {
        let e = self.avg_entry()?;
        let best = if self.side == Side::Buy {
            self.high.0 as f64 - e
        } else {
            e - self.low.0 as f64
        };
        Some((best / tick.max(1) as f64).max(0.0))
    }

    pub fn duration_ms(&self) -> i64 {
        (self.exit_time.0 - self.entry_time.0).max(0)
    }
}

/// A user edit of a trade's tags and note (saved as its own record; the last one wins).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub note: String,
}

/// What the journal file holds after its header, one per line: `{"trade":{…}}` or
/// `{"edit":{…}}`. Externally tagged on purpose: serde's internally tagged enums buffer their
/// content, which can't hold the `i128` of `Money`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Record {
    Trade(Box<Trade>),
    Edit(Edit),
}

/// A trade in progress.
#[derive(Clone, Debug)]
struct Open {
    trade: Trade,
    pos: Position,
    spec: ContractSpec,
}

impl Open {
    /// Running P&L at `mark` (realized so far + open, before fees).
    fn running(&self, mark: Price) -> Money {
        self.trade.realized + self.spec.money(self.pos.unrealised(mark), 1)
    }

    fn saw(&mut self, p: Price) {
        let t = &mut self.trade;
        t.low = Price(t.low.0.min(p.0));
        t.high = Price(t.high.0.max(p.0));
        let r = self.running(p);
        let t = &mut self.trade;
        if r < t.mae {
            t.mae = r;
        }
        if r > t.mfe {
            t.mfe = r;
        }
    }
}

#[derive(Default)]
pub struct Journal {
    open: BTreeMap<(AccountId, ContractKey), Open>,
    closed: Vec<Trade>,
    by_id: HashMap<String, usize>,
    /// Fill ids already counted.
    seen: HashSet<String>,
    /// Each order's command source (from the command results).
    sources: HashMap<OrderId, Source>,
    specs: HashMap<ContractKey, ContractSpec>,
}

impl Journal {
    pub fn new() -> Journal {
        Journal::default()
    }

    /// Rebuild from saved records (trades and edits, in file order; the last edit wins).
    pub fn from_records(records: impl IntoIterator<Item = Record>) -> Journal {
        let mut j = Journal::new();
        for r in records {
            j.apply(r);
        }
        j
    }

    /// A saved record: a closed trade (its fills count as seen) or an edit.
    pub fn apply(&mut self, r: Record) {
        match r {
            Record::Trade(t) => self.push_closed(*t),
            Record::Edit(e) => {
                let _ = self.edit(e);
            }
        }
    }

    /// Closed trades, in the order they closed.
    pub fn trades(&self) -> &[Trade] {
        &self.closed
    }

    pub fn trade(&self, id: &str) -> Option<&Trade> {
        self.by_id.get(id).map(|&i| &self.closed[i])
    }

    /// Trades still open (oldest first by entry).
    pub fn open_trades(&self) -> impl Iterator<Item = &Trade> {
        self.open.values().map(|o| &o.trade)
    }

    /// Set a closed trade's tags and note. Tags are trimmed, empty ones dropped, duplicates
    /// removed (first kept). `None` if there is no such trade.
    pub fn edit(&mut self, mut e: Edit) -> Option<Edit> {
        let i = *self.by_id.get(&e.id)?;
        let mut seen = HashSet::new();
        e.tags = e
            .tags
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty() && seen.insert(t.clone()))
            .collect();
        let t = &mut self.closed[i];
        t.tags = e.tags.clone();
        t.note = e.note.clone();
        Some(e)
    }

    fn push_closed(&mut self, t: Trade) {
        for f in &t.fills {
            if !f.trade_id.is_empty() {
                self.seen.insert(f.trade_id.clone());
            }
        }
        match self.by_id.get(&t.id) {
            // The same trade saved twice: the later copy wins (keeps its edits' place).
            Some(&i) => self.closed[i] = t,
            None => {
                self.by_id.insert(t.id.clone(), self.closed.len());
                self.closed.push(t);
            }
        }
    }

    fn spec(&mut self, contract: ContractKey) -> Option<ContractSpec> {
        if let Some(s) = self.specs.get(&contract) {
            return Some(s.clone());
        }
        let p = lines_feed::catalog::get(contract.product)?;
        let s = ContractSpec::new(p, contract, Money::ZERO)?;
        self.specs.insert(contract, s.clone());
        Some(s)
    }

    /// Market prices seen (the batches the paper broker sees): MAE/MFE of open trades.
    pub fn on_market(&mut self, contract: ContractKey, inputs: &[MarketInput]) {
        if !self.open.keys().any(|(_, k)| *k == contract) {
            return;
        }
        for i in inputs {
            let p = match *i {
                MarketInput::Trade { price, .. } | MarketInput::Last(price) => price,
                _ => continue,
            };
            for ((_, k), o) in self.open.iter_mut() {
                if *k == contract {
                    o.saw(p);
                }
            }
        }
    }

    /// One event of the core's stream. Returns the trades it closed (to save).
    pub fn on_event(&mut self, e: &Event) -> Vec<Trade> {
        match e {
            Event::Command {
                source,
                result: Ok(orders),
                ..
            } => {
                for o in orders {
                    self.sources.entry(*o).or_insert(*source);
                }
                Vec::new()
            }
            Event::Fill(f) => self.on_fill(f),
            Event::Position(p) => {
                self.seed(p);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// A position the journal doesn't know is open (a restart): start its trade from the
    /// core's report, marked incomplete. Positions the journal tracks are left alone.
    fn seed(&mut self, p: &PositionView) {
        let key = (p.account.clone(), p.contract);
        if p.qty == 0 || self.open.contains_key(&key) {
            return;
        }
        let (Some(spec), Some(avg)) = (self.spec(p.contract), p.avg_price) else {
            return;
        };
        let price = Price(avg.round() as i64);
        let side = if p.qty > 0 { Side::Buy } else { Side::Sell };
        let mut pos = Position::default();
        pos.apply_fill(side, price, Qty(p.qty.abs()));
        let at = UtcMillis(0);
        let mut trade = new_trade(
            format!(
                "seed-{}-{}",
                p.account.0,
                p.contract.label(&spec.product).as_str()
            ),
            p.account.clone(),
            p.contract,
            side,
            at,
            self.date(&spec, at),
            price,
        );
        trade.entry_qty = p.qty.abs();
        trade.entry_value = price.0 as i128 * p.qty.abs() as i128;
        trade.max_qty = p.qty.abs();
        trade.incomplete = true;
        self.open.insert(key, Open { trade, pos, spec });
    }

    fn date(&self, spec: &ContractSpec, t: UtcMillis) -> CivilDate {
        lines_feed::disk::partition::trade_date(&spec.product, t)
    }

    fn on_fill(&mut self, f: &FillEvent) -> Vec<Trade> {
        if !f.trade_id.is_empty() && !self.seen.insert(f.trade_id.clone()) {
            return Vec::new(); // already counted
        }
        let Some(spec) = self.spec(f.contract) else {
            tracing::warn!("journal: no contract spec for a fill; skipped");
            return Vec::new();
        };
        let key = (f.account.clone(), f.contract);
        let before = self.open.get(&key).map_or(0, |o| o.pos.qty());
        let signed = f.qty.0 * f.side.sign();
        let after = before + signed;
        let fill = TradeFill {
            trade_id: f.trade_id.clone(),
            at: f.at,
            side: f.side,
            price: f.price,
            qty: f.qty,
            fee: f.fee,
            source: self.sources.get(&f.order).copied(),
            reconciled: f.reconciled,
        };
        let mut closed = Vec::new();
        match RoundTrip::of(before, after) {
            RoundTrip::None => {}
            RoundTrip::Opens => self.open_with(key, &spec, fill, ""),
            RoundTrip::Continues => {
                if let Some(o) = self.open.get_mut(&key) {
                    add_fill(o, fill);
                }
            }
            RoundTrip::Closes => {
                if let Some(mut o) = self.open.remove(&key) {
                    add_fill(&mut o, fill);
                    closed.push(o.trade);
                }
            }
            RoundTrip::Reverses => {
                // Split: the part that closes the old position, and the rest opens the next.
                let close_qty = before.abs();
                let open_qty = f.qty.0 - close_qty;
                let close_fee = Money(f.fee.0 * close_qty as i128 / f.qty.0 as i128);
                let closing = TradeFill {
                    qty: Qty(close_qty),
                    fee: close_fee,
                    ..fill.clone()
                };
                let opening = TradeFill {
                    qty: Qty(open_qty),
                    fee: f.fee - close_fee,
                    ..fill
                };
                if let Some(mut o) = self.open.remove(&key) {
                    add_fill(&mut o, closing);
                    closed.push(o.trade);
                }
                self.open_with(key, &spec, opening, "/r");
            }
        }
        for t in &closed {
            self.push_closed(t.clone());
        }
        closed
    }

    fn open_with(
        &mut self,
        key: (AccountId, ContractKey),
        spec: &ContractSpec,
        fill: TradeFill,
        suffix: &str,
    ) {
        let trade = new_trade(
            format!("{}{suffix}", fill.trade_id),
            key.0.clone(),
            key.1,
            fill.side,
            fill.at,
            self.date(spec, fill.at),
            fill.price,
        );
        let mut o = Open {
            trade,
            pos: Position::default(),
            spec: spec.clone(),
        };
        add_fill(&mut o, fill);
        self.open.insert(key, o);
    }

    /// Stats over closed trades matching `keep` (see [`stats`]).
    pub fn stats(&self, keep: impl Fn(&Trade) -> bool) -> stats::Stats {
        stats::Stats::of(self.closed.iter().filter(|t| keep(t)))
    }
}

fn new_trade(
    id: String,
    account: AccountId,
    contract: ContractKey,
    side: Side,
    at: UtcMillis,
    trade_date: CivilDate,
    price: Price,
) -> Trade {
    Trade {
        id,
        account,
        contract,
        side,
        entry_time: at,
        exit_time: at,
        trade_date,
        max_qty: 0,
        entry_qty: 0,
        entry_value: 0,
        exit_qty: 0,
        exit_value: 0,
        realized: Money::ZERO,
        fees: Money::ZERO,
        mae: Money::ZERO,
        mfe: Money::ZERO,
        low: price,
        high: price,
        fills: Vec::new(),
        tags: Vec::new(),
        note: String::new(),
        screenshots: Vec::new(),
        incomplete: false,
    }
}

/// Apply one fill (never crossing flat: reversals are split before) to an open trade.
fn add_fill(o: &mut Open, f: TradeFill) {
    let realized = o.pos.apply_fill(f.side, f.price, f.qty);
    let t = &mut o.trade;
    if f.side == t.side {
        t.entry_qty += f.qty.0;
        t.entry_value += f.price.0 as i128 * f.qty.0 as i128;
    } else {
        t.exit_qty += f.qty.0;
        t.exit_value += f.price.0 as i128 * f.qty.0 as i128;
    }
    t.realized += o.spec.money(realized, 1);
    t.fees += f.fee;
    t.exit_time = f.at;
    t.max_qty = t.max_qty.max(o.pos.qty().abs());
    let p = f.price;
    t.fills.push(f);
    o.saw(p);
}

/// Journal CSV: one row per trade (closed trades, in order), US number format, quoted where
/// needed. Prices are in price units converted with the product's display format.
pub fn csv(trades: &[Trade]) -> String {
    let mut out = String::from(
        "id,account,contract,side,entry_time,exit_time,trade_date,max_qty,avg_entry,avg_exit,realized,fees,net,ticks,duration_s,mae,mfe,mae_ticks,mfe_ticks,fills,tags,note\n",
    );
    for t in trades {
        let product = lines_feed::catalog::get(t.contract.product);
        let (label, tick) = match &product {
            Some(p) => (t.contract.label(p).as_str().to_owned(), p.price.tick),
            None => (format!("{:?}", t.contract), 1),
        };
        let price = |v: Option<f64>| match (v, &product) {
            (Some(v), Some(p)) => decimal(price_value(p, v)),
            (Some(v), None) => decimal(v),
            _ => String::new(),
        };
        let num = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_default();
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            quote(&t.id),
            quote(&t.account.0),
            quote(&label),
            if t.side == Side::Buy { "long" } else { "short" },
            t.entry_time,
            t.exit_time,
            t.trade_date,
            t.max_qty,
            price(t.avg_entry()),
            price(t.avg_exit()),
            t.realized,
            t.fees,
            t.net(),
            num(t.ticks(tick)),
            t.duration_ms() / 1000,
            t.mae,
            t.mfe,
            num(t.mae_ticks(tick)),
            num(t.mfe_ticks(tick)),
            t.fills.len(),
            quote(&t.tags.join(";")),
            quote(&t.note),
        );
    }
    out
}

/// A price in price units (possibly fractional: an average) as the product's price.
pub fn price_value(product: &lines_core::Product, units: f64) -> f64 {
    units * product.price.scale.num() as f64 / product.price.scale.den() as f64
}

/// Up to six decimals, without trailing zeros.
fn decimal(v: f64) -> String {
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A CSV field: quoted when it holds a comma, quote or line break; formula-like text is
/// prefixed so spreadsheets don't evaluate it.
fn quote(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_owned()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// The journal as the service and the UI share it: the model behind a mutex the trading
/// thread holds only briefly, a lock-free revision the UI polls, and the file writer.
pub struct JournalShared {
    journal: Mutex<Journal>,
    revision: AtomicU64,
    writer: Option<store::JournalWriter>,
}

impl JournalShared {
    pub fn new(journal: Journal, writer: Option<store::JournalWriter>) -> Arc<JournalShared> {
        Arc::new(JournalShared {
            journal: Mutex::new(journal),
            revision: AtomicU64::new(0),
            writer,
        })
    }

    /// Moves whenever a trade opens, closes, changes or is edited.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Never waits: `None` if the trading thread holds it this instant.
    pub fn try_read(&self) -> Option<MutexGuard<'_, Journal>> {
        self.journal.try_lock().ok()
    }

    pub fn read(&self) -> MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Edit a trade's tags and note; saved as a record. `false` if there is no such trade.
    pub fn edit(&self, e: Edit) -> bool {
        let saved = self.read().edit(e);
        match saved {
            Some(e) => {
                if let Some(w) = &self.writer {
                    w.append(Record::Edit(e));
                }
                self.bump();
                true
            }
            None => false,
        }
    }

    pub fn persistent(&self) -> bool {
        self.writer.is_some()
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
}

/// Feeds a [`JournalShared`] from the trading service (same thread, same order as the core).
pub struct JournalObserver(pub Arc<JournalShared>);

impl Observer for JournalObserver {
    fn market(&mut self, contract: ContractKey, inputs: &[MarketInput]) {
        let mut j = self.0.read();
        if j.open.keys().any(|(_, k)| *k == contract) {
            j.on_market(contract, inputs);
            drop(j);
            self.0.bump();
        }
    }

    fn events(&mut self, events: &[Event]) {
        let mut changed = false;
        let mut closed = Vec::new();
        {
            let mut j = self.0.read();
            for e in events {
                changed |= matches!(e, Event::Fill(_) | Event::Position(_));
                closed.extend(j.on_event(e));
            }
        }
        if let Some(w) = &self.0.writer {
            for t in closed {
                w.append(Record::Trade(Box::new(t)));
            }
        }
        if changed {
            self.0.bump();
        }
    }
}

mod date_serde {
    use lines_core::CivilDate;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(d: &CivilDate, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&d.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<CivilDate, D::Error> {
        let s = String::deserialize(d)?;
        let mut it = s.split('-').map(|x| x.parse::<u32>().ok());
        (|| CivilDate::new(it.next()?? as u16, it.next()?? as u8, it.next()?? as u8))()
            .ok_or_else(|| D::Error::custom(format!("bad date {s:?}")))
    }
}

#[cfg(test)]
mod tests;
