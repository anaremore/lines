//! The "Download days…" dialog's model (decision 0030 §8), pure (no GPUI): the window
//! (`download_view`) renders it and feeds keys, clicks and the download job's updates back.
//!
//! Flow: the person picks a source (when more than one can download), a symbol (a contract,
//! or a product meaning its front contract on the first date), a range of trade dates and the
//! contents; **Check cost** lists every day as already stored, no session, not over yet,
//! included, an estimated price, or the provider's refusal in its own words, with the total
//! and size; **Download** fetches only the days that were priced, at most at the price shown;
//! progress is per day and Esc or Cancel stops it (the day in flight leaves nothing behind).

use lines_core::{CivilDate, ContractKey, Delivery, Product, UtcMillis};
use lines_feed::catalog::{self, Target};
use lines_feed::disk::day::DayState;
use lines_feed::download::{
    self, Charge, Contents, DayError, DayEstimate, Outcome, Update, size_text, usd_text,
};

/// `1 day`, `2 days`.
fn days(n: usize) -> String {
    if n == 1 {
        "1 day".into()
    } else {
        format!("{n} days")
    }
}

/// The dialog's inputs, in focus order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Source,
    Symbol,
    From,
    To,
    Contents,
}

impl Field {
    pub const ALL: [Field; 5] = [
        Field::Source,
        Field::Symbol,
        Field::From,
        Field::To,
        Field::Contents,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Source => "Source",
            Field::Symbol => "Symbol",
            Field::From => "From",
            Field::To => "To",
            Field::Contents => "Contents",
        }
    }

    /// Typed text (the others are choices stepped with ← →).
    pub fn is_text(self) -> bool {
        matches!(self, Field::Symbol | Field::From | Field::To)
    }
}

/// Where the dialog is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Editing,
    Estimating,
    Estimated,
    Downloading,
    /// The download ended (every day reported).
    Finished,
}

/// One day's line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not asked about: the store's answer (stored, no session, not over).
    Store(DayState),
    /// Waiting for its estimate.
    Asking,
    Priced,
    /// The provider's own words.
    Refused(String),
    /// Waiting its turn in a download.
    Queued,
    Receiving {
        received: u64,
    },
    /// Downloaded and stored in this session.
    Downloaded {
        bytes: u64,
    },
    /// An estimate canceled before this day was asked about.
    NotAsked,
    /// The cost couldn't be checked on this side (no key, the network): not the provider's
    /// answer.
    CheckFailed(String),
    Busy,
    PriceChanged(Charge),
    Canceled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub date: CivilDate,
    pub status: Status,
    /// Its data was asked for (a paid day may be billed from then on, whatever follows).
    pub requested: bool,
    /// Its estimate, once priced (kept through the download).
    pub est: Option<DayEstimate>,
}

/// How a status reads: the words, whether it is a problem (shown in the warning color, with
/// the words carrying it), and the size column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowText {
    pub day: String,
    pub status: String,
    /// What it costs (`$1.63`, `$0.00`), right-aligned in its own column; empty when it isn't
    /// to be fetched.
    pub cost: String,
    pub size: String,
    pub problem: bool,
    /// Done or nothing to do (dimmed).
    pub quiet: bool,
}

/// A parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub product: Product,
    pub contract: ContractKey,
    pub dates: Vec<CivilDate>,
}

/// One source that can download, as the dialog lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRow {
    /// The `--source` name (the store's directory).
    pub name: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub sources: Vec<SourceRow>,
    pub source: usize,
    pub symbol: String,
    pub from: String,
    pub to: String,
    /// The current source's choices for the symbol's product.
    pub contents: Vec<Contents>,
    pub content: usize,
    pub focus: Field,
    pub phase: Phase,
    pub rows: Vec<Row>,
    /// The resolved contract's label once checked (`ESZ26`).
    pub contract_label: Option<String>,
    /// What's wrong with the inputs, or why nothing can be downloaded here.
    pub error: Option<String>,
    /// Cancel was pressed while a job ran (a download: "Cancel remaining", the day in flight
    /// finishes).
    pub canceling: bool,
    /// "Stop now" was pressed during a download: the day in flight is discarded too.
    pub stopping: bool,
    /// When (ms) "Cancel remaining" was pressed: "Stop now" takes a press at least
    /// [`STOP_AFTER_MS`] later, so a double press or a held Esc never discards a paid day.
    pub canceled_at: Option<u64>,
    /// The store's size budget, and what it held when the estimate looked (the budget note).
    pub budget: Option<u64>,
    pub store_bytes: Option<u64>,
    /// When (ms, the window's clock) Download became pressable: a press must come at least
    /// [`SETTLE_MS`] later, so the press (or a held or double press) that asked for the cost
    /// never also starts the download.
    pub ready_at: Option<u64>,
    /// Days the running or last download fetches, in order.
    pub fetching: Vec<CivilDate>,
    /// The day row shown whole (a long message); others are one line ending in "…".
    pub selected: Option<usize>,
}

/// How long Download stays unpressable after the estimate arrives.
pub const SETTLE_MS: u64 = 600;

/// How long after "Cancel remaining" a "Stop now" press is accepted (as Reset paper's
/// confirmation, `paper_reset::READ_AT_LEAST`).
pub const STOP_AFTER_MS: u64 = 400;

/// What a Cancel press did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelAction {
    /// Nothing (a held key, too soon after Cancel remaining, nothing running).
    None,
    /// Finish the day in flight, start no other.
    Remaining,
    /// Stop at once (an estimate, or Stop now: the day in flight is discarded).
    Now,
}

