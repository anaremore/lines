//! The journal pane (roadmap 8c): every closed trade, newest first, with a stats strip for the
//! current trade date (or all dates) on top; or P&L by account or by tag (phase 2).
//!
//! - **Table**: time, contract, side, quantity, average entry and exit, net P&L, ticks,
//!   duration, MAE/MFE, tags; resizable, hideable columns (`lines_core::columns`, saved with
//!   the pane). P&L is signed text (`+$310.02`), colored up/down: never color alone.
//! - **P&L display** (`crate::pnl`, app-wide): off shows no money or tick number at all
//!   (P&L, ticks, MAE/MFE and the strip's money read `—`; count, win rate and streak stay);
//!   ticks or currency show only that unit. CSV export is always complete.
//! - **One basis**: dollars are the whole position; ticks are always per contract (`t/ct`, the
//!   CSV's too), so 4 contracts' `+$100.00` is `+2.0 t/ct` on ES. MAE/MFE ticks are their exact
//!   dollars at the trade's largest size (`Trade::excursion_ticks`), so the cell reconciles.
//! - **Stats strip**: count, win rate, net, average win and loss, expectancy, profit factor,
//!   max drawdown, streak; click it (or the palette) to switch between the current trade date
//!   and all dates.
//! - **Editing**: click a row (or Up/Down) to select it, Enter to edit its tags and note
//!   inline (Tab switches field, Enter saves, Esc cancels). Edits are saved as journal records.
//! - **Chart** (`crate::journal_shots`): a trade with a trade snapshot shows "view" in the Snapshot
//!   column (click it, or the palette's command on the selected trade: the OS's default app);
//!   without one, a dash. A file that isn't there says so in a notice.
//! - **By account / by tag** (phase 2; header tabs, palette, pane menu, saved with the pane):
//!   trades, win rate, net, average trade, profit factor and max drawdown per account or tag
//!   (a trade with several tags counts in each), over the same dates as the strip.
//!
//! The pane reads `lines_trading::JournalShared`'s published snapshots (the journal runs on its
//! own thread): it polls the journal's lock-free revision (like a store's), takes the newest
//! snapshot (a pointer clone, never waiting) and re-formats its rows only when the revision,
//! the P&L display or the trade date moved, so a frame paints cached strings and no lock is
//! held while formatting. It has a feed like every pane (link groups, plumbing) but
//! shows every account's trades whatever its market.

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, Styled,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::{CivilDate, UtcMillis};
use lines_feed::store::ChartStore;
use lines_trading::journal::stats::{self, GroupBy, Stats};
use lines_trading::journal::{Edit, Trade, price_value};
use lines_trading::{JournalShared, Money, Side};

use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pane_columns::{
    self, ColumnCmd, ColumnMenu, Columns, HEADER_ROW_H, HeaderHit, JournalCol, Table,
};
use crate::pnl::{self, PnlDisplay};
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::trading::Trading;
use crate::wsdoc::{JournalGroup, JournalState, PaneDoc};

const ROW_H: f32 = 18.0;
const TEXT_PX: f32 = 12.0;
const PAD: f32 = 6.0;

/// One trade as the table shows it (formatted once per journal change).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    /// Indexed by [`JournalCol`].
    pub cells: [String; 13],
    /// Net P&L sign: colours the P&L cell (its text is signed too).
    pub sign: i8,
    pub note: String,
    pub tags: Vec<String>,
    /// The trade snapshot's name (in the journal's `shots/` folder), if the trade has one.
    pub shot: Option<String>,
}

fn money_text(m: Money) -> String {
    let mut s = String::new();
    let _ = pnl::write_money(&mut s, m);
    s
}

fn ticks_text(t: f64) -> String {
    let mut s = String::new();
    let _ = pnl::write_ticks(&mut s, t);
    s
}

/// `+6.3`: ticks under a header that gives their unit.
fn tick_number(t: f64) -> String {
    let s = ticks_text(t);
    s.strip_suffix(" t").unwrap_or(&s).to_owned()
}

/// `+6.3 t/ct`: ticks where a header doesn't already say they are per contract.
pub(crate) fn per_contract_text(t: f64) -> String {
    let mut s = ticks_text(t);
    s.push_str("/ct");
    s
}

/// Format a trade for the table, showing P&L as `mode` allows.
pub fn row(t: &Trade, mode: PnlDisplay) -> Row {
    let product = lines_feed::catalog::get(t.contract.product);
    let tick = product.as_ref().map_or(1, |p| p.price.tick);
    let price = |v: Option<f64>| match (v, &product) {
        (Some(v), Some(p)) => decimal(price_value(p, v), p.price.scale.den()),
        _ => "—".to_owned(),
    };
    let qty = match &product {
        Some(p) => {
            let mut q = String::new();
            let _ = p.qty.write(lines_core::Qty(t.max_qty), &mut q);
            q
        }
        None => t.max_qty.to_string(),
    };
    let contract = product.as_ref().map_or_else(
        || format!("{:?}", t.contract.product),
        |p| t.contract.label(p).as_str().to_owned(),
    );
    let mut time = clock_text(t.entry_time);
    if t.incomplete {
        time.push_str(" ≈");
    }
    let dash = || "—".to_owned();
    let tk = |v: Option<f64>| match v {
        Some(v) if mode.ticks() => tick_number(v),
        _ => dash(),
    };
    let net = t.net();
    let pnl = if mode.money() {
        money_text(net)
    } else if mode.ticks() {
        t.ticks(tick).map_or_else(dash, per_contract_text)
    } else {
        dash()
    };
    // The whole position's worst/best P&L while open, and the same dollars per contract (like the
    // Ticks column and the CSV) at the trade's largest size. An unlisted product: no tick to say.
    let excursion_ticks = match product.as_ref().and_then(|p| t.excursion_ticks(p)) {
        Some((mae, mfe)) => format!("{} / {}", tick_number(mae), per_contract_text(mfe)),
        None => "— / —".to_owned(),
    };
    let excursions = match (mode.money(), mode.ticks()) {
        (true, true) => format!(
            "{} / {}  ({excursion_ticks})",
            money_text(t.mae),
            money_text(t.mfe),
        ),
        (true, false) => format!("{} / {}", money_text(t.mae), money_text(t.mfe)),
        (false, true) => excursion_ticks,
        (false, false) => dash(),
    };
    Row {
        id: t.id.clone(),
        cells: [
            time,
            t.account.0.clone(),
            contract,
            if t.side == Side::Buy { "Long" } else { "Short" }.to_owned(),
            qty,
            price(t.avg_entry()),
            price(t.avg_exit()),
            pnl,
            tk(t.ticks(tick)),
            duration_text(t.duration_ms()),
            excursions,
            if t.screenshots.is_empty() {
                dash()
            } else {
                "view".to_owned()
            },
            t.tags.join(", "),
        ],
        // The color follows the value shown: net money, else the ticks as rounded.
        sign: if mode.money() {
            match pnl::tone(net) {
                pnl::Tone::Up => 1,
                pnl::Tone::Down => -1,
                pnl::Tone::Flat => 0,
            }
        } else if mode.ticks() {
            t.ticks(tick)
                .map_or(0, |v| ((v * 10.0).round() as i64).signum() as i8)
        } else {
            0
        },
        note: t.note.clone(),
        tags: t.tags.clone(),
        shot: t.screenshots.first().cloned(),
    }
}

