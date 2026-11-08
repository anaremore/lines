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
//! - **Never double-counted.** Fills are keyed by their `trade_id`; a fill seen again in this
//!   run (a reconciled fill re-reported, the same execution delivered twice) is ignored. Only
//!   within a run: broker fill ids aren't unique across runs (paper order ids restart when the
//!   paper file is reset), and the core never re-emits an earlier run's fills.
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
//! **Trade snapshots** (phase 2): a saved journal names one file per trade as it closes and
//! asks for it ([`ShotRequest`]). The name goes into [`Trade::screenshots`] and so is saved with
//! the trade (an optional field: no format change). The journal only names and asks; the app
//! draws the file on its own thread. A trade loaded from the file or seeded from a position
//! asks for nothing.
//!
//! [`Journal`] is pure: no I/O, no clock. It runs on its own thread ([`JournalShared`]): the
//! trading thread only sends it messages ([`JournalObserver`]), and the UI reads a published
//! snapshot (an `Arc` swapped in; no lock is held while formatting), so the journal never
//! shares a lock with the trading core or holds one the UI waits on.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    /// Unique in the journal: the first fill's `trade_id` (a reversal's second trade adds
    /// `/r`; a trade seeded after a restart starts `seed-`), `@` the entry time in ms, and
    /// `#n` if that still collides. Edits refer to it.
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
    /// The trade snapshot's file name in the `shots/` folder beside the journal (roadmap 8c
    /// phase 2): a name, never a path (a path would hold the OS user name). Named when asked
    /// for: the file can be missing if drawing it failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
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

    /// MAE and MFE in ticks per contract at the trade's largest size: the position's dollars over
    /// what one tick was worth at `max_qty`, so dollars = ticks × quantity × tick value on every
    /// trade. Not a price excursion: `low` and `high` include prices from before a later add, and
    /// a contract held for only part of the move would make them disagree with the dollars.
    pub fn excursion_ticks(&self, product: &lines_core::Product) -> Option<(f64, f64)> {
        let spec = ContractSpec::new(product.clone(), self.contract, Money::ZERO)?;
        let per_tick = spec.money(i128::from(spec.tick()), i128::from(self.max_qty));
        (per_tick.0 > 0).then(|| {
            (
                self.mae.0 as f64 / per_tick.0 as f64,
                self.mfe.0 as f64 / per_tick.0 as f64,
            )
        })
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

/// A trade snapshot to draw now: its file name, and the trade as it closed with its ids,
/// account, tags and note blanked (a broker's ids can hold the account; the drawing can't write
/// what it was never given). `account` is for looking up the account's kind only (the file says
/// PAPER or LIVE in words); it is never drawn.
#[derive(Clone, Debug)]
pub struct ShotRequest {
    pub file: String,
    pub trade: Trade,
    pub account: AccountId,
}

/// What the journal thread sends the thread that draws trade snapshots.
#[derive(Debug)]
pub enum ShotMsg {
    Take(Box<ShotRequest>),
    /// Reply once everything sent before is written or given up on (quitting).
    Flush(Sender<()>),
}

/// A trade snapshot's file name: the trade date and a 64-bit FNV-1a of the trade id (stable across
/// builds and platforms; the name is saved). The id itself can hold an account (a broker's
/// fill ids), so it never appears.
pub fn shot_file_name(trade_date: CivilDate, id: &str) -> String {
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{trade_date}-{hash:016x}.svg")
}

/// Whether a saved name is a plain trade snapshot file name: a damaged or edited journal line must
/// not name a file elsewhere for the app to open.
pub fn is_shot_file_name(name: &str) -> bool {
    name.strip_suffix(".svg").is_some_and(|stem| {
        !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
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

/// Order sources remembered (cleared when exceeded; a fill follows its command quickly).
const MAX_SOURCES: usize = 16_384;

#[derive(Default)]
pub struct Journal {
    open: BTreeMap<(AccountId, ContractKey), Open>,
    closed: Vec<Trade>,
    by_id: HashMap<String, usize>,
    /// Fill ids already counted in this run.
    seen: HashSet<String>,
    /// Each order's command source (from the command results).
    sources: HashMap<OrderId, Source>,
    specs: HashMap<ContractKey, ContractSpec>,
    /// Accounts whose trades take no MAE/MFE from market batches (a broker venue's accounts
    /// while the app's prices aren't that venue's market: a replay or the mock).
    unmarked: HashSet<AccountId>,
    /// Whether trades closing from now on get a trade snapshot.
    shots: bool,
    /// Screenshots asked for and not yet taken by [`Journal::take_shot_requests`].
    shot_requests: Vec<ShotRequest>,
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

    /// A saved record: a closed trade or an edit.
    pub fn apply(&mut self, r: Record) {
        match r {
            Record::Trade(t) => self.push_closed(*t),
            Record::Edit(e) => {
                let _ = self.edit(e);
            }
        }
    }

    /// Trades closing from now on get a trade snapshot (or not).
    pub fn set_shots(&mut self, on: bool) {
        self.shots = on;
    }

    /// The screenshots asked for since the last call, oldest first.
    pub fn take_shot_requests(&mut self) -> Vec<ShotRequest> {
        std::mem::take(&mut self.shot_requests)
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

    /// Keep a closed trade. Ids are unique: a colliding one (two trades saved with one id)
    /// gets `#2`, `#3`… in file order, so edits keep applying to the first.
    fn push_closed(&mut self, mut t: Trade) {
        if self.by_id.contains_key(&t.id) {
            let base = t.id.clone();
            let mut n = 2;
            while self.by_id.contains_key(&format!("{base}#{n}")) {
                n += 1;
            }
            t.id = format!("{base}#{n}");
        }
        self.by_id.insert(t.id.clone(), self.closed.len());
        self.closed.push(t);
    }

    /// A trade that just closed: its final id (`@` entry time), kept; returns the kept copy.
    fn close(&mut self, mut t: Trade) -> Trade {
        t.id = format!("{}@{}", t.id, t.entry_time.0);
        self.push_closed(t);
        let shots = self.shots;
        let kept = self.closed.last_mut().expect("just pushed");
        // A seeded trade's entry is a guess from a position report: nothing to show it by.
        if shots && !kept.incomplete {
            let file = shot_file_name(kept.trade_date, &kept.id);
            kept.screenshots = vec![file.clone()];
            let mut trade = kept.clone();
            let account = std::mem::replace(&mut trade.account, AccountId::new(""));
            trade.id.clear();
            trade.tags.clear();
            trade.note.clear();
            for f in &mut trade.fills {
                f.trade_id.clear();
            }
            self.shot_requests.push(ShotRequest {
                file,
                trade,
                account,
            });
        }
        self.closed.last().cloned().expect("just pushed")
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

    /// Trades of these accounts take no MAE/MFE from market batches from now on (added to
    /// those already set).
    pub fn add_unmarked(&mut self, accounts: impl IntoIterator<Item = AccountId>) {
        self.unmarked.extend(accounts);
    }

    /// Market prices seen (the batches the paper broker sees): MAE/MFE of open trades, except
    /// those of `unmarked` accounts.
    pub fn on_market(&mut self, contract: ContractKey, inputs: &[MarketInput]) {
        if !self.open.keys().any(|(_, k)| *k == contract) {
            return;
        }
        let unmarked = &self.unmarked;
        for i in inputs {
            let p = match *i {
                MarketInput::Trade { price, .. } | MarketInput::Last(price) => price,
                // An option has no prints here: its mid (the lower unit on a half), else its
                // last.
                MarketInput::OptionQuote { bid, ask, last, .. } => {
                    match (crate::market::quote_sides(bid, ask), last) {
                        ((Some(b), Some(a)), _) => Price((b.0 + a.0).div_euclid(2)),
                        (_, Some(l)) => l,
                        _ => continue,
                    }
                }
                _ => continue,
            };
            for ((a, k), o) in self.open.iter_mut() {
                if *k == contract && !unmarked.contains(a) {
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
                // Bounded: sources of the newest orders only (fills come soon after).
                if self.sources.len() > MAX_SOURCES {
                    self.sources.clear();
                }
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
        closed.into_iter().map(|t| self.close(t)).collect()
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
    if t.incomplete && t.fills.is_empty() {
        // Seeded after a restart (entry time unknown): it dates from the first fill seen.
        t.entry_time = f.at;
        t.trade_date = lines_feed::disk::partition::trade_date(&o.spec.product, f.at);
    }
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
/// needed. Prices are in price units converted with the product's display format. Money is the
/// whole position; `ticks`, `mae_ticks` and `mfe_ticks` are per contract, the pane's numbers
/// (`Trade::ticks`, `Trade::excursion_ticks`; the two excursions as magnitudes: ≥ 0).
pub fn csv(trades: &[Trade]) -> String {
    let mut out = String::from(
        "id,account,contract,side,entry_time,exit_time,trade_date,max_qty,avg_entry,avg_exit,realized,fees,net,ticks,duration_s,mae,mfe,mae_ticks,mfe_ticks,fills,tags,note\n",
    );
    for t in trades {
        let product = lines_feed::catalog::get(t.contract.product);
        let label = match &product {
            Some(p) => t.contract.label(p).as_str().to_owned(),
            None => format!("{:?}", t.contract),
        };
        let price = |v: Option<f64>| match (v, &product) {
            (Some(v), Some(p)) => decimal(price_value(p, v)),
            (Some(v), None) => decimal(v),
            _ => String::new(),
        };
        let num = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_default();
        let excursion = product.as_ref().and_then(|p| t.excursion_ticks(p));
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
            num(product.as_ref().and_then(|p| t.ticks(p.price.tick))),
            t.duration_ms() / 1000,
            t.mae,
            t.mfe,
            num(excursion.map(|e| e.0.abs())),
            num(excursion.map(|e| e.1)),
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

/// What the UI reads: the closed trades and the open count, as of the journal's revision.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub trades: Vec<Trade>,
    pub open: usize,
}

enum Msg {
    Market(ContractKey, Vec<MarketInput>),
    Events(Vec<Event>),
    Edit(Edit),
    /// Write the closed trades as CSV to this path.
    Export(PathBuf),
    /// Reply once everything sent before is applied and published.
    Sync(Sender<()>),
    /// Reply once everything sent before is on disk (quitting).
    Flush(Sender<()>),
    /// See [`Journal::add_unmarked`].
    Unmarked(Vec<AccountId>),
}

/// The journal on its own thread (`lines-journal`). The trading thread sends it the core's
/// market batches and events ([`JournalObserver`]); the UI sends edits and exports and reads
/// [`Snapshot`]s. The only lock is the snapshot pointer, held for a swap or a clone.
pub struct JournalShared {
    tx: Sender<Msg>,
    published: Mutex<Arc<Snapshot>>,
    revision: AtomicU64,
    persistent: bool,
}

impl JournalShared {
    /// Start the journal thread with `journal` (loaded from its file) and its writer (`None`:
    /// in memory only).
    pub fn start(journal: Journal, writer: Option<store::JournalWriter>) -> Arc<JournalShared> {
        JournalShared::start_with_shots(journal, writer, None)
    }

    /// [`JournalShared::start`], asking `shots` for a trade snapshot of each trade that
    /// closes. Only a saved journal takes them: without a writer, none are asked for.
    pub fn start_with_shots(
        mut journal: Journal,
        writer: Option<store::JournalWriter>,
        shots: Option<Sender<ShotMsg>>,
    ) -> Arc<JournalShared> {
        let shots = shots.filter(|_| writer.is_some());
        journal.set_shots(shots.is_some());
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(JournalShared {
            tx,
            published: Mutex::new(Arc::new(Snapshot::default())),
            revision: AtomicU64::new(0),
            persistent: writer.is_some(),
        });
        shared.publish(&journal);
        // Weak: when the app drops the journal (and the trading service its observer), the
        // channel closes and the thread ends, dropping (so flushing) its writer.
        let s = Arc::downgrade(&shared);
        let spawned = thread::Builder::new()
            .name("lines-journal".into())
            .spawn(move || run(journal, writer, shots, rx, s));
        if let Err(e) = spawned {
            tracing::error!("journal thread failed to start: {e}");
        }
        shared
    }

    /// The trading service's view of it.
    pub fn observer(&self) -> JournalObserver {
        JournalObserver {
            tx: self.tx.clone(),
        }
    }

    /// Moves whenever a trade opens, closes or is edited (lock-free); never on market data or
    /// position reports alone.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// The latest snapshot. Never waits: `None` if it is being swapped this instant.
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.published.try_lock().ok().map(|s| s.clone())
    }

    /// Trades of these accounts take no MAE/MFE from market batches ([`Journal::add_unmarked`]).
    pub fn add_unmarked(&self, accounts: Vec<AccountId>) {
        let _ = self.tx.send(Msg::Unmarked(accounts));
    }

    /// Edit a trade's tags and note (applied and saved on the journal thread).
    pub fn edit(&self, e: Edit) {
        let _ = self.tx.send(Msg::Edit(e));
    }

    /// Write the closed trades as CSV to `path` (on the journal thread; errors are logged).
    pub fn export(&self, path: PathBuf) {
        let _ = self.tx.send(Msg::Export(path));
    }

    /// Wait until everything sent so far is applied and published. Blocks: tests and
    /// screenshots only, never the UI's trading path.
    pub fn sync(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Msg::Sync(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }

    /// Wait (up to `timeout`) until everything sent so far is on disk, trade snapshots
    /// included. For quitting.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Msg::Flush(tx)).is_ok() && rx.recv_timeout(timeout).is_ok()
    }

    /// Saved to a file (else this run only).
    pub fn persistent(&self) -> bool {
        self.persistent
    }

    fn publish(&self, j: &Journal) {
        let snap = Arc::new(Snapshot {
            trades: j.trades().to_vec(),
            open: j.open_trades().count(),
        });
        *self.published.lock().unwrap_or_else(|e| e.into_inner()) = snap;
        self.revision.fetch_add(1, Ordering::Release);
    }
}

fn run(
    mut j: Journal,
    writer: Option<store::JournalWriter>,
    shots: Option<Sender<ShotMsg>>,
    rx: Receiver<Msg>,
    shared: std::sync::Weak<JournalShared>,
) {
    let publish = |j: &Journal| {
        if let Some(s) = shared.upgrade() {
            s.publish(j);
        }
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Market(k, inputs) => j.on_market(k, &inputs),
            Msg::Unmarked(accounts) => j.add_unmarked(accounts),
            Msg::Events(events) => {
                // Publish only when what the pane shows changed: a trade closed, or the open
                // count moved. The core reports positions on every market batch; those, and
                // fills that only grow an open trade, change nothing shown.
                let open = j.open_trades().count();
                let mut closed = false;
                for e in &events {
                    for t in j.on_event(e) {
                        closed = true;
                        if let Some(w) = &writer {
                            w.append(Record::Trade(Box::new(t)));
                        }
                    }
                }
                if closed || j.open_trades().count() != open {
                    publish(&j);
                }
                for r in j.take_shot_requests() {
                    if let Some(tx) = &shots {
                        let _ = tx.send(ShotMsg::Take(Box::new(r)));
                    }
                }
            }
            Msg::Edit(e) => {
                if let Some(e) = j.edit(e) {
                    if let Some(w) = &writer {
                        w.append(Record::Edit(e));
                    }
                    publish(&j);
                }
            }
            Msg::Export(path) => {
                let out = csv(j.trades());
                let written = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::write(&path, out));
                match written {
                    Ok(()) => tracing::info!("journal exported to {}", path.display()),
                    Err(e) => tracing::error!("journal export to {} failed: {e}", path.display()),
                }
            }
            Msg::Sync(tx) => {
                let _ = tx.send(());
            }
            Msg::Flush(tx) => {
                if let Some(w) = &writer {
                    w.flush(Duration::from_secs(5));
                }
                if let Some(s) = &shots {
                    let (done, wait) = mpsc::channel();
                    if s.send(ShotMsg::Flush(done)).is_ok() {
                        let _ = wait.recv_timeout(Duration::from_secs(5));
                    }
                }
                let _ = tx.send(());
            }
        }
    }
}

/// Feeds a [`JournalShared`] from the trading service: messages only, so the trading thread
/// never takes a lock the journal or the UI holds.
pub struct JournalObserver {
    tx: Sender<Msg>,
}

impl Observer for JournalObserver {
    /// Only trade, last and option-quote prices (all MAE/MFE uses): a depth batch costs nothing
    /// here.
    fn market(&mut self, contract: ContractKey, inputs: &[MarketInput]) {
        let prices: Vec<MarketInput> = inputs
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    MarketInput::Trade { .. }
                        | MarketInput::Last(_)
                        | MarketInput::OptionQuote { .. }
                )
            })
            .copied()
            .collect();
        if !prices.is_empty() {
            let _ = self.tx.send(Msg::Market(contract, prices));
        }
    }

    fn events(&mut self, events: &[Event]) {
        if !events.is_empty() {
            let _ = self.tx.send(Msg::Events(events.to_vec()));
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