/// What the main button does when pressed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Primary {
    CheckCost,
    Download,
}

/// `Thu 2026-11-05`.
pub fn day_text(d: CivilDate) -> String {
    const W: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    format!("{} {d}", W[d.weekday() as usize])
}

/// `2026-11-05` (also `2026-11-5`), else `None`.
pub fn parse_date(s: &str) -> Option<CivilDate> {
    let mut it = s.trim().split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 {
        return None;
    }
    CivilDate::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

/// The newest trade date of `product` whose session closed by `now`.
pub fn last_finished(product: &Product, now: UtcMillis) -> CivilDate {
    let mut d = CivilDate::from_utc(now).add_days(1);
    for _ in 0..14 {
        if let Some((_, close)) = lines_feed::disk::day::session_span(product, d)
            && close <= now
        {
            return d;
        }
        d = d.add_days(-1);
    }
    CivilDate::from_utc(now).add_days(-1)
}

impl Model {
    /// A dialog for these sources, on `symbol` and the last finished trade date.
    pub fn new(sources: Vec<SourceRow>, symbol: &str, now: UtcMillis) -> Model {
        let day = catalog::by_symbol(symbol)
            .or_else(|| match catalog::parse_target(symbol) {
                Some(Target::Contract(k)) => catalog::get(k.product),
                _ => None,
            })
            .map_or_else(
                || CivilDate::from_utc(now).add_days(-1),
                |p| last_finished(&p, now),
            );
        let error = sources
            .is_empty()
            .then(|| "No source in this build can download past days.".to_owned());
        Model {
            sources,
            source: 0,
            symbol: symbol.to_owned(),
            from: day.to_string(),
            to: day.to_string(),
            contents: Vec::new(),
            content: 0,
            focus: Field::Symbol,
            phase: Phase::Editing,
            rows: Vec::new(),
            contract_label: None,
            error,
            canceling: false,
            stopping: false,
            canceled_at: None,
            budget: None,
            store_bytes: None,
            ready_at: None,
            fetching: Vec::new(),
            selected: None,
        }
    }

    /// Fields shown (Source only when there is a choice).
    pub fn fields(&self) -> Vec<Field> {
        Field::ALL
            .into_iter()
            .filter(|f| *f != Field::Source || self.sources.len() > 1)
            .collect()
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Estimating | Phase::Downloading)
    }

    pub fn source_name(&self) -> Option<&str> {
        self.sources.get(self.source).map(|s| s.name.as_str())
    }

    pub fn contents_id(&self) -> Option<&'static str> {
        self.contents.get(self.content).map(|c| c.id)
    }

    /// The text of a field.
    pub fn value(&self, f: Field) -> String {
        match f {
            Field::Source => self
                .sources
                .get(self.source)
                .map_or_else(|| "none".into(), |s| s.label.clone()),
            Field::Symbol => self.symbol.clone(),
            Field::From => self.from.clone(),
            Field::To => self.to.clone(),
            Field::Contents => self
                .contents
                .get(self.content)
                .map_or_else(|| "none".into(), |c| c.label.to_owned()),
        }
    }

    /// Any input change drops the estimate: it was for other inputs.
    fn edited(&mut self) {
        if !self.busy() {
            self.phase = Phase::Editing;
            self.rows.clear();
            self.contract_label = None;
            self.ready_at = None;
            self.selected = None;
            if !self.sources.is_empty() {
                self.error = None;
            }
        }
    }

    pub fn focus(&mut self, f: Field) {
        if self.fields().contains(&f) {
            self.focus = f;
        }
    }

    /// Tab / Shift+Tab.
    pub fn next_field(&mut self, back: bool) {
        let fs = self.fields();
        let i = fs.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = fs.len();
        self.focus = fs[if back { (i + n - 1) % n } else { (i + 1) % n }];
    }

    pub fn type_text(&mut self, s: &str) {
        if self.busy() || !self.focus.is_text() {
            return;
        }
        let field = match self.focus {
            Field::Symbol => &mut self.symbol,
            Field::From => &mut self.from,
            _ => &mut self.to,
        };
        let limit = if self.focus == Field::Symbol { 12 } else { 10 };
        for c in s.chars() {
            let ok = match self.focus {
                Field::Symbol => c.is_ascii_alphanumeric() || c == '/',
                _ => c.is_ascii_digit() || c == '-',
            };
            if ok && field.chars().count() < limit {
                field.push(c.to_ascii_uppercase());
            }
        }
        self.edited();
    }

    pub fn backspace(&mut self) {
        if self.busy() {
            return;
        }
        match self.focus {
            Field::Symbol => {
                self.symbol.pop();
            }
            Field::From => {
                self.from.pop();
            }
            Field::To => {
                self.to.pop();
            }
            _ => return,
        }
        self.edited();
    }

    /// ← → on a choice steps it; on a date, one day.
    pub fn step(&mut self, by: i64) {
        if self.busy() {
            return;
        }
        match self.focus {
            Field::Source if !self.sources.is_empty() => {
                let n = self.sources.len() as i64;
                self.source = (self.source as i64 + by).rem_euclid(n) as usize;
                self.content = 0;
            }
            Field::Contents if !self.contents.is_empty() => {
                let n = self.contents.len() as i64;
                self.content = (self.content as i64 + by).rem_euclid(n) as usize;
            }
            Field::From | Field::To => {
                let s = if self.focus == Field::From {
                    &mut self.from
                } else {
                    &mut self.to
                };
                if let Some(d) = parse_date(s) {
                    *s = d.add_days(by).to_string();
                }
            }
            _ => return,
        }
        self.edited();
    }

    /// The current source's choices (the window asks the source when the source or product
    /// changes); the selection is kept when still offered.
    pub fn set_contents(&mut self, contents: Vec<Contents>) {
        let keep = self.contents_id();
        self.content = keep
            .and_then(|id| contents.iter().position(|c| c.id == id))
            .unwrap_or(0);
        self.contents = contents;
    }

    /// The symbol's product, if it names one.
    pub fn product(&self) -> Option<Product> {
        match catalog::parse_target(&self.symbol)? {
            Target::Product(p) => catalog::get(p),
            Target::Contract(k) => catalog::get(k.product),
        }
    }

    /// Parse the inputs. A product means its front contract on the first date.
    pub fn request(&self) -> Result<Request, String> {
        let from = parse_date(&self.from).ok_or("From: use a date like 2026-11-05.")?;
        let to = parse_date(&self.to).ok_or("To: use a date like 2026-11-05.")?;
        let dates = download::dates(from, to)?;
        let (product, contract) = match catalog::parse_target(&self.symbol) {
            Some(Target::Contract(k)) => (catalog::get(k.product).ok_or("Unknown product.")?, k),
            Some(Target::Product(id)) => {
                let p = catalog::get(id).ok_or("Unknown product.")?;
                let k = lines_core::roll::calendar_front(&p, from)
                    .ok_or_else(|| format!("{} has no front contract on {from}.", p.symbol))?;
                (p, k)
            }
            None => {
                return Err(format!(
                    "{:?} is not a symbol (try ESZ26 or ES).",
                    self.symbol
                ));
            }
        };
        if !matches!(contract.delivery, Delivery::Monthly { .. }) {
            return Err(format!(
                "{}: only futures contracts can be downloaded.",
                product.symbol
            ));
        }
        if self.contents.is_empty() {
            return Err(format!(
                "{} can't download {}.",
                self.value(Field::Source),
                product.symbol
            ));
        }
        Ok(Request {
            product,
            contract,
            dates,
        })
    }

    /// Check cost pressed: the store is looked at on the job's thread; rows come with its
    /// plan ([`Update::Plan`]).
    pub fn begin_estimate(&mut self, contract_label: String) {
        self.error = None;
        self.canceling = false;
        self.contract_label = Some(contract_label);
        self.rows.clear();
        self.selected = None;
        self.ready_at = None;
        self.store_bytes = None;
        self.phase = Phase::Estimating;
    }

    /// The input error to show.
    pub fn refuse(&mut self, why: String) {
        self.error = Some(why);
        self.phase = Phase::Editing;
        self.rows.clear();
    }

    /// Days the estimate priced, with the price accepted.
    pub fn to_download(&self) -> Vec<(CivilDate, Charge)> {
        self.rows
            .iter()
            .filter_map(|r| match (&r.status, r.est) {
                (Status::Priced, Some(e)) => Some((r.date, e.charge)),
                _ => None,
            })
            .collect()
    }

    /// Whether Download is ready to press; stamps the moment it became so (call after every
    /// update with the window's clock).
    pub fn settle(&mut self, now_ms: u64) {
        if self.phase == Phase::Estimated && !self.to_download().is_empty() {
            self.ready_at.get_or_insert(now_ms);
        } else {
            self.ready_at = None;
        }
    }

    /// What a press of the main button (Enter, or a click) at `now_ms` does. A held key
    /// never acts; Download needs a press at least [`SETTLE_MS`] after it became pressable,
    /// so the press that asked for the cost (held, repeated or doubled) can't start it.
    pub fn press(&self, now_ms: u64, held: bool) -> Option<Primary> {
        if held {
            return None;
        }
        match self.phase {
            Phase::Editing if !self.sources.is_empty() => Some(Primary::CheckCost),
            Phase::Estimated => self
                .ready_at
                .filter(|&t| now_ms >= t + SETTLE_MS)
                .map(|_| Primary::Download),
            _ => None,
        }
    }

    /// Download pressed: the priced days queue.
    pub fn begin_download(&mut self) -> Vec<(CivilDate, Charge)> {
        let days = self.to_download();
        if days.is_empty() || self.phase != Phase::Estimated {
            return Vec::new();
        }
        for r in &mut self.rows {
            if r.status == Status::Priced {
                r.status = Status::Queued;
            }
        }
        self.fetching = days.iter().map(|d| d.0).collect();
        self.canceling = false;
        self.ready_at = None;
        self.phase = Phase::Downloading;
        days
    }

    /// Cancel pressed: an estimate stops; a download first finishes the day in flight ("Cancel
    /// remaining"), and a second press stops it too ("Stop now"). Returns whether the job
    /// stops at once.
    pub fn cancel(&mut self, now_ms: u64, held: bool) -> CancelAction {
        if held {
            return CancelAction::None;
        }
        match self.phase {
            Phase::Estimating => {
                self.canceling = true;
                CancelAction::Now
            }
            Phase::Downloading if !self.canceling => {
                self.canceling = true;
                self.canceled_at = Some(now_ms);
                CancelAction::Remaining
            }
            Phase::Downloading
                if !self.stopping
                    && self
                        .canceled_at
                        .is_some_and(|t| now_ms >= t + STOP_AFTER_MS) =>
            {
                self.stopping = true;
                CancelAction::Now
            }
            _ => CancelAction::None,
        }
    }

    fn row_mut(&mut self, date: CivilDate) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.date == date)
    }

    /// One update from the running job.
    pub fn apply(&mut self, u: Update) {
        match u {
            Update::Plan { days, store_bytes } => {
                self.store_bytes = Some(store_bytes);
                self.rows = days
                    .into_iter()
                    .map(|(date, s)| Row {
                        date,
                        status: match s {
                            DayState::Missing => Status::Asking,
                            other => Status::Store(other),
                        },
                        est: None,
                        requested: false,
                    })
                    .collect();
            }
            Update::Estimate { date, result } => {
                if let Some(r) = self.row_mut(date) {
                    match result {
                        Ok(e) => {
                            r.status = Status::Priced;
                            r.est = Some(e);
                        }
                        Err(DayError::Refused(refusal)) => r.status = Status::Refused(refusal.0),
                        Err(DayError::Failed(m)) => r.status = Status::CheckFailed(m),
                    }
                }
            }
            Update::Started { date } => {
                if let Some(r) = self.row_mut(date) {
                    r.status = Status::Receiving { received: 0 };
                }
            }
            Update::Requested { date } => {
                if let Some(r) = self.row_mut(date) {
                    r.requested = true;
                }
            }
            Update::Progress { date, received } => {
                if let Some(r) = self.row_mut(date)
                    && matches!(r.status, Status::Receiving { .. })
                {
                    r.status = Status::Receiving { received };
                }
            }
            Update::Day { date, outcome } => {
                if let Some(r) = self.row_mut(date) {
                    r.status = match outcome {
                        Outcome::Stored(w) => Status::Downloaded { bytes: w.bytes },
                        Outcome::Skipped(s) => Status::Store(s),
                        Outcome::Busy => Status::Busy,
                        Outcome::Refused(m) => Status::Refused(m),
                        Outcome::PriceChanged(c) => Status::PriceChanged(c),
                        Outcome::Canceled => Status::Canceled,
                        Outcome::Failed(m) => Status::Failed(m),
                    };
                }
            }
            Update::Finished => {
                // Days an estimate never reached (canceled): not asked.
                for r in &mut self.rows {
                    if r.status == Status::Asking {
                        r.status = Status::NotAsked;
                    }
                }
                self.phase = match self.phase {
                    Phase::Estimating => Phase::Estimated,
                    Phase::Downloading => Phase::Finished,
                    other => other,
                };
                self.canceling = false;
                self.stopping = false;
            }
        }
    }

    /// Days to download, their total price and size.
    pub fn total(&self) -> (usize, u64, u64) {
        self.rows
            .iter()
            .fold((0, 0, 0), |(n, usd, b), r| match (&r.status, r.est) {
                (Status::Priced | Status::Queued, Some(e)) => {
                    (n + 1, usd + e.charge.usd_micros(), b + e.bytes)
                }
                _ => (n, usd, b),
            })
    }

    /// A running download's days: (done, of total), for the window footer's activity line.
    pub fn days_progress(&self) -> Option<(usize, usize)> {
        (self.phase == Phase::Downloading).then(|| {
            let (_, _, done, _) = self.progress();
            (done, self.fetching.len())
        })
    }

    /// The download's days: (estimated total, spent so far, done, the day in flight 1-based).
    fn progress(&self) -> (u64, u64, usize, Option<usize>) {
        let mut est = 0;
        let mut spent = 0;
        let mut done = 0;
        let mut current = None;
        for (i, d) in self.fetching.iter().enumerate() {
            let Some(r) = self.rows.iter().find(|r| r.date == *d) else {
                continue;
            };
            let usd = r.est.map_or(0, |e| e.charge.usd_micros());
            est += usd;
            // Billed from the request on, whatever followed: never reads lower than it is.
            if r.requested {
                spent += usd;
            }
            match r.status {
                Status::Downloaded { .. } => done += 1,
                Status::Receiving { .. } => current = Some(i + 1),
                _ => {}
            }
        }
        (est, spent, done, current)
    }

    /// The line under the days.
    pub fn summary(&self) -> String {
        let (n, usd, bytes) = self.total();
        let unavailable = self.unavailable();
        let tail = if unavailable > 0 {
            // A no-break space: the count never wraps away from its word.
            format!(" · {unavailable}\u{a0}unavailable")
        } else {
            String::new()
        };
        match self.phase {
            Phase::Editing => String::new(),
            Phase::Estimating => "Checking the cost…".into(),
            Phase::Estimated if n == 0 => format!("Nothing to download{tail}"),
            Phase::Estimated => format!(
                "{} to download · {} · {}{tail}",
                days(n),
                if usd == 0 {
                    "Included in your plan".to_owned()
                } else {
                    format!("Estimated {}", usd_text(usd))
                },
                size_text(bytes)
            ),
            Phase::Downloading | Phase::Finished => {
                let (est, spent, done, current) = self.progress();
                let total = self.fetching.len();
                let mut s = match (self.phase, current) {
                    (Phase::Downloading, _) if self.stopping => format!("Stopping… · {done} done"),
                    (Phase::Downloading, _) if self.canceling => {
                        format!("Stopping after this day · {done} done")
                    }
                    (Phase::Downloading, Some(k)) => format!("Day {k} of {total} · {done} done"),
                    (Phase::Downloading, None) => format!("{done} of {} done", days(total)),
                    _ => format!("Downloaded {done} of {}", days(total)),
                };
                if est > 0 {
                    s.push_str(&format!(
                        " · Charged {} of est. {}",
                        usd_text(spent),
                        usd_text(est)
                    ));
                }
                s.push_str(&tail);
                s
            }
        }
    }

    /// Days the provider or the store can't give (refused, failed, busy, repriced).
    pub fn unavailable(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r.status,
                    Status::Refused(_)
                        | Status::Failed(_)
                        | Status::CheckFailed(_)
                        | Status::Busy
                        | Status::Store(DayState::Busy)
                        | Status::PriceChanged(_)
                )
            })
            .count()
    }

    /// When the days to download push the store past its budget: they are kept anyway
    /// (downloaded days are never evicted), so other days go. Certain when the store is over
    /// already; otherwise the estimate's size (the provider's measure, usually larger than
    /// what is stored) only says it may.
    pub fn budget_note(&self) -> Option<String> {
        if self.phase != Phase::Estimated {
            return None;
        }
        let (budget, held) = (self.budget?, self.store_bytes?);
        let (n, _, bytes) = self.total();
        let after = held.saturating_add(bytes);
        if n == 0 || after <= budget {
            return None;
        }
        // Downloaded days are pinned (retention never removes them).
        Some(if held > budget {
            format!(
                "Passes the {} store budget by {}: older live days are removed first; \
                 downloaded days are kept.",
                size_text(budget),
                size_text(after - budget)
            )
        } else {
            format!(
                "Up to {} before compression · store budget {}: older live days are removed \
                 first; downloaded days are kept.",
                size_text(bytes),
                size_text(budget)
            )
        })
    }

    /// The main button's words, or `None` when there's nothing to press.
    pub fn primary(&self) -> Option<String> {
        match self.phase {
            Phase::Editing if self.sources.is_empty() => None,
            Phase::Editing => Some("Check cost".into()),
            Phase::Estimated => {
                let (n, usd, _) = self.total();
                (n > 0).then(|| {
                    if usd == 0 {
                        format!("Download {} · included", days(n))
                    } else {
                        format!("Download {} · est. {}", days(n), usd_text(usd))
                    }
                })
            }
            Phase::Estimating | Phase::Downloading | Phase::Finished => None,
        }
    }

    /// The other button.
    pub fn secondary(&self) -> &'static str {
        match self.phase {
            Phase::Downloading if self.canceling => "Stop now",
            Phase::Downloading => "Cancel remaining",
            Phase::Estimating => "Cancel",
            _ => "Close",
        }
    }

    /// ↑ ↓ select a day (its whole message shows).
    pub fn select(&mut self, by: i64) {
        if self.rows.is_empty() {
            self.selected = None;
            return;
        }
        let n = self.rows.len() as i64;
        let at = match self.selected {
            Some(i) => (i as i64 + by).clamp(0, n - 1),
            None if by < 0 => n - 1,
            None => 0,
        };
        self.selected = Some(at as usize);
    }

    pub fn row_text(&self, r: &Row) -> RowText {
        let paid = r.est.map_or(0, |e| e.charge.usd_micros());
        let cost = match (&r.status, r.est) {
            (_, None) => String::new(),
            (Status::Priced | Status::Queued, _) | (Status::Receiving { .. }, _)
                if !r.requested =>
            {
                if paid == 0 {
                    "$0.00".into()
                } else {
                    format!("est. {}", usd_text(paid))
                }
            }
            // From the request on, a paid day may be billed (finished, stopped or failed).
            (_, Some(_)) if r.requested => {
                if paid == 0 {
                    "$0.00".into()
                } else {
                    format!("charged {}", usd_text(paid))
                }
            }
            _ => String::new(),
        };
        let billed = r.requested && paid > 0;
        let est_size = r.est.map(|e| e.bytes);
        let (status, size, problem, quiet) = match &r.status {
            Status::Store(DayState::Stored { complete: true }) => {
                ("Already stored".into(), String::new(), false, true)
            }
            Status::Store(DayState::Stored { complete: false }) => (
                "Already stored (partly: recorded live)".into(),
                String::new(),
                false,
                true,
            ),
            Status::Store(DayState::NoSession) => ("No session".into(), String::new(), false, true),
            Status::Store(DayState::NotFinished) => {
                ("Session not over yet".into(), String::new(), false, true)
            }
            Status::Store(DayState::Missing) | Status::Asking => {
                ("Checking…".into(), String::new(), false, false)
            }
            Status::Priced => (
                match r.est.map(|e| e.charge) {
                    Some(Charge::Included) | None => "Included in your plan".into(),
                    Some(Charge::Estimated { .. }) => "Estimated".into(),
                },
                est_size.map(size_text).unwrap_or_default(),
                false,
                false,
            ),
            Status::Refused(m) => (format!("✕ Not available: {m}"), String::new(), true, false),
            Status::Store(DayState::Busy) => (
                "✕ Busy: this contract is being recorded or downloaded".into(),
                String::new(),
                true,
                false,
            ),
            Status::Queued => (
                "Waiting".into(),
                est_size.map(size_text).unwrap_or_default(),
                false,
                false,
            ),
            Status::Receiving { received } => (
                "Downloading…".into(),
                match est_size {
                    Some(b) => format!("{} / ~{}", size_text(*received), size_text(b)),
                    None => size_text(*received),
                },
                false,
                false,
            ),
            Status::Downloaded { bytes } => {
                ("✓ Downloaded".into(), size_text(*bytes), false, false)
            }
            Status::NotAsked => ("Not checked".into(), String::new(), false, true),
            Status::CheckFailed(m) => (
                format!("✕ Couldn't check the cost: {m}"),
                String::new(),
                true,
                false,
            ),
            Status::Busy => (
                "✕ Busy: this contract is being recorded or downloaded; try again when that ends"
                    .into(),
                String::new(),
                true,
                false,
            ),
            Status::PriceChanged(c) => (
                format!(
                    "Not downloaded: the price is now {}",
                    c.text().to_lowercase()
                ),
                String::new(),
                true,
                false,
            ),
            Status::Canceled if billed => (
                "Canceled: nothing stored (may be billed)".into(),
                String::new(),
                false,
                true,
            ),
            Status::Canceled => (
                "Canceled: nothing stored".into(),
                String::new(),
                false,
                true,
            ),
            Status::Failed(m) if billed => (
                format!("✕ Not stored (may be billed): {m}"),
                String::new(),
                true,
                false,
            ),
            Status::Failed(m) => (format!("✕ Not stored: {m}"), String::new(), true, false),
        };
        RowText {
            day: day_text(r.date),
            status,
            cost,
            size,
            problem,
            quiet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::disk::day::Written;
    use lines_feed::download::{DayError, Refusal, demo};

    fn d(day: u8) -> CivilDate {
        CivilDate::new(2026, 11, day).unwrap()
    }

    fn model() -> Model {
        let now = UtcMillis::parse_iso("2026-11-09T12:00:00Z").unwrap();
        let mut m = Model::new(
            vec![SourceRow {
                name: "demo".into(),
                label: "Demo".into(),
            }],
            "ESZ26",
            now,
        );
        m.set_contents(vec![demo::TOB, demo::DEPTH]);
        m
    }

    fn est(usd_micros: u64, bytes: u64) -> DayEstimate {
        DayEstimate {
            charge: if usd_micros == 0 {
                Charge::Included
            } else {
                Charge::Estimated { usd_micros }
            },
            bytes,
        }
    }

    fn plan(m: &mut Model, days: Vec<(CivilDate, DayState)>) {
        m.begin_estimate("ESZ26".into());
        m.apply(Update::Plan {
            days,
            store_bytes: 0,
        });
    }

    /// An estimate of `days`, all priced at `usd`, finished at t = 1000 ms.
    fn estimated(days: &[u8], usd: u64) -> Model {
        let mut m = model();
        plan(
            &mut m,
            days.iter().map(|&n| (d(n), DayState::Missing)).collect(),
        );
        for &n in days {
            m.apply(Update::Estimate {
                date: d(n),
                result: Ok(est(usd, 61_000_000)),
            });
        }
        m.apply(Update::Finished);
        m.settle(1000);
        m
    }

    #[test]
    fn opens_on_the_last_finished_trade_date() {
        let m = model();
        // Monday noon UTC: Monday's session isn't over, Friday 2026-11-06 is the last.
        assert_eq!(m.from, "2026-11-06");
        assert_eq!(m.to, "2026-11-06");
        assert_eq!(m.focus, Field::Symbol);
        assert_eq!(
            m.fields(),
            vec![Field::Symbol, Field::From, Field::To, Field::Contents]
        );
        assert_eq!(m.primary().as_deref(), Some("Check cost"));
        assert_eq!(m.secondary(), "Close");
    }

    #[test]
    fn inputs_parse_and_edit() {
        let mut m = model();
        let r = m.request().unwrap();
        assert_eq!(r.contract.label(&r.product).as_str(), "ESZ26");
        m.focus(Field::Symbol);
        for _ in 0..5 {
            m.backspace();
        }
        m.type_text("es");
        let r = m.request().unwrap();
        assert_eq!(
            r.contract.label(&r.product).as_str(),
            "ESZ26",
            "front contract"
        );
        m.type_text("Q!");
        assert_eq!(m.symbol, "ESQ");
        assert!(m.request().unwrap_err().contains("not a symbol"));
        m.focus(Field::From);
        m.step(-3);
        assert_eq!(m.from, "2026-11-03");
        m.next_field(false);
        assert_eq!(m.focus, Field::To);
        m.backspace();
        assert!(m.request().is_err());
        m.focus(Field::Contents);
        m.step(1);
        assert_eq!(m.value(Field::Contents), "Full depth");
        m.step(1);
        assert_eq!(m.value(Field::Contents), "Trades + top of book");
        m.set_contents(vec![demo::DEPTH]);
        assert_eq!(m.contents_id(), Some("depth"));
    }

    #[test]
    fn an_estimate_lists_every_day_and_totals_what_will_be_fetched() {
        let mut m = model();
        plan(
            &mut m,
            vec![
                (d(3), DayState::Missing),
                (d(4), DayState::Stored { complete: true }),
                (d(5), DayState::Missing),
                (d(7), DayState::NoSession),
                (d(6), DayState::Missing),
            ],
        );
        assert_eq!(m.summary(), "Checking the cost…");
        assert_eq!(m.primary(), None);
        assert_eq!(m.secondary(), "Cancel");
        m.apply(Update::Estimate {
            date: d(3),
            result: Ok(est(1_630_000, 3_510_000_000)),
        });
        m.apply(Update::Estimate {
            date: d(5),
            result: Ok(est(0, 61_000_000)),
        });
        let msg = "This dataset isn't included in your plan before 2026-11-06.";
        m.apply(Update::Estimate {
            date: d(6),
            result: Err(DayError::Refused(Refusal(msg.into()))),
        });
        m.apply(Update::Finished);
        assert_eq!(m.phase, Phase::Estimated);
        let texts: Vec<(String, String, String, bool)> = m
            .rows
            .iter()
            .map(|r| {
                let t = m.row_text(r);
                (t.status, t.cost, t.size, t.problem)
            })
            .collect();
        let s = |a: &str| a.to_owned();
        assert_eq!(
            texts,
            vec![
                (s("Estimated"), s("est. $1.63"), s("3.27 GiB"), false),
                (s("Already stored"), s(""), s(""), false),
                (s("Included in your plan"), s("$0.00"), s("58.2 MiB"), false),
                (s("No session"), s(""), s(""), false),
                (format!("✕ Not available: {msg}"), s(""), s(""), true),
            ]
        );
        assert_eq!(
            m.summary(),
            "2 days to download · Estimated $1.63 · 3.33 GiB · 1\u{a0}unavailable"
        );
        assert_eq!(m.primary().as_deref(), Some("Download 2 days · est. $1.63"));
        assert_eq!(m.row_text(&m.rows[2]).cost, "$0.00");
        assert_eq!(
            m.to_download(),
            vec![
                (
                    d(3),
                    Charge::Estimated {
                        usd_micros: 1_630_000
                    }
                ),
                (d(5), Charge::Included)
            ]
        );
        assert_eq!(m.row_text(&m.rows[0]).day, "Tue 2026-11-03");
    }

    /// Design §6: a press that asked for the cost, held, repeated or doubled, never starts a
    /// (possibly paid) download; a fresh press after the estimate shows does.
    #[test]
    fn the_press_that_checked_the_cost_never_downloads() {
        let mut m = model();
        assert_eq!(m.press(0, false), Some(Primary::CheckCost));
        assert_eq!(m.press(0, true), None, "a held key never acts");
        // The estimate arrives 40 ms after the press (a fast answer).
        m.begin_estimate("ESZ26".into());
        m.apply(Update::Plan {
            days: vec![(d(5), DayState::Missing)],
            store_bytes: 0,
        });
        assert_eq!(m.press(20, false), None, "still estimating");
        m.apply(Update::Estimate {
            date: d(5),
            result: Ok(est(1_630_000, 1)),
        });
        m.apply(Update::Finished);
        m.settle(40);
        assert_eq!(m.press(40, true), None, "held Enter repeating");
        assert_eq!(m.press(90, false), None, "a double press or click");
        assert_eq!(m.press(40 + SETTLE_MS - 1, false), None);
        assert_eq!(m.press(40 + SETTLE_MS, false), Some(Primary::Download));
        assert_eq!(m.press(40 + SETTLE_MS, true), None);
        // Settling again doesn't move the moment it became ready.
        m.settle(5_000);
        assert_eq!(m.ready_at, Some(40));
        // Any edit takes it back to Check cost.
        m.focus(Field::To);
        m.step(1);
        assert_eq!(m.ready_at, None);
        assert_eq!(m.press(10_000, false), Some(Primary::CheckCost));
    }

    #[test]
    fn every_day_stored_offers_nothing() {
        let mut m = model();
        plan(&mut m, vec![(d(5), DayState::Stored { complete: true })]);
        m.apply(Update::Finished);
        assert_eq!(m.phase, Phase::Estimated);
        // A second end (nothing was asked) leaves it there.
        m.apply(Update::Finished);
        assert_eq!(m.phase, Phase::Estimated);
        m.settle(1);
        assert_eq!(m.ready_at, None);
        assert_eq!(m.summary(), "Nothing to download");
        assert_eq!(m.primary(), None);
        assert!(m.begin_download().is_empty());
    }

    #[test]
    fn the_budget_note_shows_when_the_days_may_not_fit() {
        let mut m = estimated(&[5], 0);
        m.budget = Some(100_000_000);
        m.store_bytes = Some(10_000_000);
        assert_eq!(m.budget_note(), None, "61 MB more fits");
        m.store_bytes = Some(50_000_000);
        assert_eq!(
            m.budget_note().as_deref(),
            Some(
                "Up to 58.2 MiB before compression · store budget 95.4 MiB: older live days are \
                 removed first; downloaded days are kept."
            )
        );
        m.store_bytes = Some(120_000_000);
        assert_eq!(
            m.budget_note().as_deref(),
            Some(
                "Passes the 95.4 MiB store budget by 77.2 MiB: older live days are removed \
                 first; downloaded days are kept."
            )
        );
    }

    /// Changing what to download (the contents) drops the estimate: Check cost asks again.
    #[test]
    fn a_contents_change_asks_for_the_cost_again() {
        let mut m = estimated(&[5], 1_630_000);
        assert_eq!(m.phase, Phase::Estimated);
        m.focus(Field::Contents);
        m.step(1);
        assert_eq!(m.phase, Phase::Editing);
        assert!(m.rows.is_empty() && m.to_download().is_empty());
        assert_eq!(m.press(10_000, false), Some(Primary::CheckCost));
    }

    /// A canceled estimate leaves the days it never reached "Not checked".
    #[test]
    fn a_canceled_estimate_marks_unasked_days() {
        let mut m = model();
        plan(
            &mut m,
            vec![(d(5), DayState::Missing), (d(6), DayState::Missing)],
        );
        m.apply(Update::Estimate {
            date: d(5),
            result: Ok(est(0, 1)),
        });
        assert_eq!(
            m.cancel(0, false),
            CancelAction::Now,
            "an estimate stops at once"
        );
        m.apply(Update::Finished);
        assert_eq!(m.row_text(&m.rows[1]).status, "Not checked");
        assert_eq!(
            m.summary(),
            "1 day to download · Included in your plan · 1 B"
        );
    }

    #[test]
    fn a_download_reports_progress_cancel_and_the_end() {
        let mut m = model();
        plan(
            &mut m,
            vec![
                (d(3), DayState::Missing),
                (d(4), DayState::Missing),
                (d(5), DayState::Missing),
            ],
        );
        m.apply(Update::Estimate {
            date: d(3),
            result: Err(DayError::Failed("no key".into())),
        });
        for day in [4, 5] {
            m.apply(Update::Estimate {
                date: d(day),
                result: Ok(est(1_630_000, 61_000_000)),
            });
        }
        m.apply(Update::Finished);
        // Not the provider's answer: said as this side's problem.
        assert_eq!(
            m.row_text(&m.rows[0]).status,
            "✕ Couldn't check the cost: no key"
        );
        let days = m.begin_download();
        assert_eq!(days.len(), 2);
        assert_eq!(m.phase, Phase::Downloading);
        assert_eq!(m.secondary(), "Cancel remaining");
        assert_eq!(m.row_text(&m.rows[2]).status, "Waiting");
        m.apply(Update::Started { date: d(4) });
        m.apply(Update::Requested { date: d(4) });
        m.apply(Update::Progress {
            date: d(4),
            received: 12 * 1024 * 1024,
        });
        assert_eq!(m.row_text(&m.rows[1]).size, "12.0 MiB / ~58.2 MiB");
        assert_eq!(
            m.summary(),
            "Day 1 of 2 · 0 done · Charged $1.63 of est. $3.26 · 1\u{a0}unavailable"
        );
        m.apply(Update::Day {
            date: d(4),
            outcome: Outcome::Stored(Written {
                bytes: 9 * 1024 * 1024,
                ..Written::default()
            }),
        });
        m.apply(Update::Started { date: d(5) });
        assert_eq!(
            m.summary(),
            "Day 2 of 2 · 1 done · Charged $1.63 of est. $3.26 · 1\u{a0}unavailable"
        );
        assert_eq!(m.row_text(&m.rows[1]).status, "✓ Downloaded");
        assert_eq!(m.row_text(&m.rows[1]).cost, "charged $1.63");
        assert_eq!(m.row_text(&m.rows[2]).cost, "est. $1.63");
        m.input_while_busy_is_ignored();
        assert_eq!(
            m.cancel(1000, true),
            CancelAction::None,
            "a held Esc does nothing"
        );
        assert_eq!(m.cancel(1000, false), CancelAction::Remaining);
        assert!(m.summary().starts_with("Stopping after this day · 1 done"));
        assert_eq!(m.secondary(), "Stop now");
        // A double press or a held key never discards the (paid) day in flight.
        assert_eq!(m.cancel(1050, false), CancelAction::None, "a double press");
        assert_eq!(
            m.cancel(1000 + STOP_AFTER_MS, true),
            CancelAction::None,
            "held"
        );
        assert_eq!(m.cancel(1000 + STOP_AFTER_MS, false), CancelAction::Now);
        assert!(m.summary().starts_with("Stopping… · 1 done"));
        m.apply(Update::Requested { date: d(5) });
        m.apply(Update::Day {
            date: d(5),
            outcome: Outcome::Canceled,
        });
        m.apply(Update::Finished);
        assert_eq!(m.phase, Phase::Finished);
        assert_eq!(
            m.summary(),
            "Downloaded 1 of 2 days · Charged $3.26 of est. $3.26 · 1\u{a0}unavailable"
        );
        // Stopped after its request: it may be billed, and the total says so.
        assert_eq!(
            m.row_text(&m.rows[2]).status,
            "Canceled: nothing stored (may be billed)"
        );
        assert_eq!(m.row_text(&m.rows[2]).cost, "charged $1.63");
        assert_eq!(m.secondary(), "Close");
        // Editing starts over.
        m.focus(Field::To);
        m.step(1);
        assert_eq!(m.phase, Phase::Editing);
        assert!(m.rows.is_empty());
    }

    #[test]
    fn problems_use_the_providers_words_and_rows_can_be_selected() {
        let mut m = estimated(&[5, 6], 0);
        m.begin_download();
        m.apply(Update::Day {
            date: d(5),
            outcome: Outcome::Refused("401 auth failed: check your API key".into()),
        });
        let t = m.row_text(&m.rows[0]);
        assert_eq!(
            t.status,
            "✕ Not available: 401 auth failed: check your API key"
        );
        assert!(t.problem);
        m.apply(Update::Day {
            date: d(5),
            outcome: Outcome::Busy,
        });
        assert!(
            m.row_text(&m.rows[0])
                .status
                .contains("being recorded or downloaded")
        );
        m.apply(Update::Day {
            date: d(5),
            outcome: Outcome::PriceChanged(Charge::Estimated {
                usd_micros: 520_000,
            }),
        });
        assert_eq!(
            m.row_text(&m.rows[0]).status,
            "Not downloaded: the price is now estimated $0.52"
        );
        m.select(1);
        assert_eq!(m.selected, Some(0));
        m.select(1);
        m.select(1);
        assert_eq!(m.selected, Some(1));
        m.select(-5);
        assert_eq!(m.selected, Some(0));
    }

    #[test]
    fn no_source_says_so() {
        let now = UtcMillis::parse_iso("2026-11-09T12:00:00Z").unwrap();
        let m = Model::new(vec![], "ES", now);
        assert!(m.error.as_deref().unwrap().contains("No source"));
        assert_eq!(m.primary(), None);
        assert_eq!(m.press(0, false), None);
    }

    impl Model {
        fn input_while_busy_is_ignored(&mut self) {
            let before = (self.symbol.clone(), self.from.clone(), self.content);
            self.focus(Field::Symbol);
            self.type_text("X");
            self.backspace();
            self.focus(Field::Contents);
            self.step(1);
            assert_eq!(
                before,
                (self.symbol.clone(), self.from.clone(), self.content)
            );
        }
    }
}