/// A price with the decimals its scale implies (`den` 100 → at least 2), more if the value
/// has them (averages), at most 6, without trailing zeros past the minimum.
pub(crate) fn decimal(v: f64, den: i64) -> String {
    let min = (den.max(1) as f64).log10().ceil() as usize;
    let s = format!("{v:.6}");
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut frac = frac.trim_end_matches('0').to_owned();
    while frac.len() < min {
        frac.push('0');
    }
    if frac.is_empty() {
        int.to_owned()
    } else {
        format!("{int}.{frac}")
    }
}

/// `11-05 14:31:07` (UTC, like the chart's axis).
pub fn clock_text(t: UtcMillis) -> String {
    let d = CivilDate::from_utc(t);
    let s = t.0.rem_euclid(86_400_000) / 1000;
    format!(
        "{:02}-{:02} {:02}:{:02}:{:02}",
        d.month,
        d.day,
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `45s`, `2m 05s`, `1h 03m`.
pub fn duration_text(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

/// One item of the stats strip. Items are whole: a narrow pane leaves out the last ones
/// (they wrap out of sight), never part of a number.
#[derive(Clone, Debug, PartialEq)]
pub struct StatItem {
    pub text: String,
    /// The value's sign, for its color (its text is signed too); 0: plain.
    pub tone: i8,
    /// The loudest item (net P&L).
    pub strong: bool,
}

fn item(text: String) -> StatItem {
    StatItem {
        text,
        tone: 0,
        strong: false,
    }
}

/// The stats strip in priority order: the count, then net P&L (loudest) and max drawdown,
/// then win rate, averages, expectancy, profit factor, streak, open trades. Money only when
/// `mode` shows currency (stats are money: none in tick or off modes).
pub fn stats_items(label: &str, s: &Stats, open: usize, mode: PnlDisplay) -> Vec<StatItem> {
    let mut out = vec![item(format!(
        "{label}: {} trade{}",
        s.count,
        if s.count == 1 { "" } else { "s" }
    ))];
    if s.count > 0 {
        let money = |m: Option<Money>| m.map_or_else(|| "—".to_owned(), money_text);
        if mode.money() {
            out.push(StatItem {
                text: format!("net {}", money_text(s.net)),
                tone: s.net.0.signum() as i8,
                strong: true,
            });
            out.push(item(format!(
                "max drawdown {}",
                money_text(-s.max_drawdown)
            )));
        }
        out.push(item(format!(
            "win {:.0}%",
            s.win_rate().unwrap_or(0.0) * 100.0
        )));
        if mode.money() {
            out.push(item(format!("avg win {}", money(s.avg_win()))));
            out.push(item(format!("avg loss {}", money(s.avg_loss()))));
            out.push(item(format!("expectancy {}", money(s.expectancy()))));
            out.push(item(format!(
                "profit factor {}",
                s.profit_factor()
                    .map_or_else(|| "—".to_owned(), |p| format!("{p:.2}"))
            )));
        }
        out.push(item(match s.streak {
            0 => "no streak".to_owned(),
            1 => "1 win in a row".to_owned(),
            -1 => "1 loss in a row".to_owned(),
            n if n > 0 => format!("{n} wins in a row"),
            n => format!("{} losses in a row", -n),
        }));
    }
    if open > 0 {
        out.push(item(format!("{open} open")));
    }
    out
}

/// The strip as one line of text (tests).
#[cfg(test)]
pub fn stats_text(label: &str, s: &Stats, open: usize, mode: PnlDisplay) -> String {
    stats_items(label, s, open, mode)
        .into_iter()
        .map(|i| i.text)
        .collect::<Vec<_>>()
        .join("  ·  ")
}

/// Columns of the by-account and by-tag tables.
pub const GROUP_COLS: usize = 7;

/// One account's or tag's line (formatted once per journal change).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupRow {
    /// Name, trades, win rate, net, average trade, profit factor, max drawdown.
    pub cells: [String; GROUP_COLS],
    /// Net P&L sign, for its color (its text is signed too); 0 when money isn't shown.
    pub sign: i8,
}

/// The header of a group table.
pub fn group_headers(g: JournalGroup) -> [&'static str; GROUP_COLS] {
    [
        if g == JournalGroup::Tags {
            "Tag"
        } else {
            "Account"
        },
        "Trades",
        "Win",
        "Net",
        "Avg trade",
        "Profit factor",
        "Max drawdown",
    ]
}

/// The word for an account no current list has (a broker not connected yet or gone, an
/// account removed): never assumed to be paper.
pub(crate) const UNLISTED: &str = "UNLISTED";

/// How real a badge's money is, most real first (so a clip or an id clash never favors paper).
fn realness(badge: &str) -> u8 {
    match badge {
        "LIVE" => 0,
        "PAPER" => 3,
        "SIM" => 2,
        _ => 1, // UNLISTED: treated as real until a list says otherwise
    }
}

/// Accounts, least real first: collected into a map, the most real of a clashing id wins.
fn least_real_first(accounts: &[crate::trading::AccountInfo]) -> Vec<&crate::trading::AccountInfo> {
    let mut v: Vec<_> = accounts.iter().collect();
    v.sort_by_key(|a| std::cmp::Reverse(realness(a.badge)));
    v
}

/// Each account's label: its badge in words (PAPER, SIM or LIVE, as the trading header shows
/// it), first so a narrow column cuts the name and never the kind; then its name.
fn account_labels(
    accounts: &[crate::trading::AccountInfo],
) -> std::collections::HashMap<String, String> {
    least_real_first(accounts)
        .into_iter()
        .map(|a| (a.id.0.clone(), format!("{} · {}", a.badge, a.name)))
        .collect()
}

/// Each account's badge (the most real of a clashing id).
pub(crate) fn account_badges(
    accounts: &[crate::trading::AccountInfo],
) -> std::collections::HashMap<String, &'static str> {
    least_real_first(accounts)
        .into_iter()
        .map(|a| (a.id.0.clone(), a.badge))
        .collect()
}

/// An account's label, marked when no current list has it.
fn account_label(names: &std::collections::HashMap<String, String>, id: &str) -> String {
    names
        .get(id)
        .cloned()
        .unwrap_or_else(|| format!("{UNLISTED} · {id}"))
}

/// What the header says the pane's money is: the kinds of the accounts that have trades in
/// the journal (with none yet, of the accounts listed), as words, most real first so a narrow
/// header clips the paper word and never the live one: "PAPER", "LIVE", "LIVE + PAPER",
/// "UNLISTED + PAPER". A PAPER mark means every trade is on a listed paper account: real money
/// never sits under it, and a mixed journal says it is mixed.
fn view_mark<'a>(
    traded: impl Iterator<Item = &'a str>,
    badges: &std::collections::HashMap<String, &'static str>,
) -> Option<String> {
    let mut words: Vec<&str> = traded
        .map(|id| badges.get(id).copied().unwrap_or(UNLISTED))
        .collect();
    if words.is_empty() {
        words = badges.values().copied().collect();
    }
    words.sort_by_key(|w| realness(w));
    words.dedup();
    (!words.is_empty()).then(|| words.join(" + "))
}

/// The pane's change detector: a trade, the display setting or the account list.
fn revision_of(journal: u64, display: u64, accounts: u64) -> u64 {
    journal
        .wrapping_add(display << 32)
        .wrapping_add(accounts << 48)
}

/// P&L per account or per tag: best net first (then by name), untagged last; by name alone
/// when `mode` shows no money (the order would tell the P&L). Money only when `mode` shows
/// currency, like the strip.
pub fn group_rows<'a>(
    trades: impl IntoIterator<Item = &'a Trade>,
    g: JournalGroup,
    names: &std::collections::HashMap<String, String>,
    mode: PnlDisplay,
) -> Vec<GroupRow> {
    let by = match g {
        JournalGroup::Trades => return Vec::new(),
        JournalGroup::Accounts => GroupBy::Account,
        JournalGroup::Tags => GroupBy::Tag,
    };
    let money = |m: Option<Money>| match m {
        Some(m) if mode.money() => money_text(m),
        _ => "—".to_owned(),
    };
    let mut rows: Vec<(bool, Money, GroupRow)> = stats::grouped(trades, by)
        .into_iter()
        .map(|(key, s)| {
            let untagged = g == JournalGroup::Tags && key == stats::UNTAGGED;
            let name = match g {
                _ if untagged => "untagged".to_owned(),
                JournalGroup::Accounts => account_label(names, &key),
                _ => key,
            };
            let row = GroupRow {
                cells: [
                    name,
                    s.count.to_string(),
                    format!("{:.0}%", s.win_rate().unwrap_or(0.0) * 100.0),
                    money(Some(s.net)),
                    money(s.expectancy()),
                    match s.profit_factor() {
                        Some(p) if mode.money() => format!("{p:.2}"),
                        _ => "—".to_owned(),
                    },
                    money(Some(-s.max_drawdown)),
                ],
                sign: if mode.money() {
                    s.net.0.signum() as i8
                } else {
                    0
                },
            };
            (untagged, s.net, row)
        })
        .collect();
    let by_net = mode.money();
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(if by_net {
                b.1.cmp(&a.1)
            } else {
                std::cmp::Ordering::Equal
            })
            .then_with(|| {
                a.2.cells[0]
                    .to_lowercase()
                    .cmp(&b.2.cells[0].to_lowercase())
            })
    });
    rows.into_iter().map(|(_, _, r)| r).collect()
}

/// Which field of the inline editor has the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Tags,
    Note,
}

/// The inline tags/note editor (pure: keys in, an [`Edit`] out).
#[derive(Clone, Debug, PartialEq)]
pub struct Editor {
    pub id: String,
    pub tags: String,
    pub note: String,
    pub field: Field,
}

/// What a key did to the editor.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorKey {
    /// Still editing.
    Typing,
    Save(Edit),
    Cancel,
    /// Not the editor's key.
    Ignored,
}

impl Editor {
    pub fn new(r: &Row) -> Editor {
        Editor {
            id: r.id.clone(),
            tags: r.tags.join(", "),
            note: r.note.clone(),
            field: Field::Tags,
        }
    }

    fn text(&mut self) -> &mut String {
        match self.field {
            Field::Tags => &mut self.tags,
            Field::Note => &mut self.note,
        }
    }

    /// A key (`key` as GPUI names it; `typed`: the character it types, if any).
    pub fn key(&mut self, key: &str, typed: Option<&str>) -> EditorKey {
        match key {
            "enter" => EditorKey::Save(self.edit()),
            "escape" => EditorKey::Cancel,
            "tab" => {
                self.field = match self.field {
                    Field::Tags => Field::Note,
                    Field::Note => Field::Tags,
                };
                EditorKey::Typing
            }
            "backspace" => {
                self.text().pop();
                EditorKey::Typing
            }
            _ => match typed.filter(|c| !c.chars().any(char::is_control)) {
                Some(c) => {
                    let t = self.text();
                    if t.chars().count() < 400 {
                        t.push_str(c);
                    }
                    EditorKey::Typing
                }
                None => EditorKey::Ignored,
            },
        }
    }

    /// Tags split at commas (the journal trims and de-duplicates them).
    pub fn edit(&self) -> Edit {
        Edit {
            id: self.id.clone(),
            tags: self.tags.split(',').map(str::to_owned).collect(),
            note: self.note.trim().to_owned(),
        }
    }
}

pub struct JournalView {
    feed: PaneFeed,
    common: PaneCommon,
    focus: FocusHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    frozen: bool,
    journal: Option<Arc<JournalShared>>,
    seen_revision: u64,
    /// The journal revision, P&L display epoch, account generation and trade date the rows
    /// and stats were built from.
    built: Option<(u64, u64, u64, CivilDate)>,
    /// The current trade date as last known (kept while the store is busy).
    today: Option<CivilDate>,
    /// When `today` was last checked by the poll (about once a second).
    date_checked: Option<std::time::Instant>,
    /// The P&L display setting (read at render).
    mode: PnlDisplay,
    rows: Vec<Row>,
    /// What the table lists, and the group lines when not the trades.
    group: JournalGroup,
    groups: Vec<GroupRow>,
    /// First group line shown.
    group_scroll: usize,
    stats_today: Vec<StatItem>,
    stats_all: Vec<StatItem>,
    /// Account labels by id (the trading core's accounts): PAPER, SIM or LIVE in words, then
    /// the name (paper vs live is always visible, never by color alone; docs/design.md).
    names: std::collections::HashMap<String, String>,
    /// Each listed account's badge, for the header's mark.
    badges: std::collections::HashMap<String, &'static str>,
    /// What the header says the pane's money is ([`view_mark`]).
    mark: Option<String>,
    /// The account list's generation `names` and `badges` were read at: read again on any
    /// change (a broker joins, a kind or name changes).
    accounts_seen: u64,
    all_dates: bool,
    selected: Option<String>,
    /// First row shown.
    scroll: usize,
    editor: Option<Editor>,
    glyphs: Glyphs,
    cols: Columns,
    bounds: Bounds<Pixels>,
}

impl EventEmitter<FeedChanged> for JournalView {}
impl EventEmitter<ColumnMenu> for JournalView {}

impl Pane for JournalView {
    fn store(&self) -> &Arc<ChartStore> {
        &self.store
    }

    fn feed(&self) -> &PaneFeed {
        &self.feed
    }

    fn common(&mut self) -> &mut PaneCommon {
        &mut self.common
    }

    fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn save_state(&self, doc: &mut PaneDoc) {
        let columns = self.cols.save(&JournalCol::ALL);
        doc.journal = (self.all_dates || !columns.is_empty() || !self.group.is_trades()).then_some(
            JournalState {
                all_dates: self.all_dates,
                columns,
                group: self.group,
            },
        );
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    /// A new trade date (checked about once a second) redraws the stats strip.
    fn changed(&mut self) -> bool {
        if self
            .date_checked
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1))
        {
            return false;
        }
        self.date_checked = Some(std::time::Instant::now());
        let before = self.today;
        before.is_some() && self.today() != before
    }

    /// The journal's revision (with the P&L display setting's and the account list's), not
    /// the market's: the pane repaints when a trade, the display or an account changes.
    fn revision(&self) -> u64 {
        revision_of(
            self.journal.as_ref().map_or(0, |j| j.revision()),
            pnl::display_epoch(),
            crate::trading::accounts_epoch(),
        )
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.store = feed.store().clone();
        self.feed = feed;
        self.built = None;
        cx.notify();
    }
}

impl JournalView {
    /// A new look (the Settings window's appearance).
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }
    pub fn new(feed: PaneFeed, theme: Theme, frozen: bool, cx: &mut Context<Self>) -> Self {
        let store = feed.store().clone();
        pane::watch_store(cx);
        JournalView {
            feed,
            common: PaneCommon::default(),
            focus: cx.focus_handle(),
            store,
            theme,
            frozen,
            journal: Trading::try_global(cx).map(|t| t.journal.clone()),
            seen_revision: u64::MAX,
            built: None,
            today: None,
            date_checked: None,
            mode: PnlDisplay::default(),
            rows: Vec::new(),
            group: JournalGroup::Trades,
            groups: Vec::new(),
            group_scroll: 0,
            stats_today: Vec::new(),
            stats_all: Vec::new(),
            names: Default::default(),
            badges: Default::default(),
            mark: None,
            accounts_seen: u64::MAX,
            all_dates: false,
            selected: None,
            scroll: 0,
            editor: None,
            glyphs: Glyphs::new(TEXT_PX),
            cols: Columns::new(Table::Journal),
            bounds: Bounds::default(),
        }
    }

    pub fn restore(&mut self, state: &JournalState) {
        self.all_dates = state.all_dates;
        self.group = state.group;
        self.cols.load(&JournalCol::ALL, &state.columns);
    }

    pub fn group(&self) -> JournalGroup {
        self.group
    }

    /// List trades, or P&L by account or by tag.
    pub fn set_group(&mut self, g: JournalGroup, cx: &mut Context<Self>) {
        if self.group != g {
            self.group = g;
            self.group_scroll = 0;
            self.editor = None;
            self.built = None;
            cx.notify();
        }
    }

    pub fn column_command(
        &mut self,
        cmd: ColumnCmd,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        self.cols.apply(cmd)?;
        cx.notify();
        Ok(())
    }

    pub fn columns(&self) -> &Columns {
        &self.cols
    }

    pub fn all_dates(&self) -> bool {
        self.all_dates
    }

    /// Stats over all dates, or the current trade date.
    pub fn toggle_all_dates(&mut self, cx: &mut Context<Self>) {
        self.all_dates = !self.all_dates;
        // The group lines cover the same dates.
        self.built = None;
        cx.notify();
    }

    /// Edit the selected trade's tags and note (else the newest trade's).
    pub fn start_edit(&mut self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        if !self.group.is_trades() {
            return Err("switch the journal to Trades to edit a trade");
        }
        let i = self
            .selected_index()
            .or((!self.rows.is_empty()).then_some(0));
        let r = i
            .and_then(|i| self.rows.get(i))
            .ok_or("no trades to edit yet")?;
        self.selected = Some(r.id.clone());
        self.editor = Some(Editor::new(r));
        cx.notify();
        Ok(())
    }

    /// Open the selected trade's trade snapshot in the OS's default app.
    pub fn open_selected_chart(&self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        if !self.group.is_trades() {
            return Err("switch the journal to Trades to open a trade's snapshot");
        }
        let r = self
            .selected_index()
            .and_then(|i| self.rows.get(i))
            .ok_or("select a trade in the journal first")?;
        self.open_chart(r.shot.as_deref(), cx)
    }

    fn open_chart(&self, file: Option<&str>, cx: &mut Context<Self>) -> Result<(), &'static str> {
        let dir = Trading::try_global(cx).and_then(|t| t.shots_dir.clone());
        let path = crate::journal_shots::locate(dir.as_deref(), file)?;
        cx.open_url(&crate::journal_shots::file_url(&path));
        Ok(())
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.rows.iter().position(|r| &r.id == id)
    }

    /// The current trade date of the pane's market: on a replay's (or a screenshot's)
    /// market time, else the wall clock.
    ///
    /// Read each render (so a new session shows without waiting for a trade); if the store is
    /// busy this instant, the last known date stands (never a UTC guess).
    fn today(&mut self) -> Option<CivilDate> {
        let replay =
            self.frozen || matches!(self.feed.config.source, lines_feed::Source::Replay { .. });
        if let Some(d) = self.store.try_read() {
            let now = d
                .market_time
                .filter(|_| replay)
                .unwrap_or_else(lines_feed::now_utc);
            self.today = Some(lines_feed::disk::partition::trade_date(&d.product, now));
        }
        self.today
    }

    /// Read the accounts' labels and badges again when the account list changed (a broker
    /// connects after the pane opens, a kind or name changes): never a stale label on P&L.
    /// The tick repaints on the same generation ([`Self::revision`]).
    fn refresh_accounts(&mut self, cx: &App) {
        let epoch = crate::trading::accounts_epoch();
        if epoch == self.accounts_seen {
            return;
        }
        let Some(t) = Trading::try_global(cx) else {
            return;
        };
        self.accounts_seen = epoch;
        let accounts = t.accounts();
        self.names = account_labels(&accounts);
        self.badges = account_badges(&accounts);
    }

    /// Rebuild rows and stats if the journal moved. Returns false if the journal was busy
    /// this instant (try again on the next poll).
    fn rebuild(&mut self) -> bool {
        let Some(j) = self.journal.clone() else {
            return true;
        };
        let Some(today) = self.today() else {
            return false; // the store hasn't been readable yet
        };
        let rev = (
            j.revision(),
            pnl::display_epoch(),
            self.accounts_seen,
            today,
        );
        if self.built == Some(rev) {
            return true;
        }
        // A pointer clone; everything below runs without any lock.
        let Some(snap) = j.snapshot() else {
            return false;
        };
        let mode = self.mode;
        // New trades go on top: while scrolled, keep the same rows in view (nothing the
        // trader aims at shifts under the pointer, docs/design.md §7).
        let added = snap.trades.len().saturating_sub(self.rows.len());
        if self.scroll > 0 && self.built.is_some() {
            self.scroll += added;
        }
        let names = &self.names;
        self.rows = snap
            .trades
            .iter()
            .rev()
            .map(|t| {
                let mut r = row(t, mode);
                r.cells[JournalCol::Account.index()] = account_label(names, &t.account.0);
                r
            })
            .collect();
        self.mark = view_mark(
            snap.trades.iter().map(|t| t.account.0.as_str()),
            &self.badges,
        );
        self.stats_today = stats_items(
            &format!("Today {today}"),
            &Stats::of(snap.trades.iter().filter(|t| t.trade_date == today)),
            snap.open,
            mode,
        );
        self.stats_all = stats_items("All dates", &Stats::of(&snap.trades), snap.open, mode);
        let all = self.all_dates;
        self.groups = group_rows(
            snap.trades.iter().filter(|t| all || t.trade_date == today),
            self.group,
            names,
            mode,
        );
        // Fewer lines than before (another date range, a tag gone): keep the first in view.
        self.group_scroll = self.group_scroll.min(self.groups.len().saturating_sub(1));
        self.built = Some(rev);
        true
    }

    fn table_xy(&self, p: Point<Pixels>) -> (f32, f32) {
        (f(p.x - self.bounds.origin.x), f(p.y - self.bounds.origin.y))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if !self.group.is_trades() {
            return;
        }
        let (x, y) = self.table_xy(ev.position);
        match self.cols.hit(x, y, ev.click_count) {
            HeaderHit::Divider(d) => {
                self.cols.start_drag(d, x);
                cx.stop_propagation();
                cx.notify();
            }
            HeaderHit::AutoFit(c) => {
                let _ = self.column_command(ColumnCmd::AutoFit(c), cx);
                cx.stop_propagation();
            }
            HeaderHit::Row => {}
            HeaderHit::Outside => {
                let i = ((y - HEADER_ROW_H) / ROW_H).floor();
                if i >= 0.0
                    && let Some(r) = self.rows.get(self.scroll + i as usize).cloned()
                {
                    self.selected = Some(r.id.clone());
                    let on_chart = r.shot.is_some()
                        && self
                            .cols
                            .cell(JournalCol::Chart.index(), 0.0)
                            .is_some_and(|(cx0, cw)| x >= cx0 && x < cx0 + cw);
                    if on_chart {
                        // The first click of a double opens it, once; the editor stays shut.
                        if ev.click_count == 1
                            && let Err(e) = self.open_chart(r.shot.as_deref(), cx)
                        {
                            // Deferred: a notice redraws windows, this one being updated now.
                            cx.defer(move |cx| crate::workspace::notice(e.to_owned(), cx));
                        }
                    } else if ev.click_count >= 2 {
                        self.editor = Some(Editor::new(&r));
                    }
                    cx.notify();
                }
            }
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.cols.drag.is_none() {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.cols.drag = None;
            cx.notify();
            return;
        }
        let (x, _) = self.table_xy(ev.position);
        if let Some(cmd) = self.cols.drag_to(x) {
            let _ = self.column_command(cmd, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.cols.drag.take().is_some() {
            cx.notify();
        }
    }

    fn on_right_down(&mut self, ev: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.group.is_trades() {
            return;
        }
        let (x, y) = self.table_xy(ev.position);
        if self.cols.hit(x, y, 1) != HeaderHit::Outside {
            cx.emit(ColumnMenu(ev.position));
            cx.stop_propagation();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if let Some(ed) = &mut self.editor {
            // AltGr (Ctrl+Alt on Windows) types characters such as @ { €; other modified
            // keys are commands and still work while editing.
            let alt_gr = m.control && m.alt && k.key_char.is_some();
            if (m.control || m.alt || m.platform) && !alt_gr {
                return;
            }
            match ed.key(k.key.as_str(), k.key_char.as_deref()) {
                EditorKey::Typing => {}
                EditorKey::Cancel => self.editor = None,
                EditorKey::Save(e) => {
                    self.editor = None;
                    if let Some(j) = &self.journal {
                        j.edit(e);
                    }
                }
                EditorKey::Ignored => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.control || m.alt || m.platform || !self.group.is_trades() {
            return;
        }
        let n = self.rows.len();
        let i = self.selected_index();
        let next = match k.key.as_str() {
            "down" => Some(i.map_or(0, |i| (i + 1).min(n.saturating_sub(1)))),
            "up" => Some(i.map_or(0, |i| i.saturating_sub(1))),
            "enter" => {
                let _ = self.start_edit(cx);
                cx.stop_propagation();
                return;
            }
            _ => None,
        };
        let Some(next) = next.filter(|_| n > 0) else {
            return;
        };
        self.selected = Some(self.rows[next].id.clone());
        let visible = self.visible_rows();
        if next < self.scroll {
            self.scroll = next;
        } else if next >= self.scroll + visible {
            self.scroll = next + 1 - visible;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &gpui::ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dy = f(ev.delta.pixel_delta(window.line_height()).y);
        let rows = (dy / ROW_H).round() as i64;
        if rows != 0 {
            let (scroll, n) = if self.group.is_trades() {
                (&mut self.scroll, self.rows.len())
            } else {
                (&mut self.group_scroll, self.groups.len())
            };
            let max = n.saturating_sub(1) as i64;
            *scroll = (*scroll as i64 - rows).clamp(0, max) as usize;
            cx.notify();
        }
    }

    fn visible_rows(&self) -> usize {
        (((f(self.bounds.size.height) - HEADER_ROW_H) / ROW_H).floor() as usize).max(1)
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        window.paint_quad(fill(bounds, t.bg));
        if !self.group.is_trades() {
            self.paint_groups(bounds, hair, window, cx);
            return;
        }
        self.cols.measure_headers(&mut self.glyphs, &t, PAD, window);
        let rows = self.visible_rows();
        let end = (self.scroll + rows).min(self.rows.len());
        for i in self.scroll..end {
            for (c, col) in JournalCol::ALL.iter().enumerate() {
                if *col == JournalCol::Tags {
                    continue; // fills; never widens the table
                }
                let tw = self
                    .glyphs
                    .width(&self.rows[i].cells[c], t.text_main, false, window);
                self.cols.need(c, tw, PAD);
            }
        }
        // A narrow pane gives up the least needed columns first (never half a number).
        self.cols.layout_journal(w);
        let left: [bool; 13] = JournalCol::ALL.map(JournalCol::left_aligned);
        self.cols
            .paint_header(&mut self.glyphs, &t, bounds, PAD, &left, window, cx);
        let oy = top + HEADER_ROW_H;
        if self.rows.is_empty() {
            let msg = if self.journal.is_some() {
                "No closed trades yet. Trades appear here when a position goes back to flat."
            } else {
                "Trading isn't running."
            };
            self.glyphs.paint(
                msg,
                ox + PAD,
                oy + 4.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }
        let selected = self.selected_index();
        for (n, i) in (self.scroll..end).enumerate() {
            let y = oy + n as f32 * ROW_H;
            if y + ROW_H > top + h + 0.5 {
                break;
            }
            let r = &self.rows[i];
            for (c, col) in JournalCol::ALL.iter().enumerate() {
                let Some((x, cw)) = self.cols.cell(c, ox) else {
                    continue;
                };
                let color = match col {
                    JournalCol::Pnl if r.sign > 0 => t.up,
                    JournalCol::Pnl if r.sign < 0 => t.down,
                    JournalCol::Time | JournalCol::Duration | JournalCol::MaeMfe => t.text_dim,
                    JournalCol::Chart if r.shot.is_none() => t.text_dim,
                    _ => t.text_main,
                };
                let text = r.cells[c].as_str();
                let bold = *col == JournalCol::Pnl;
                let cell = Bounds::new(point(px(x), px(y)), size(px(cw), px(ROW_H)));
                window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                    if col.left_aligned() {
                        self.glyphs
                            .paint(text, x + PAD, y, ROW_H, color, bold, window, cx);
                    } else {
                        self.glyphs.paint_right(
                            text,
                            x + cw - PAD,
                            y,
                            ROW_H,
                            color,
                            bold,
                            window,
                            cx,
                        );
                    }
                });
            }
            if Some(i) == selected {
                // Selection: an accent outline (no fill, so text contrast is untouched).
                for (yy, hh) in [(y, hair * 2.0), (y + ROW_H - hair * 2.0, hair * 2.0)] {
                    window.paint_quad(fill(
                        Bounds::new(point(px(ox), px(yy)), size(px(w), px(hh))),
                        t.accent,
                    ));
                }
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(y)), size(px(3.0), px(ROW_H))),
                    t.accent,
                ));
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + ROW_H - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        let body = Bounds::new(point(px(ox), px(oy)), size(px(w), px(h - HEADER_ROW_H)));
        self.cols.paint_rules(&t, body, window);
    }

    /// The by-account or by-tag table: the name left, numbers right; columns that don't fit
    /// whole are left out from the right (never part of a number).
    fn paint_groups(
        &mut self,
        bounds: Bounds<Pixels>,
        hair: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        let heads = group_headers(self.group);
        let mut widths = [0.0f32; GROUP_COLS];
        for (c, head) in heads.iter().enumerate() {
            widths[c] = self.glyphs.width(head, t.text_dim, false, window);
        }
        for r in &self.groups {
            for (c, (wc, cell)) in widths.iter_mut().zip(&r.cells).enumerate() {
                *wc = wc.max(self.glyphs.width(cell, t.text_main, c == 3, window));
            }
        }
        for wc in &mut widths {
            *wc += PAD * 4.0;
        }
        // The name column always shows (cut to fit if it must, at least 80 px); number
        // columns that don't fit whole are left out from the right.
        let numbers: f32 = widths[1..].iter().sum();
        widths[0] = widths[0].min((w - numbers).max(80.0)).min(w);
        let mut shown = 1;
        let mut used = widths[0];
        for wc in &widths[1..] {
            if used + wc > w {
                break;
            }
            used += wc;
            shown += 1;
        }
        window.paint_quad(fill(
            Bounds::new(point(px(ox), px(top)), size(px(w), px(HEADER_ROW_H))),
            t.panel,
        ));
        let mut x = ox;
        for (c, (head, &cw)) in heads.iter().zip(&widths).enumerate().take(shown) {
            if c == 0 {
                self.glyphs.paint(
                    head,
                    x + PAD,
                    top,
                    HEADER_ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            } else {
                self.glyphs.paint_right(
                    head,
                    x + cw - PAD,
                    top,
                    HEADER_ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            x += cw;
        }
        let oy = top + HEADER_ROW_H;
        if self.groups.is_empty() {
            let msg = if self.journal.is_some() {
                "No closed trades in these dates."
            } else {
                "Trading isn't running."
            };
            self.glyphs.paint(
                msg,
                ox + PAD,
                oy + 4.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }
        let start = self.group_scroll.min(self.groups.len() - 1);
        let mut end_y = oy;
        for (n, r) in self.groups[start..].iter().enumerate() {
            let y = oy + n as f32 * ROW_H;
            if y + ROW_H > top + h + 0.5 {
                break;
            }
            end_y = y + ROW_H;
            let mut x = ox;
            for (c, (cell, &cw)) in r.cells.iter().zip(&widths).enumerate().take(shown) {
                let color = match c {
                    3 if r.sign > 0 => t.up,
                    3 if r.sign < 0 => t.down,
                    _ => t.text_main,
                };
                if c == 0 {
                    // A long name is cut at its column, never over the numbers.
                    let cell_b = Bounds::new(point(px(x), px(y)), size(px(cw), px(ROW_H)));
                    window.with_content_mask(
                        Some(gpui::ContentMask { bounds: cell_b }),
                        |window| {
                            self.glyphs
                                .paint(cell, x + PAD, y, ROW_H, color, false, window, cx);
                        },
                    );
                } else {
                    self.glyphs.paint_right(
                        cell,
                        x + cw - PAD,
                        y,
                        ROW_H,
                        color,
                        c == 3,
                        window,
                        cx,
                    );
                }
                x += cw;
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + ROW_H - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        // By tag: say why the lines can add up to more than the strip.
        if self.group == JournalGroup::Tags && end_y + ROW_H <= top + h + 0.5 {
            self.glyphs.paint(
                "A trade with several tags counts under each of them.",
                ox + PAD,
                end_y + 2.0,
                ROW_H,
                t.text_dim,
                false,
                window,
                cx,
            );
        }
    }

    /// Trades / By account / By tag: the current one underlined and bold (never color alone).
    fn group_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .flex()
            .flex_row()
            .gap_2()
            .children(JournalGroup::ALL.into_iter().map(|g| {
                let on = g == self.group;
                div()
                    .id(g.label())
                    .px_1()
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(if on {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if on { t.text_main } else { t.text_dim })
                    .when(on, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                    .child(g.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.set_group(g, cx);
                        }),
                    )
            }))
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let right = match &self.journal {
            Some(j) if j.persistent() => "saved",
            Some(_) => "not saved: this run only",
            None => "",
        };
        let mark = self.mark.as_deref().filter(|_| self.journal.is_some());
        div()
            .flex()
            .flex_row()
            .justify_between()
            .gap_2()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(pane::controls_gutter(crate::layout::PaneKind::Journal)))
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(
                pane::header_content()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .children(pane::link_chip(self.common.link, &t))
                            .child(div().text_color(t.text_main).child("Journal"))
                            // What the money here is, in words, as everywhere else
                            // (docs/design.md §3): the PAPER chip only when every trade is on a
                            // paper account.
                            .children(mark.map(|m| {
                                let one = |w| crate::trade_bar::account_badge(w, &t);
                                match m {
                                    "PAPER" => one("PAPER").into_any_element(),
                                    "SIM" => one("SIM").into_any_element(),
                                    "LIVE" => one("LIVE").into_any_element(),
                                    // Mixed or unlisted: outlined like LIVE, never the paper chip.
                                    _ => div()
                                        .flex_shrink_0()
                                        .px_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(t.text_main)
                                        .text_color(t.text_main)
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .relative()
                                        .children(crate::layout_rec::text_marker(m))
                                        .child(m.to_owned())
                                        .into_any_element(),
                                }
                            }))
                            .child(self.group_tabs(cx)),
                    )
                    .child(div().text_color(t.text_dim).child(right)),
            )
    }

    fn strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let items = if self.all_dates {
            &self.stats_all
        } else {
            &self.stats_today
        };
        // Whole items on one line: those that don't fit wrap onto a second line, which the
        // fixed height hides. Never part of a number.
        let line = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .h(px(16.0))
            .overflow_hidden()
            .children(items.iter().enumerate().map(|(i, it)| {
                let color = match it.tone {
                    1 => t.up,
                    -1 => t.down,
                    _ => t.text_main,
                };
                div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(color)
                    .when(it.strong, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                    .child(if i == 0 {
                        it.text.clone()
                    } else {
                        format!("  ·  {}", it.text)
                    })
            }));
        div()
            .id("journal-stats")
            .px_2()
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .text_color(t.text_main)
            .cursor_pointer()
            .child(line)
            // A click switches between the current trade date and all dates.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle_all_dates(cx);
                }),
            )
    }

    fn editor_bar(&self) -> Option<impl IntoElement> {
        let ed = self.editor.as_ref()?;
        let t = self.theme;
        let field = |label: &'static str, text: &str, active: bool| {
            div()
                .flex()
                .flex_row()
                .gap_1()
                .child(div().text_color(t.text_dim).child(label))
                .child(
                    div()
                        .px_1()
                        .min_w(px(80.0))
                        .border_1()
                        .border_color(if active { t.accent } else { t.axis_line })
                        .text_color(t.text_main)
                        .child(if active {
                            format!("{text}▏")
                        } else {
                            text.to_owned()
                        }),
                )
        };
        Some(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .items_center()
                .px_2()
                .py_1()
                .bg(t.panel)
                .border_t_1()
                .border_color(t.axis_line)
                .text_size(px(12.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .child(field("Tags", &ed.tags, ed.field == Field::Tags))
                .child(field("Note", &ed.note, ed.field == Field::Note))
                .child(
                    div()
                        .text_color(t.text_dim)
                        .child("Tab: next field  ·  Enter: save  ·  Esc: cancel"),
                ),
        )
    }
}

impl JournalView {
    /// Taking typed text now (a note being edited): the workspace leaves drawing keys to it
    /// (`commands::runs_while_typing`).
    pub fn takes_text(&self) -> bool {
        self.editor.is_some()
    }
}

impl Render for JournalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_accounts(cx);
        let mode = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default();
        if mode != self.mode {
            self.mode = mode;
            self.built = None;
        }
        if !self.rebuild() {
            // The trading thread held the journal this instant: draw again on the next poll.
            self.seen_revision = u64::MAX;
        }
        let entity = cx.entity();
        let t = self.theme;
        let e = entity.clone();
        let body = canvas(
            move |bounds, window, cx| e.read(cx).cols.divider_hitboxes(bounds, window),
            move |bounds, hitboxes, window, cx| {
                pane_columns::set_divider_cursors(&hitboxes, window);
                entity.update(cx, |view, cx| view.paint(bounds, window, cx));
            },
        )
        .size_full();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header(cx))
            .child(self.strip(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll(cx.listener(Self::on_scroll))
                    .child(body),
            )
            .children(self.editor_bar())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{ContractKey, Month, Price, Qty};
    use lines_feed::catalog;
    use lines_trading::journal::{Journal, Record};
    use lines_trading::{AccountId, Event, FillEvent, OrderId};

    /// Long 4 ES in three fills, out in two: net +310.02 after $2.48 of fees.
    fn trade() -> Trade {
        let es = ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap());
        let t0 = UtcMillis::parse_iso("2026-11-05T14:31:07Z").unwrap().0;
        let mut j = Journal::new();
        let mut out = Vec::new();
        for (i, (side, p, q, fee_cents, s)) in [
            (Side::Buy, 600_000, 2, 62, 0),
            (Side::Buy, 600_025, 1, 31, 1),
            (Side::Buy, 599_950, 1, 31, 30),
            (Side::Sell, 600_200, 2, 62, 90),
            (Side::Sell, 600_100, 2, 62, 120),
        ]
        .into_iter()
        .enumerate()
        {
            out.extend(j.on_event(&Event::Fill(FillEvent {
                account: AccountId::new("paper"),
                order: OrderId(1),
                contract: es,
                side,
                price: Price(p),
                qty: Qty(q),
                fee: Money::cents(fee_cents),
                realised: Money::ZERO,
                trade_id: format!("f{i}"),
                reconciled: false,
                at: UtcMillis(t0 + s * 1000),
            })));
        }
        let mut t = out.pop().expect("one round trip");
        t.tags = vec!["breakout".into(), "A+".into()];
        t
    }

    #[test]
    fn rows_are_signed_and_readable() {
        let r = row(&trade(), PnlDisplay::Both);
        let c = |col: JournalCol| r.cells[col.index()].as_str();
        assert_eq!(c(JournalCol::Time), "11-05 14:31:07");
        assert_eq!(c(JournalCol::Contract), "ESZ26");
        assert_eq!(c(JournalCol::Side), "Long");
        assert_eq!(c(JournalCol::Qty), "4");
        assert_eq!(c(JournalCol::Entry), "5999.9375");
        assert_eq!(c(JournalCol::Exit), "6001.50");
        assert_eq!(c(JournalCol::Pnl), "+$310.02");
        // The header says what unit: `Ticks/ct`.
        assert_eq!(c(JournalCol::Ticks), "+6.3");
        assert_eq!(c(JournalCol::Duration), "2m 00s");
        // Deliberately changed: this was `-7.0 t / +33.0 t` (the position's dollars over one
        // contract's $12.50), which shared a row and a unit with the Ticks column's per-contract
        // number, and disagreed with the CSV. Now every tick number is per contract, and these
        // still reconcile: 4 contracts × 8.25 t × $12.50 is the +$412.50.
        assert_eq!(
            c(JournalCol::MaeMfe),
            "-$87.50 / +$412.50  (-1.8 / +8.3 t/ct)"
        );
        assert_eq!(c(JournalCol::Tags), "breakout, A+");
        assert_eq!(r.sign, 1);
        assert_eq!(duration_text(45_000), "45s");
        assert_eq!(duration_text(3_780_000), "1h 03m");
        assert_eq!(decimal(6001.0, 100), "6001.00");
        assert_eq!(decimal(0.5, 1), "0.5");
    }

    /// A product not in the catalog has no tick to state: dashes for its excursions' ticks, not
    /// raw price units called ticks.
    #[test]
    fn an_unlisted_product_states_no_excursion_ticks() {
        let mut t = trade();
        t.contract.product = lines_core::ProductId(u32::MAX);
        let cells = row(&t, PnlDisplay::Both).cells;
        assert_eq!(
            cells[JournalCol::MaeMfe.index()],
            "-$87.50 / +$412.50  (— / —)"
        );
    }

    /// One basis in the pane and the export: the cells' ticks are the CSV's numbers (its
    /// `mae_ticks` is a magnitude, the cell is signed), and the header says per contract.
    #[test]
    fn pane_and_csv_ticks_share_one_basis() {
        let t = trade();
        let export = lines_trading::journal::csv(std::slice::from_ref(&t));
        let mut lines = export.lines();
        let names: Vec<&str> = lines.next().unwrap().split(',').collect();
        let values: Vec<&str> = lines.next().unwrap().split(',').collect();
        let field = |name: &str| {
            let i = names.iter().position(|n| *n == name).unwrap();
            values[i].parse::<f64>().unwrap()
        };
        assert_eq!(field("ticks"), 6.25);
        assert_eq!(field("mae_ticks"), 1.75);
        assert_eq!(field("mfe_ticks"), 8.25);
        let cells = row(&t, PnlDisplay::Both).cells;
        assert_eq!(cells[JournalCol::Ticks.index()], tick_number(6.25));
        assert!(
            cells[JournalCol::MaeMfe.index()].ends_with(&format!(
                "({} / {})",
                tick_number(-1.75),
                per_contract_text(8.25)
            )),
            "{}",
            cells[JournalCol::MaeMfe.index()]
        );
        assert_eq!(JournalCol::Ticks.header(), "Ticks/ct");
        assert_eq!(JournalCol::Ticks.label(), "Ticks per contract");
    }

    /// The Chart cell: "view" with a file, a dash without (the reverse state).
    #[test]
    fn the_chart_cell_says_whether_there_is_a_file() {
        let mut t = trade();
        let r = row(&t, PnlDisplay::Off);
        assert_eq!(r.cells[JournalCol::Chart.index()], "—");
        assert_eq!(r.shot, None);
        t.screenshots = vec!["2026-11-05-0123456789abcdef.svg".into()];
        let r = row(&t, PnlDisplay::Off);
        assert_eq!(r.cells[JournalCol::Chart.index()], "view");
        assert_eq!(r.shot.as_deref(), Some(t.screenshots[0].as_str()));
    }

    /// P&L display (`crate::pnl`): off shows no money or tick number anywhere; ticks or
    /// currency only that unit. The sign is always written; with P&L off nothing is colored.
    #[test]
    fn pnl_display_decides_what_shows() {
        let t = trade();
        let cells = |m| row(&t, m).cells;
        let off = cells(PnlDisplay::Off);
        for col in [JournalCol::Pnl, JournalCol::Ticks, JournalCol::MaeMfe] {
            assert_eq!(off[col.index()], "—", "{col:?}");
        }
        assert_eq!(row(&t, PnlDisplay::Off).sign, 0, "nothing colored");
        assert_eq!(off[JournalCol::Entry.index()], "5999.9375", "prices stay");
        let ticks = cells(PnlDisplay::Ticks);
        assert_eq!(ticks[JournalCol::Pnl.index()], "+6.3 t/ct");
        assert_eq!(ticks[JournalCol::Ticks.index()], "+6.3");
        assert_eq!(ticks[JournalCol::MaeMfe.index()], "-1.8 / +8.3 t/ct");
        // In ticks mode the color follows the ticks shown, not the money after fees.
        let mut scratch = t.clone();
        scratch.exit_value = scratch.entry_value; // exit = entry: 0.0 t, a loss after fees
        scratch.realized = Money::ZERO;
        assert_eq!(row(&scratch, PnlDisplay::Ticks).sign, 0);
        assert_eq!(row(&scratch, PnlDisplay::Currency).sign, -1);
        let cur = cells(PnlDisplay::Currency);
        assert_eq!(cur[JournalCol::Pnl.index()], "+$310.02");
        assert_eq!(cur[JournalCol::Ticks.index()], "—");
        assert_eq!(cur[JournalCol::MaeMfe.index()], "-$87.50 / +$412.50");
    }

    #[test]
    fn stats_strip_reads_like_a_sentence() {
        let j = Journal::from_records([Record::Trade(Box::new(trade()))]);
        let s = j.stats(|_| true);
        assert_eq!(
            stats_text("Today 2026-11-05", &s, 1, PnlDisplay::Both),
            "Today 2026-11-05: 1 trade  ·  net +$310.02  ·  max drawdown $0.00  ·  win 100%  ·  avg win +$310.02  ·  avg loss —  ·  expectancy +$310.02  ·  profit factor —  ·  1 win in a row  ·  1 open"
        );
        // Net P&L is the loudest item, colored by its sign (and signed).
        let items = stats_items("Today", &s, 0, PnlDisplay::Both);
        assert!(items[1].strong && items[1].tone == 1);
        assert_eq!(
            stats_text("Today 2026-11-05", &s, 0, PnlDisplay::Off),
            "Today 2026-11-05: 1 trade  ·  win 100%  ·  1 win in a row"
        );
        assert_eq!(
            stats_text("All dates", &Stats::default(), 0, PnlDisplay::Both),
            "All dates: 0 trades"
        );
    }

    /// P&L by account and by tag (best net first, untagged last; a trade with two
    /// tags counts in each); money only in a currency mode.
    #[test]
    fn groups_by_account_and_tag() {
        let win = trade(); // +$310.02, tags breakout and A+, account "paper"
        let mut loss = win.clone();
        loss.id = "l".into();
        loss.realized = Money::ZERO; // −$2.48 of fees
        loss.tags = vec!["fade".into()];
        loss.account = lines_trading::AccountId::new("eval");
        let mut plain = loss.clone();
        plain.id = "p".into();
        plain.tags.clear();
        let all = [win, loss, plain];
        let names: std::collections::HashMap<String, String> =
            [("eval".to_owned(), "Paper 50K eval".to_owned())].into();
        let by_account = group_rows(&all, JournalGroup::Accounts, &names, PnlDisplay::Both);
        let cells = |r: &GroupRow| r.cells.clone();
        assert_eq!(
            cells(&by_account[1]),
            [
                "Paper 50K eval",
                "2",
                "0%",
                "-$4.96",
                "-$2.48",
                "0.00",
                "-$4.96"
            ]
            .map(String::from)
        );
        assert_eq!(by_account[1].sign, -1);
        assert_eq!(
            cells(&by_account[0]),
            [
                "UNLISTED · paper",
                "1",
                "100%",
                "+$310.02",
                "+$310.02",
                "—",
                "$0.00"
            ]
            .map(String::from),
            "an account no list has says so, with its id"
        );
        let by_tag = group_rows(&all, JournalGroup::Tags, &names, PnlDisplay::Both);
        let order: Vec<&str> = by_tag.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(order, ["A+", "breakout", "fade", "untagged"]);
        assert_eq!(by_tag[0].cells[3], "+$310.02");
        let off = group_rows(&all, JournalGroup::Tags, &names, PnlDisplay::Off);
        assert!(off.iter().all(|r| r.sign == 0));
        assert!(
            off.iter().all(|r| r.cells[3..].iter().all(|c| c == "—")),
            "no money"
        );
        assert_eq!(off[0].cells[1..3], ["1".to_owned(), "100%".to_owned()]);
        // With money hidden the order is by name, so it doesn't tell the P&L either.
        let off_accounts = group_rows(&all, JournalGroup::Accounts, &names, PnlDisplay::Off);
        let order: Vec<&str> = off_accounts.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(order, ["Paper 50K eval", "UNLISTED · paper"]);
        assert!(group_rows(&all, JournalGroup::Trades, &names, PnlDisplay::Both).is_empty());
        // Each account says paper or live in words (a broker account next to paper ones).
        let account = |id: &str, name: &str, paper: bool, badge| crate::trading::AccountInfo {
            id: AccountId::new(id),
            name: name.into(),
            paper,
            rules: None,
            option_guard: Default::default(),
            venue: (!paper).then_some(0),
            badge,
            read_only: None,
        };
        let labels = account_labels(&[
            account("sim", "Paper 50K eval", true, "PAPER"),
            account("ext", "Eval", false, "LIVE"),
        ]);
        assert_eq!(labels["sim"], "PAPER · Paper 50K eval");
        assert_eq!(labels["ext"], "LIVE · Eval");
        assert_eq!(account_label(&labels, "gone"), "UNLISTED · gone");
        // The header's mark: PAPER only when every trade is on a listed paper account.
        let badges: std::collections::HashMap<String, &'static str> = [
            ("sim".to_owned(), "PAPER"),
            ("ext".to_owned(), "LIVE"),
            ("demo".to_owned(), "SIM"),
        ]
        .into();
        let mark = |ids: &[&'static str]| view_mark(ids.iter().copied(), &badges);
        assert_eq!(mark(&["sim", "sim"]).as_deref(), Some("PAPER"));
        assert_eq!(mark(&["ext"]).as_deref(), Some("LIVE"));
        // Most real first: a narrow header clips the paper word, never the live one.
        assert_eq!(mark(&["sim", "ext"]).as_deref(), Some("LIVE + PAPER"));
        assert_eq!(mark(&["sim", "demo"]).as_deref(), Some("SIM + PAPER"));
        assert_eq!(
            mark(&["sim", "demo", "ext"]).as_deref(),
            Some("LIVE + SIM + PAPER")
        );
        // After a restart the list is paper only, while the file holds a broker's trades.
        let paper_only: std::collections::HashMap<String, &'static str> =
            [("sim".to_owned(), "PAPER")].into();
        assert_eq!(
            view_mark(["sim", "venue-01"].into_iter(), &paper_only).as_deref(),
            Some("UNLISTED + PAPER"),
            "real money never under a PAPER mark"
        );
        assert_eq!(
            view_mark(["venue-01"].into_iter(), &paper_only).as_deref(),
            Some("UNLISTED")
        );
        // No trades yet: what's listed; nothing listed: no mark.
        assert_eq!(
            view_mark(std::iter::empty(), &paper_only).as_deref(),
            Some("PAPER")
        );
        assert_eq!(view_mark(std::iter::empty(), &Default::default()), None);
        // Any change to the account list repaints the pane, even with the count unchanged.
        assert_ne!(revision_of(7, 1, 0), revision_of(7, 1, 1));
        // An id two venues share reads as the more real of the two, whatever the list's order.
        let clash = [
            account("same", "A", false, "LIVE"),
            account("same", "B", false, "SIM"),
        ];
        assert_eq!(account_badges(&clash)["same"], "LIVE");
        assert_eq!(account_labels(&clash)["same"], "LIVE · A");
        let clash = [clash[1].clone(), clash[0].clone()];
        assert_eq!(account_badges(&clash)["same"], "LIVE");
        assert_eq!(group_headers(JournalGroup::Tags)[0], "Tag");
    }

    #[test]
    fn editor_types_switches_saves_and_cancels() {
        let r = row(&trade(), PnlDisplay::Both);
        let mut ed = Editor::new(&r);
        assert_eq!(ed.tags, "breakout, A+");
        assert_eq!(ed.key("backspace", None), EditorKey::Typing);
        assert_eq!(ed.key("backspace", None), EditorKey::Typing);
        for c in [",", " ", "o", "p", "e", "n"] {
            ed.key(c, Some(c));
        }
        assert_eq!(ed.key("tab", None), EditorKey::Typing);
        for c in ["g", "o", "o", "d"] {
            ed.key(c, Some(c));
        }
        assert_eq!(ed.key("f5", None), EditorKey::Ignored);
        assert_eq!(
            ed.key("escape-ish", Some("\u{1b}")),
            EditorKey::Ignored,
            "control characters never type"
        );
        let EditorKey::Save(e) = ed.key("enter", None) else {
            panic!("saves")
        };
        // "breakout, A+" less two characters, then ", open": an empty tag in the middle.
        assert_eq!(e.tags, ["breakout", " ", " open"]);
        assert_eq!(e.note, "good");
        // The journal tidies the tags.
        let mut j = Journal::from_records([Record::Trade(Box::new(trade()))]);
        assert_eq!(j.edit(e).unwrap().tags, ["breakout", "open"]);
        assert_eq!(Editor::new(&r).key("escape", None), EditorKey::Cancel);
    }
}
