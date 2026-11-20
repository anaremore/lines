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
//!   (a trade with several tags counts in each), over the same dates as the strip. By account
//!   adds each account's consistency-rule progress (its prop preset's rule, over all dates;
//!   `stats::consistency`), and so does the strip when the pane shows one account.
//! - **Accounts shown** (header chip, palette, pane menu, saved with the pane): every account,
//!   one account group, or one account. The rows, the strip, the tables and the header's mark
//!   all follow it. Broker accounts (a venue observed, trades placed outside Lines included)
//!   are journaled like paper.
//! - **Broker fills**: a net P&L whose fees are the product fee table's estimate (the broker
//!   reported none) reads `est.`; MAE/MFE that Lines' own market data didn't cover read
//!   "no data", never a guess. An account no current list has shows only the end of its id.
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
use lines_trading::journal::{Trade, price_value};
use lines_trading::{JournalShared, Money, Side};

use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pane_columns::{
    self, ColumnCmd, ColumnMenu, Columns, HeaderHit, JournalCol, Table, header_row_h,
};
use crate::pnl::{self, PnlDisplay};
use crate::popover;
use crate::text::{Glyphs, f};
use crate::theme::Theme;
use crate::trading::Trading;
use crate::wsdoc::{JournalFilter, JournalGroup, JournalState, PaneDoc};

const ROW_H_100: f32 = 18.0;

/// [`ROW_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn row_h() -> f32 {
    crate::zoom::f(ROW_H_100)
}
const TEXT_PX: f32 = 12.0;
const PAD_100: f32 = 6.0;
/// [`PAD_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn pad() -> f32 {
    crate::zoom::f(PAD_100)
}

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

/// The scroll that shows row `i` with `visible` rows in view, moving as little as it can.
fn reveal_scroll(scroll: usize, i: usize, visible: usize) -> usize {
    if i < scroll {
        i
    } else if i >= scroll + visible {
        i + 1 - visible.max(1)
    } else {
        scroll
    }
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
    // An unlisted product has no tick to state, rather than raw price units called ticks.
    let ticks = product.as_ref().and_then(|p| t.ticks(p.price.tick));
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
    let pnl = if mode.money() && t.fees_estimated() {
        // The fees are the product fee table's (the broker reported none): money-relevant.
        format!("{} est.", money_text(net))
    } else if mode.money() {
        money_text(net)
    } else if mode.ticks() {
        ticks.map_or_else(dash, per_contract_text)
    } else {
        dash()
    };
    // The whole position's worst/best P&L while open, and the same dollars per contract (like the
    // Ticks column and the CSV) at the trade's largest size.
    let excursion_ticks = match product.as_ref().and_then(|p| t.excursion_ticks(p)) {
        Some((mae, mfe)) => format!("{} / {}", tick_number(mae), per_contract_text(mfe)),
        None => "— / —".to_owned(),
    };
    let excursions = match (mode.money(), mode.ticks()) {
        (false, false) => dash(),
        // Lines' market data didn't cover the trade: never a guess from its fills.
        _ if t.excursion_unavailable => "no data".to_owned(),
        (true, true) => format!(
            "{} / {}  ({excursion_ticks})",
            money_text(t.mae),
            money_text(t.mfe),
        ),
        (true, false) => format!("{} / {}", money_text(t.mae), money_text(t.mfe)),
        (false, true) => excursion_ticks,
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
            tk(ticks),
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
            ticks.map_or(0, |v| ((v * 10.0).round() as i64).signum() as i8)
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

/// `11-05 14:31:07` on the display zone's clock (like the chart's axis).
pub fn clock_text(t: UtcMillis) -> String {
    let (_, mo, d, h, mi, s) = lines_core::display_tz::current().civil(t);
    format!("{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// The display zone's label at `t` ("ET", "UTC", "CET").
pub fn zone_text(t: UtcMillis) -> String {
    let mut s = String::new();
    let _ = lines_core::display_tz::current().write_label(t, &mut s);
    s
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

/// Columns of the by-account and by-tag tables (the last, consistency, by account only).
pub const GROUP_COLS: usize = 8;

/// One account's or tag's line (formatted once per journal change).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupRow {
    /// Name, trades, win rate, net, average trade, profit factor, max drawdown, consistency
    /// (empty by tag).
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
        if g == JournalGroup::Accounts {
            "Consistency (all dates)"
        } else {
            ""
        },
    ]
}

/// An account's consistency-rule progress, in words (never color alone): the best trade
/// date's share of the total profit against the rule, and, when over and money is shown, the
/// profit still needed. `—` without a rule.
pub fn consistency_text(c: Option<&stats::ConsistencyProgress>, mode: PnlDisplay) -> String {
    let Some(c) = c else {
        return "—".to_owned();
    };
    let max = c.max_pct;
    match (c.best_pct, c.ok) {
        (Some(p), Some(true)) => format!("best day {p}% of {max}% max: within"),
        (Some(p), _) if mode.money() => format!(
            "best day {p}% of {max}% max: over, needs {} more profit",
            money_text(c.needed).trim_start_matches('+')
        ),
        (Some(p), _) => format!("best day {p}% of {max}% max: over"),
        (None, _) => format!("no profit yet ({max}% max)"),
    }
}

/// Each account's consistency rule (its maximum best-day share, percent): the prop rules
/// Settings › Accounts gives it (by name, else id), else the rules in force at the core (a
/// paper account's). An account without one isn't listed.
fn consistency_rules(
    accounts: &[crate::trading::AccountInfo],
    settings: Option<&crate::picker::Settings>,
) -> std::collections::BTreeMap<String, u8> {
    accounts
        .iter()
        .filter_map(|a| {
            let from_settings = settings.and_then(|s| {
                [&a.name, &a.id.0]
                    .into_iter()
                    .find_map(|k| s.prop_rules.get(k))
                    .map(|d| d.resolve().ok().flatten())
            });
            let rules = match from_settings {
                Some(r) => r,
                None => a.rules.clone(),
            };
            let pct = rules?.consistency?.max_best_day_pct;
            Some((a.id.0.clone(), pct))
        })
        .collect()
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

/// An account's label, marked when no current list has it. An unlisted id shows only its last
/// four characters (a broker's account id is private; its list, once connected, decides how
/// much of it to show), except Lines' own paper ids.
fn account_label(names: &std::collections::HashMap<String, String>, id: &str) -> String {
    names
        .get(id)
        .cloned()
        .unwrap_or_else(|| format!("{UNLISTED} · {}", masked_id(id)))
}

/// `…1234` for an id longer than four characters; Lines' paper ids (`paper`, `paper-…`) whole.
fn masked_id(id: &str) -> String {
    let n = id.chars().count();
    if id == "paper" || id.starts_with("paper-") || n <= 4 {
        return id.to_owned();
    }
    let tail: String = id.chars().skip(n - 4).collect();
    format!("…{tail}")
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
    consistency: &std::collections::HashMap<String, String>,
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
            let rule = match g {
                JournalGroup::Accounts => {
                    consistency.get(&key).cloned().unwrap_or_else(|| "—".into())
                }
                _ => String::new(),
            };
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
                    rule,
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
    /// Which accounts are shown (`None`: every account).
    filter: Option<JournalFilter>,
    /// The ids `filter` keeps (a group's members as last read; `None`: every account).
    keep: Option<std::collections::HashSet<String>>,
    /// Each account's consistency rule (max best-day percent), as last read.
    rules: std::collections::BTreeMap<String, u8>,
    selected: Option<String>,
    /// First row shown.
    scroll: usize,
    /// Bring the selected trade into view at the next paint (`select_trade`).
    reveal: bool,
    /// The tag editor (a floating panel), when open.
    tags: Option<crate::journal_tags::TagEditor>,
    /// More selected trades beside `selected` (Ctrl+click, Shift+click): the tag editor
    /// applies to all of them.
    also: Vec<String>,
    /// The screenshot fixture `--journal-tags` was applied.
    shot_tags: bool,
    /// The trade row under the pointer (its Tags cell offers "+ tag").
    hover: Option<String>,
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
        doc.journal = (self.all_dates
            || !columns.is_empty()
            || !self.group.is_trades()
            || self.filter.is_some())
        .then_some(JournalState {
            all_dates: self.all_dates,
            columns,
            group: self.group,
            filter: self.filter.clone(),
        });
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
            filter: None,
            keep: None,
            rules: Default::default(),
            selected: None,
            scroll: 0,
            reveal: false,
            tags: None,
            also: Vec::new(),
            shot_tags: false,
            hover: None,
            glyphs: Glyphs::new(TEXT_PX),
            cols: Columns::new(Table::Journal),
            bounds: Bounds::default(),
        }
    }

    pub fn restore(&mut self, state: &JournalState) {
        self.all_dates = state.all_dates;
        self.group = state.group;
        self.filter = state.filter.clone();
        self.cols.load(&JournalCol::ALL, &state.columns);
    }

    pub fn filter(&self) -> Option<&JournalFilter> {
        self.filter.as_ref()
    }

    /// Show every account, one group or one account.
    pub fn set_filter(&mut self, f: Option<JournalFilter>, cx: &mut Context<Self>) {
        if self.filter != f {
            self.filter = f;
            self.keep = None;
            self.scroll = 0;
            self.group_scroll = 0;
            self.close_tags(cx);
            self.built = None;
            cx.notify();
        }
    }

    /// What can be shown, in order: every account, each group (by name), each listed account,
    /// then accounts with trades that no list has. Each with its label.
    pub fn filter_choices(&self, cx: &App) -> Vec<(Option<JournalFilter>, String)> {
        let mut out = vec![(None, "All accounts".to_owned())];
        let Some(t) = Trading::try_global(cx) else {
            return out;
        };
        let mut groups: Vec<String> = t
            .cache
            .read()
            .groups()
            .iter()
            .map(|g| g.name.clone())
            .collect();
        groups.sort_by_key(|g| g.to_lowercase());
        for g in groups {
            let label = format!("Group · {g}");
            out.push((Some(JournalFilter::Group(g)), label));
        }
        let mut seen = std::collections::HashSet::new();
        for a in t.accounts() {
            if seen.insert(a.id.0.clone()) {
                out.push((
                    Some(JournalFilter::Account(a.id.0.clone())),
                    account_label(&self.names, &a.id.0),
                ));
            }
        }
        if let Some(snap) = self.journal.as_ref().and_then(|j| j.snapshot()) {
            for tr in &snap.trades {
                if seen.insert(tr.account.0.clone()) {
                    out.push((
                        Some(JournalFilter::Account(tr.account.0.clone())),
                        account_label(&self.names, &tr.account.0),
                    ));
                }
            }
        }
        out
    }

    /// The header chip's and the menu's words for what is shown.
    pub fn filter_label(&self) -> String {
        match &self.filter {
            None => "All accounts".to_owned(),
            Some(JournalFilter::Group(g)) => format!("Group · {g}"),
            Some(JournalFilter::Account(a)) => account_label(&self.names, a),
        }
    }

    /// The next choice after the one shown (wrapping to every account).
    pub fn next_filter(&mut self, cx: &mut Context<Self>) {
        let choices = self.filter_choices(cx);
        let i = choices
            .iter()
            .position(|(f, _)| *f == self.filter)
            .map_or(0, |i| (i + 1) % choices.len());
        let f = choices.into_iter().nth(i).and_then(|(f, _)| f);
        self.set_filter(f, cx);
    }

    /// The choice a menu row names by its key (0: every account); `false` if none matches
    /// now (the list changed since the menu opened).
    pub fn pick_filter(&mut self, key: u64, cx: &mut Context<Self>) -> bool {
        if key == 0 {
            self.set_filter(None, cx);
            return true;
        }
        let found = self
            .filter_choices(cx)
            .into_iter()
            .find_map(|(f, _)| f.filter(|f| f.key() == key));
        match found {
            Some(f) => {
                self.set_filter(Some(f), cx);
                true
            }
            None => false,
        }
    }

    /// The account ids the filter keeps, from the account groups as they are now.
    fn keep_of(&self, cx: &App) -> Option<std::collections::HashSet<String>> {
        match &self.filter {
            None => None,
            Some(JournalFilter::Account(a)) => Some(std::iter::once(a.clone()).collect()),
            Some(JournalFilter::Group(g)) => Some(
                Trading::try_global(cx)
                    .and_then(|t| {
                        t.cache
                            .read()
                            .groups()
                            .iter()
                            .find(|x| x.name == *g)
                            .map(|x| x.members.iter().map(|m| m.0.clone()).collect())
                    })
                    // A group that is gone keeps nothing (its name still shows).
                    .unwrap_or_default(),
            ),
        }
    }

    pub fn group(&self) -> JournalGroup {
        self.group
    }

    /// List trades, or P&L by account or by tag.
    pub fn set_group(&mut self, g: JournalGroup, cx: &mut Context<Self>) {
        if self.group != g {
            self.group = g;
            self.group_scroll = 0;
            self.close_tags(cx);
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

    /// Open the tag editor for the selected trades (else the newest trade).
    pub fn start_edit(&mut self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        if !self.group.is_trades() {
            return Err("switch the journal to Trades to tag a trade");
        }
        let i = self
            .selected_index()
            .or((!self.rows.is_empty()).then_some(0));
        let id = i
            .and_then(|i| self.rows.get(i))
            .map(|r| r.id.clone())
            .ok_or("no trades to tag yet")?;
        if self.selected.as_deref() != Some(id.as_str()) {
            self.selected = Some(id);
            self.also.clear();
        }
        self.open_tags(cx)
    }

    /// Every selected trade's id that the pane shows (a filter may hide some): the primary
    /// first.
    fn selection(&self) -> Vec<String> {
        let mut v: Vec<String> = self.selected.iter().cloned().collect();
        for a in &self.also {
            if !v.contains(a) {
                v.push(a.clone());
            }
        }
        v.retain(|id| self.rows.iter().any(|r| &r.id == id));
        v
    }

    fn is_selected(&self, id: &str) -> bool {
        self.selected.as_deref() == Some(id) || self.also.iter().any(|a| a == id)
    }

    /// Screenshot fixture (`--journal-tags`): tag the second and third newest trades, then open
    /// the editor on the newest. Blocks on the journal (screenshots only).
    fn shot_fixture(&mut self, cx: &mut Context<Self>) {
        let wanted = Trading::try_global(cx).is_some_and(|t| t.start_journal_tags);
        if self.shot_tags || !wanted || self.rows.len() < 3 {
            return;
        }
        let Some(j) = self.journal.clone() else {
            return;
        };
        self.shot_tags = true;
        for (i, tags) in [(1, vec!["RI bounce"]), (2, vec!["chase", "late"])] {
            j.edit(lines_trading::journal::Edit {
                id: self.rows[i].id.clone(),
                tags: tags.into_iter().map(str::to_owned).collect(),
                note: String::new(),
                excursion: None,
            });
        }
        j.sync();
        self.built = None;
        let _ = self.rebuild();
        self.selected = Some(self.rows[0].id.clone());
        let _ = self.open_tags(cx);
    }

    /// Open the tag editor on the selection (the popover rule: any other popover closes).
    fn open_tags(&mut self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        let snap = self
            .journal
            .as_ref()
            .and_then(|j| j.snapshot())
            .ok_or("the journal is busy; try again")?;
        let ids = self.selection();
        let trades: Vec<&Trade> = ids
            .iter()
            .filter_map(|id| snap.trades.iter().find(|t| &t.id == id))
            .collect();
        let Some(first) = trades.first() else {
            return Err("select a trade in the journal first");
        };
        let title = if trades.len() == 1 {
            let label = account_label(&self.names, &first.account.0);
            let time = clock_text(first.entry_time);
            let hm = time.get(6..11).unwrap_or(&time);
            format!("Tags · {label} · {hm}")
        } else {
            format!("Tags · {} trades", trades.len())
        };
        let recent = crate::journal_tags::recent_tags(snap.trades.iter());
        self.tags = Some(crate::journal_tags::TagEditor::new(title, &trades, &recent));
        crate::popover::opened(
            cx,
            crate::popover::Kind::JournalTags,
            |v: &mut Self, cx| v.close_tags(cx),
            |v: &Self| v.tags.is_some(),
        );
        cx.notify();
        Ok(())
    }

    /// Close the tag editor, saving a typed note.
    fn close_tags(&mut self, cx: &mut Context<Self>) {
        if let Some(ed) = self.tags.take() {
            self.save(ed.close());
            crate::popover::closed(cx, crate::popover::Kind::JournalTags);
            cx.notify();
        }
    }

    fn save(&self, edits: Vec<lines_trading::journal::Edit>) {
        if let Some(j) = &self.journal {
            for e in edits {
                j.edit(e);
            }
        }
    }

    /// The tag editor's click on a chip's × (`remove`) or a recent tag (`add`).
    fn tag_click(&mut self, tag: &str, add: bool, cx: &mut Context<Self>) {
        let Some(ed) = &mut self.tags else {
            return;
        };
        let edits = if add { ed.add(tag) } else { ed.remove(tag) };
        if let Some(e) = edits {
            self.save(e);
        }
        cx.notify();
    }

    /// Show a trade (a click on its chart marker): the trades list, that trade selected and
    /// scrolled into view once its row is there.
    pub fn select_trade(&mut self, id: &str, cx: &mut Context<Self>) {
        // A trade the filter hides: show every account, so the trade asked for is there. The
        // filter's accounts as they are now (a pane not drawn yet hasn't read them).
        let keep = self.keep_of(cx);
        let hidden = keep.as_ref().is_some_and(|keep| {
            self.journal
                .as_ref()
                .and_then(|j| j.snapshot())
                .and_then(|s| s.trades.iter().find(|t| t.id == id).cloned())
                .is_some_and(|t| !keep.contains(&t.account.0))
        });
        if hidden {
            self.set_filter(None, cx);
        }
        self.group = JournalGroup::Trades;
        self.selected = Some(id.to_owned());
        // Scrolled to at paint: a journal opened for it has no rows or size yet.
        self.reveal = true;
        cx.notify();
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
        self.open_chart(&r.id, r.shot.as_deref(), cx)
    }

    /// Open trade `id`'s snapshot `file`. One drawn without data (or missing) is asked for again
    /// (the store, then the data source) and the notice says so, rather than opening a blank.
    fn open_chart(
        &self,
        id: &str,
        file: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        let dir = Trading::try_global(cx).and_then(|t| t.shots_dir.clone());
        let found = crate::journal_shots::locate(dir.as_deref(), file);
        let redraw = match &found {
            Ok(path) => crate::journal_shots::needs_drawing(path),
            Err(_) => file.is_some() && dir.is_some(),
        };
        if redraw && let Some(j) = self.journal.as_ref().filter(|j| j.persistent()) {
            j.repair(id.to_owned());
            return Err(
                "no chart data for this trade yet: fetching it now; view it again in a moment",
            );
        }
        let path = found?;
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

    /// The accounts' consistency rules (from settings, else the core's) and the filter's
    /// accounts: read each render, as a Settings or group change moves no journal revision; a
    /// change rebuilds the lines.
    fn refresh_rules(&mut self, cx: &App) {
        let Some(t) = Trading::try_global(cx) else {
            return;
        };
        let settings = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| &a.settings);
        let rules = consistency_rules(&t.accounts(), settings);
        if rules != self.rules {
            self.rules = rules;
            self.built = None;
        }
        // A group's members as they are now.
        let keep = self.keep_of(cx);
        if keep != self.keep {
            self.keep = keep;
            self.built = None;
        }
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
        let names = &self.names;
        let keep = &self.keep;
        let kept = |a: &str| keep.as_ref().is_none_or(|k| k.contains(a));
        let trades: Vec<&Trade> = snap.trades.iter().filter(|t| kept(&t.account.0)).collect();
        // New trades go on top: while scrolled, keep the same rows in view (nothing the
        // trader aims at shifts under the pointer, docs/design.md §7). Counted among the trades
        // shown: another account's trade, hidden by the filter, moves nothing.
        let added = trades.len().saturating_sub(self.rows.len());
        if self.scroll > 0 && self.built.is_some() {
            self.scroll += added;
        }
        let open = snap.open_accounts.iter().filter(|a| kept(&a.0)).count();
        self.rows = trades
            .iter()
            .rev()
            .map(|t| {
                let mut r = row(t, mode);
                r.cells[JournalCol::Account.index()] = account_label(names, &t.account.0);
                r
            })
            .collect();
        // With a filter, what the pane shows; with none, as before (the listed accounts'
        // kinds while nothing has traded).
        self.mark = if keep.is_some() && trades.is_empty() {
            None
        } else {
            view_mark(trades.iter().map(|t| t.account.0.as_str()), &self.badges)
        };
        // Consistency over every date of each account shown, by its rule.
        let consistency: std::collections::HashMap<String, String> = self
            .rules
            .iter()
            .filter(|(a, _)| kept(a))
            .map(|(a, &pct)| {
                let c =
                    stats::consistency(trades.iter().copied().filter(|t| t.account.0 == *a), pct);
                (a.clone(), consistency_text(c.as_ref(), mode))
            })
            .collect();
        let one = match &self.filter {
            Some(JournalFilter::Account(a)) => consistency.get(a),
            _ => None,
        };
        self.stats_today = stats_items(
            // The trade date the stats count (the exchange's), not the display zone's day: a
            // Tokyo evening trade is the next morning there. It says whose date it is.
            &format!("Trade date {today} (exchange)"),
            &Stats::of(trades.iter().copied().filter(|t| t.trade_date == today)),
            open,
            mode,
        );
        self.stats_all = stats_items("All dates", &Stats::of(trades.iter().copied()), open, mode);
        if let Some(c) = one {
            for s in [&mut self.stats_today, &mut self.stats_all] {
                s.push(item(format!("consistency: {c}")));
            }
        }
        let all = self.all_dates;
        self.groups = group_rows(
            trades
                .iter()
                .copied()
                .filter(|t| all || t.trade_date == today),
            self.group,
            names,
            &consistency,
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
                let i = ((y - header_row_h()) / row_h()).floor();
                if i >= 0.0
                    && let Some(r) = self.rows.get(self.scroll + i as usize).cloned()
                {
                    let m = ev.modifiers;
                    if m.control || m.platform {
                        // Ctrl/Cmd+click: add or take out one trade.
                        if self.selected.as_deref() == Some(r.id.as_str()) {
                            self.selected = self.also.pop();
                        } else if let Some(k) = self.also.iter().position(|a| *a == r.id) {
                            self.also.remove(k);
                        } else {
                            if let Some(s) = self.selected.take() {
                                self.also.push(s);
                            }
                            self.selected = Some(r.id.clone());
                        }
                        cx.notify();
                        return;
                    }
                    if m.shift
                        && let Some(a) = self.selected_index()
                    {
                        // Shift+click: the range from the selected trade.
                        let n = self.scroll + i as usize;
                        let (lo, hi) = (a.min(n), a.max(n));
                        self.also = self.rows[lo..=hi]
                            .iter()
                            .map(|x| x.id.clone())
                            .filter(|x| Some(x) != self.selected.as_ref())
                            .collect();
                        cx.notify();
                        return;
                    }
                    let in_selection = self.is_selected(&r.id);
                    let on_tags = self
                        .cols
                        .cell(JournalCol::Tags.index(), 0.0)
                        .is_some_and(|(cx0, cw)| x >= cx0 && x < cx0 + cw);
                    if !(on_tags && in_selection) {
                        self.also.clear();
                    }
                    self.selected = Some(r.id.clone());
                    if on_tags && ev.click_count == 1 {
                        if let Err(e) = self.open_tags(cx) {
                            cx.defer(move |cx| crate::workspace::notice(e.to_owned(), cx));
                        }
                        cx.stop_propagation();
                        return;
                    }
                    let on_chart = r.shot.is_some()
                        && self
                            .cols
                            .cell(JournalCol::Chart.index(), 0.0)
                            .is_some_and(|(cx0, cw)| x >= cx0 && x < cx0 + cw);
                    if on_chart {
                        // The first click of a double opens it, once; the editor stays shut.
                        if ev.click_count == 1
                            && let Err(e) = self.open_chart(&r.id, r.shot.as_deref(), cx)
                        {
                            // Deferred: a notice redraws windows, this one being updated now.
                            cx.defer(move |cx| crate::workspace::notice(e.to_owned(), cx));
                        }
                    } else if ev.click_count >= 2
                        && let Err(e) = self.open_tags(cx)
                    {
                        cx.defer(move |cx| crate::workspace::notice(e.to_owned(), cx));
                    }
                    cx.notify();
                }
            }
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.cols.drag.is_none() {
            // The row under the pointer: its Tags cell offers "+ tag".
            let (_, y) = self.table_xy(ev.position);
            let i = ((y - header_row_h()) / row_h()).floor();
            let hover = (self.group.is_trades() && i >= 0.0 && ev.pressed_button.is_none())
                .then(|| self.rows.get(self.scroll + i as usize))
                .flatten()
                .map(|r| r.id.clone());
            if hover != self.hover {
                self.hover = hover;
                cx.notify();
            }
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
            return;
        }
        // A row: it becomes the selection (kept if already in it) for the menu's "Tags…".
        let i = ((y - header_row_h()) / row_h()).floor();
        if i >= 0.0
            && let Some(id) = self
                .rows
                .get(self.scroll + i as usize)
                .map(|r| r.id.clone())
            && !self.is_selected(&id)
        {
            self.selected = Some(id);
            self.also.clear();
            cx.notify();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if let Some(ed) = &mut self.tags {
            // AltGr (Ctrl+Alt on Windows) types characters such as @ { €; other modified
            // keys are commands and still work while editing.
            let alt_gr = m.control && m.alt && k.key_char.is_some();
            if (m.control || m.alt || m.platform) && !alt_gr {
                return;
            }
            use crate::journal_tags::TagKey;
            match ed.key(k.key.as_str(), k.key_char.as_deref()) {
                TagKey::Typing => {}
                TagKey::Changed(e) => self.save(e),
                TagKey::Close(e) => {
                    self.save(e);
                    self.tags = None;
                    crate::popover::closed(cx, crate::popover::Kind::JournalTags);
                }
                TagKey::Ignored => return,
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
            // Enter, or T (the journal's own key, like Enter: no app binding, a row selected).
            "enter" | "t" if !m.shift => {
                if let Err(e) = self.start_edit(cx) {
                    cx.defer(move |cx| crate::workspace::notice(e.to_owned(), cx));
                }
                cx.stop_propagation();
                return;
            }
            _ => None,
        };
        let Some(next) = next.filter(|_| n > 0) else {
            return;
        };
        self.selected = Some(self.rows[next].id.clone());
        self.also.clear();
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
        let rows = (dy / row_h()).round() as i64;
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
        (((f(self.bounds.size.height) - header_row_h()) / row_h()).floor() as usize).max(1)
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.common.paints += 1;
        self.bounds = bounds;
        // A trade selected from a chart: brought into view once its row exists.
        if self.reveal
            && let Some(i) = self.selected_index()
        {
            self.scroll = reveal_scroll(self.scroll, i, self.visible_rows());
            self.reveal = false;
        }
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, top) = (f(bounds.origin.x), f(bounds.origin.y));
        let (w, h) = (f(bounds.size.width), f(bounds.size.height));
        window.paint_quad(fill(bounds, t.bg));
        if !self.group.is_trades() {
            self.paint_groups(bounds, hair, window, cx);
            return;
        }
        self.cols.name_zone(lines_feed::now_utc());
        self.cols
            .measure_headers(&mut self.glyphs, &t, pad(), window);
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
                self.cols.need(c, tw, pad());
            }
        }
        // A narrow pane gives up the least needed columns first (never half a number).
        self.cols.layout_journal(w);
        let left: [bool; 13] = JournalCol::ALL.map(JournalCol::left_aligned);
        self.cols
            .paint_header(&mut self.glyphs, &t, bounds, pad(), &left, window, cx);
        let oy = top + header_row_h();
        if self.rows.is_empty() {
            let msg = if self.journal.is_some() {
                "No closed trades yet. Trades appear here when a position goes back to flat."
            } else {
                "Trading isn't running."
            };
            self.glyphs.paint(
                msg,
                ox + pad(),
                oy + 4.0,
                row_h(),
                t.text_dim,
                false,
                window,
                cx,
            );
            return;
        }
        for (n, i) in (self.scroll..end).enumerate() {
            let y = oy + n as f32 * row_h();
            if y + row_h() > top + h + 0.5 {
                break;
            }
            let picked = self.is_selected(&self.rows[i].id);
            let offer = picked || self.hover.as_deref() == Some(self.rows[i].id.as_str());
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
                let cell = Bounds::new(point(px(x), px(y)), size(px(cw), px(row_h())));
                window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                    if col.left_aligned() {
                        self.glyphs
                            .paint(text, x + pad(), y, row_h(), color, bold, window, cx);
                    } else {
                        self.glyphs.paint_right(
                            text,
                            x + cw - pad(),
                            y,
                            row_h(),
                            color,
                            bold,
                            window,
                            cx,
                        );
                    }
                });
            }
            // "+ tag" in the Tags cell of the row under the pointer or selected (only there:
            // no clutter), after its tags; a click on the cell opens the tag editor.
            if offer && let Some((x, cw)) = self.cols.cell(JournalCol::Tags.index(), ox) {
                let tags = &self.rows[i].cells[JournalCol::Tags.index()];
                let tw = if tags.is_empty() {
                    0.0
                } else {
                    self.glyphs.width(tags, t.text_main, false, window) + 8.0
                };
                let cell = Bounds::new(point(px(x), px(y)), size(px(cw), px(row_h())));
                window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                    self.glyphs.paint(
                        "+ tag",
                        x + pad() + tw,
                        y,
                        row_h(),
                        t.accent,
                        false,
                        window,
                        cx,
                    );
                });
            }
            if picked {
                // Selection: an accent outline (no fill, so text contrast is untouched).
                for (yy, hh) in [(y, hair * 2.0), (y + row_h() - hair * 2.0, hair * 2.0)] {
                    window.paint_quad(fill(
                        Bounds::new(point(px(ox), px(yy)), size(px(w), px(hh))),
                        t.accent,
                    ));
                }
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(y)), size(px(3.0), px(row_h()))),
                    t.accent,
                ));
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + row_h() - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        let body = Bounds::new(point(px(ox), px(oy)), size(px(w), px(h - header_row_h())));
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
        for (wc, head) in widths.iter_mut().zip(heads) {
            // By tag has no consistency column.
            *wc = if head.is_empty() {
                0.0
            } else {
                *wc + pad() * 4.0
            };
        }
        // The name column always shows (cut to fit if it must, at least 80 px); number
        // columns that don't fit whole are left out from the right. The consistency column
        // (last, wide) never squeezes the name: it is the first left out.
        let numbers: f32 = widths[1..GROUP_COLS - 1].iter().sum();
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
            Bounds::new(point(px(ox), px(top)), size(px(w), px(header_row_h()))),
            t.panel,
        ));
        let mut x = ox;
        for (c, (head, &cw)) in heads.iter().zip(&widths).enumerate().take(shown) {
            if c == 0 {
                self.glyphs.paint(
                    head,
                    x + pad(),
                    top,
                    header_row_h(),
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            } else {
                self.glyphs.paint_right(
                    head,
                    x + cw - pad(),
                    top,
                    header_row_h(),
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
            x += cw;
        }
        let oy = top + header_row_h();
        if self.groups.is_empty() {
            let msg = if self.journal.is_some() {
                "No closed trades in these dates."
            } else {
                "Trading isn't running."
            };
            self.glyphs.paint(
                msg,
                ox + pad(),
                oy + 4.0,
                row_h(),
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
            let y = oy + n as f32 * row_h();
            if y + row_h() > top + h + 0.5 {
                break;
            }
            end_y = y + row_h();
            let mut x = ox;
            for (c, (cell, &cw)) in r.cells.iter().zip(&widths).enumerate().take(shown) {
                let color = match c {
                    3 if r.sign > 0 => t.up,
                    3 if r.sign < 0 => t.down,
                    _ => t.text_main,
                };
                if c == 0 {
                    // A long name is cut at its column, never over the numbers.
                    let cell_b = Bounds::new(point(px(x), px(y)), size(px(cw), px(row_h())));
                    window.with_content_mask(
                        Some(gpui::ContentMask { bounds: cell_b }),
                        |window| {
                            self.glyphs.paint(
                                cell,
                                x + pad(),
                                y,
                                row_h(),
                                color,
                                false,
                                window,
                                cx,
                            );
                        },
                    );
                } else {
                    self.glyphs.paint_right(
                        cell,
                        x + cw - pad(),
                        y,
                        row_h(),
                        color,
                        c == 3,
                        window,
                        cx,
                    );
                }
                x += cw;
            }
            window.paint_quad(fill(
                Bounds::new(point(px(ox), px(y + row_h() - hair)), size(px(w), px(hair))),
                t.grid,
            ));
        }
        // By tag: say why the lines can add up to more than the strip.
        if self.group == JournalGroup::Tags && end_y + row_h() <= top + h + 0.5 {
            self.glyphs.paint(
                "A trade with several tags counts under each of them.",
                ox + pad(),
                end_y + 2.0,
                row_h(),
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

    /// Which accounts are shown, in words; a click shows the next (every account, each group,
    /// each account). The pane menu's Accounts list picks one directly.
    fn filter_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .id("journal-filter")
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(t.axis_line)
            .cursor_pointer()
            .text_color(if self.filter.is_some() {
                t.text_main
            } else {
                t.text_dim
            })
            .when(self.filter.is_some(), |d| {
                d.font_weight(gpui::FontWeight::SEMIBOLD)
            })
            .child(self.filter_label())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.next_filter(cx);
                }),
            )
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
            .text_size(crate::zoom::ui(12.0))
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
                            .child(self.group_tabs(cx))
                            .child(self.filter_chip(cx)),
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
            .h(crate::zoom::ui(16.0))
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
            .text_size(crate::zoom::ui(12.0))
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

    /// The tag editor's floating panel (owner-approved mock, 2026-11-19), opened under the
    /// selected row's Tags cell, over the table (it never grows a row or the pane): chips with
    /// a × each, the input, recent tags, the note, and the keys in one dim line.
    fn tags_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        use crate::journal_tags::Field;
        let ed = self.tags.as_ref()?;
        let t = self.theme;
        // At the UI zoom, as its text is.
        let width = crate::zoom::f(300.0);
        // Under the selected row's Tags cell, inside the table (pane px from the table's corner).
        let (bw, bh) = (f(self.bounds.size.width), f(self.bounds.size.height));
        let row = self
            .selected_index()
            .filter(|i| *i >= self.scroll)
            .map_or(0, |i| i - self.scroll);
        let x = self
            .cols
            .cell(JournalCol::Tags.index(), 0.0)
            .map_or(0.0, |(x, _)| x)
            .min((bw - width - 8.0).max(0.0))
            .max(0.0);
        let below = header_row_h() + (row + 1) as f32 * row_h() + 2.0;
        // Room for the panel under the row, else above it.
        // The frame puts its title bar where the body would start, the body under it.
        let y = if below + 200.0 + popover::title_h() > bh {
            (below - row_h() - 200.0 - popover::title_h()).max(0.0)
        } else {
            below
        };
        let chip =
            |id: (&'static str, usize), text: &str, removable: bool, cx: &mut Context<Self>| {
                let tag = text.to_owned();
                let mut c = div()
                    .id(id)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .bg(t.element_selected)
                    .text_color(t.text_main)
                    .child(text.to_owned());
                if removable {
                    c = c.child(
                        div()
                            .id((id.0, id.1 + 10_000))
                            .cursor_pointer()
                            .text_color(t.text_dim)
                            .hover(|s| s.text_color(t.text_main))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |v, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    v.tag_click(&tag, false, cx);
                                }),
                            ),
                    );
                } else {
                    c = c
                        .cursor_pointer()
                        .hover(|s| s.bg(t.element_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |v, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                v.tag_click(&tag, true, cx);
                            }),
                        );
                }
                c
            };
        let caret = |on: bool| if on { "▏" } else { "" };
        let tags_on = ed.field == Field::Tags;
        let input = div()
            .id("journal-tags-input")
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_1()
            .py(px(2.0))
            .min_h(px(22.0))
            .border_1()
            .border_color(if tags_on { t.accent } else { t.control_edge })
            .rounded_sm()
            .cursor_text()
            .children(
                ed.chips
                    .iter()
                    .enumerate()
                    .map(|(i, c)| chip(("journal-tag-chip", i), c, true, cx))
                    .collect::<Vec<_>>(),
            )
            .child(if ed.input.is_empty() && !tags_on {
                div().text_color(t.text_dim).child("Add a tag")
            } else if ed.input.is_empty() {
                div()
                    .text_color(t.text_dim)
                    .child(format!("{}type a tag, Enter to add", caret(true)))
            } else {
                div()
                    .text_color(t.text_main)
                    .child(format!("{}{}", ed.input, caret(tags_on)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if let Some(ed) = &mut v.tags {
                        ed.field = Field::Tags;
                    }
                    cx.notify();
                }),
            );
        let recent = (!ed.recent.is_empty()).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_1()
                .text_color(t.text_dim)
                .child("Recent:")
                .children(
                    ed.recent
                        .iter()
                        .enumerate()
                        .map(|(i, r)| chip(("journal-tag-recent", i), r, false, cx))
                        .collect::<Vec<_>>(),
                )
        });
        let note_on = ed.field == Field::Note;
        let note_text = if !ed.note.is_empty() || note_on {
            (format!("{}{}", ed.note, caret(note_on)), t.text_main)
        } else if ed.mixed_note {
            ("Notes differ: typing replaces them".to_owned(), t.text_dim)
        } else {
            ("Note…".to_owned(), t.text_dim)
        };
        let note = div()
            .id("journal-tags-note")
            .px_1()
            .py(px(2.0))
            .min_h(px(22.0))
            .border_1()
            .border_color(if note_on { t.accent } else { t.control_edge })
            .rounded_sm()
            .cursor_text()
            .text_color(note_text.1)
            .child(note_text.0)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if let Some(ed) = &mut v.tags {
                        ed.field = Field::Note;
                    }
                    cx.notify();
                }),
            );
        let keys = if ed.count() > 1 {
            format!(
                "Esc closes · Backspace removes the last tag · applies to the {} selected trades",
                ed.count()
            )
        } else {
            "Esc closes · Backspace removes the last tag · Tab: note".to_owned()
        };
        let body = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .bg(t.panel)
            .border_1()
            .border_color(t.axis_line)
            .rounded_b_md()
            .text_size(crate::zoom::ui(12.0))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| {
                cx.stop_propagation()
            })
            .child(input)
            .children(recent)
            .child(note)
            .child(
                div()
                    .text_size(crate::zoom::ui(11.0))
                    .text_color(t.text_dim)
                    .child(keys),
            );
        let dismiss: popover::OnDismiss =
            Box::new(cx.listener(|v, _: &MouseDownEvent, _, cx| v.close_tags(cx)));
        Some(popover::floating(
            popover::Panel::JournalTags,
            ed.title.clone(),
            &t,
            body,
            dismiss,
        ))
    }
}

impl JournalView {
    /// Taking typed text now (a note being edited): the workspace leaves drawing keys to it
    /// (`commands::runs_while_typing`).
    pub fn takes_text(&self) -> bool {
        self.tags.is_some()
    }
}

impl Render for JournalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_accounts(cx);
        self.refresh_rules(cx);
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
        self.shot_fixture(cx);
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
                    // "+ tag" goes with the pointer.
                    .id("journal-table")
                    .on_hover(cx.listener(|v, hovered: &bool, _, cx| {
                        if !*hovered && v.hover.take().is_some() {
                            cx.notify();
                        }
                    }))
                    .child(body)
                    .children(self.tags_panel(cx)),
            )
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
            "−$87.50 / +$412.50  (−1.8 / +8.3 t/ct)"
        );
        assert_eq!(c(JournalCol::Tags), "breakout, A+");
        assert_eq!(r.sign, 1);
        assert_eq!(duration_text(45_000), "45s");
        assert_eq!(duration_text(3_780_000), "1h 03m");
        assert_eq!(decimal(6001.0, 100), "6001.00");
        assert_eq!(decimal(0.5, 1), "0.5");
    }

    /// A product not in the catalog has no tick to state: dashes and empty fields, not raw price
    /// units called ticks. Its dollars stay.
    #[test]
    fn an_unlisted_product_states_no_ticks() {
        let mut t = trade();
        t.contract.product = lines_core::ProductId(u32::MAX);
        let cells = row(&t, PnlDisplay::Both).cells;
        assert_eq!(
            cells[JournalCol::MaeMfe.index()],
            "−$87.50 / +$412.50  (— / —)"
        );
        assert_eq!(cells[JournalCol::Ticks.index()], "—");
        assert_eq!(
            row(&t, PnlDisplay::Ticks).cells[JournalCol::Pnl.index()],
            "—"
        );
        let export = lines_trading::journal::csv(std::slice::from_ref(&t));
        let mut lines = export.lines();
        let names: Vec<&str> = lines.next().unwrap().split(',').collect();
        // From the end: the unlisted contract's label is quoted and holds commas.
        let from_end: Vec<&str> = lines.next().unwrap().rsplit(',').collect();
        for name in ["ticks", "mae_ticks", "mfe_ticks"] {
            let i = names.iter().position(|n| *n == name).unwrap();
            assert_eq!(from_end[names.len() - 1 - i], "", "{name}");
        }
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
        assert_eq!(ticks[JournalCol::MaeMfe.index()], "−1.8 / +8.3 t/ct");
        // In ticks mode the color follows the ticks shown, not the money after fees.
        let mut scratch = t.clone();
        scratch.exit_value = scratch.entry_value; // exit = entry: 0.0 t, a loss after fees
        scratch.realized = Money::ZERO;
        assert_eq!(row(&scratch, PnlDisplay::Ticks).sign, 0);
        assert_eq!(row(&scratch, PnlDisplay::Currency).sign, -1);
        let cur = cells(PnlDisplay::Currency);
        assert_eq!(cur[JournalCol::Pnl.index()], "+$310.02");
        assert_eq!(cur[JournalCol::Ticks.index()], "—");
        assert_eq!(cur[JournalCol::MaeMfe.index()], "−$87.50 / +$412.50");
    }

    #[test]
    fn stats_strip_reads_like_a_sentence() {
        let j = Journal::from_records([Record::Trade(Box::new(trade()))]);
        let s = j.stats(|_| true);
        assert_eq!(
            stats_text("Trade date 2026-11-05", &s, 1, PnlDisplay::Both),
            "Trade date 2026-11-05: 1 trade  ·  net +$310.02  ·  max drawdown $0.00  ·  win 100%  ·  avg win +$310.02  ·  avg loss —  ·  expectancy +$310.02  ·  profit factor —  ·  1 win in a row  ·  1 open"
        );
        // Net P&L is the loudest item, colored by its sign (and signed).
        let items = stats_items("Today", &s, 0, PnlDisplay::Both);
        assert!(items[1].strong && items[1].tone == 1);
        assert_eq!(
            stats_text("Trade date 2026-11-05", &s, 0, PnlDisplay::Off),
            "Trade date 2026-11-05: 1 trade  ·  win 100%  ·  1 win in a row"
        );
        assert_eq!(
            stats_text("All dates", &Stats::default(), 0, PnlDisplay::Both),
            "All dates: 0 trades"
        );
    }

    #[test]
    fn a_trade_opened_from_a_chart_scrolls_into_view() {
        // 10 rows in view from 0: row 150 is brought to the bottom, row 3 stays put,
        // a row above the view goes to the top.
        assert_eq!(reveal_scroll(0, 150, 10), 141);
        assert_eq!(reveal_scroll(0, 3, 10), 0);
        assert_eq!(reveal_scroll(40, 12, 10), 12);
        assert_eq!(reveal_scroll(0, 5, 0), 5, "no size yet: the row itself");
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
        let no_rules = std::collections::HashMap::new();
        let by_account = group_rows(
            &all,
            JournalGroup::Accounts,
            &names,
            &no_rules,
            PnlDisplay::Both,
        );
        let cells = |r: &GroupRow| r.cells.clone();
        assert_eq!(
            cells(&by_account[1]),
            [
                "Paper 50K eval",
                "2",
                "0%",
                "−$4.96",
                "−$2.48",
                "0.00",
                "−$4.96",
                "—"
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
                "$0.00",
                "—"
            ]
            .map(String::from),
            "an account no list has says so, with its id"
        );
        let by_tag = group_rows(
            &all,
            JournalGroup::Tags,
            &names,
            &no_rules,
            PnlDisplay::Both,
        );
        let order: Vec<&str> = by_tag.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(order, ["A+", "breakout", "fade", "untagged"]);
        assert_eq!(by_tag[0].cells[3], "+$310.02");
        let off = group_rows(&all, JournalGroup::Tags, &names, &no_rules, PnlDisplay::Off);
        assert!(off.iter().all(|r| r.sign == 0));
        assert!(
            off.iter()
                .all(|r| r.cells[3..GROUP_COLS - 1].iter().all(|c| c == "—")),
            "no money"
        );
        assert_eq!(off[0].cells[1..3], ["1".to_owned(), "100%".to_owned()]);
        // With money hidden the order is by name, so it doesn't tell the P&L either.
        let off_accounts = group_rows(
            &all,
            JournalGroup::Accounts,
            &names,
            &no_rules,
            PnlDisplay::Off,
        );
        let order: Vec<&str> = off_accounts.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(order, ["Paper 50K eval", "UNLISTED · paper"]);
        assert!(
            group_rows(
                &all,
                JournalGroup::Trades,
                &names,
                &no_rules,
                PnlDisplay::Both
            )
            .is_empty()
        );
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
            start_balance: None,
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
        // By tag has no consistency column; by account does.
        assert_eq!(group_headers(JournalGroup::Tags)[GROUP_COLS - 1], "");
        assert!(off.iter().all(|r| r.cells[GROUP_COLS - 1].is_empty()));
        assert_eq!(
            group_headers(JournalGroup::Accounts)[GROUP_COLS - 1],
            "Consistency (all dates)"
        );
    }

    /// A broker trade whose fees are the fee table's says `est.` on its net; one Lines' market
    /// data didn't cover says "no data" for MAE/MFE in every mode that shows numbers.
    #[test]
    fn broker_rows_label_estimates_and_unmeasured_excursions() {
        let mut t = trade();
        for f in &mut t.fills {
            f.fee_estimated = true;
        }
        t.excursion_unavailable = true;
        let c = |m: PnlDisplay| {
            let r = row(&t, m);
            (
                r.cells[JournalCol::Pnl.index()].clone(),
                r.cells[JournalCol::MaeMfe.index()].clone(),
            )
        };
        assert_eq!(
            c(PnlDisplay::Both),
            ("+$310.02 est.".into(), "no data".into())
        );
        assert_eq!(c(PnlDisplay::Ticks).1, "no data");
        assert_eq!(
            c(PnlDisplay::Ticks).0,
            "+6.3 t/ct",
            "ticks are by price: no fees"
        );
        assert_eq!(c(PnlDisplay::Off), ("—".into(), "—".into()));
        // A measured paper trade reads as before.
        assert_eq!(
            row(&trade(), PnlDisplay::Both).cells[JournalCol::Pnl.index()],
            "+$310.02"
        );
    }

    /// An account no list has (a broker not connected yet) shows only the end of its id;
    /// Lines' paper ids show whole.
    #[test]
    fn unlisted_broker_ids_are_masked() {
        let none = std::collections::HashMap::new();
        assert_eq!(account_label(&none, "ACCT-0000123456"), "UNLISTED · …3456");
        assert_eq!(account_label(&none, "paper-eval"), "UNLISTED · paper-eval");
        assert_eq!(account_label(&none, "paper"), "UNLISTED · paper");
        assert_eq!(masked_id("abcd"), "abcd");
    }

    /// Consistency in words: within, over (with the profit needed only when money shows), no
    /// profit yet, no rule.
    #[test]
    fn consistency_reads_in_words() {
        let mut win = trade(); // +$310.02 on 2026-11-05
        win.id = "w".into();
        let mut small = trade();
        small.id = "s".into();
        small.realized = Money::cents(10_248); // net +$100.00
        small.trade_date = CivilDate::new(2026, 11, 6).unwrap();
        let c = stats::consistency([&win, &small], 50);
        assert_eq!(
            consistency_text(c.as_ref(), PnlDisplay::Both),
            "best day 75% of 50% max: over, needs $210.02 more profit"
        );
        assert_eq!(
            consistency_text(c.as_ref(), PnlDisplay::Off),
            "best day 75% of 50% max: over"
        );
        let c = stats::consistency([&win, &small], 80);
        assert_eq!(
            consistency_text(c.as_ref(), PnlDisplay::Both),
            "best day 75% of 80% max: within"
        );
        let mut loss = trade();
        loss.realized = Money::ZERO;
        let c = stats::consistency([&loss], 30);
        assert_eq!(
            consistency_text(c.as_ref(), PnlDisplay::Both),
            "no profit yet (30% max)"
        );
        assert_eq!(consistency_text(None, PnlDisplay::Both), "—");
    }

    /// Each account's rule: Settings › Accounts' prop rules (by name, else id) win over the
    /// core's; an account with neither, or a preset without consistency, has none.
    #[test]
    fn consistency_rules_come_from_settings_else_the_core() {
        let preset = |pct: Option<u8>| {
            let mut p = lines_trading::rules::presets()
                .into_iter()
                .next()
                .expect("a built-in preset");
            p.consistency = pct.map(|p| lines_trading::rules::Consistency {
                max_best_day_pct: p,
            });
            p
        };
        let account = |id: &str, name: &str, rules| crate::trading::AccountInfo {
            id: AccountId::new(id),
            name: name.into(),
            paper: false,
            rules,
            option_guard: Default::default(),
            venue: Some(0),
            badge: "LIVE",
            read_only: None,
            start_balance: None,
        };
        let accounts = [
            account("a1", "Eval one", Some(preset(Some(40)))),
            account("a2", "Eval two", None),
            account("a3", "Funded", Some(preset(None))),
        ];
        let core = consistency_rules(&accounts, None);
        assert_eq!(
            core.into_iter().collect::<Vec<_>>(),
            [("a1".to_owned(), 40)]
        );
        let with = lines_trading::rules::presets()
            .into_iter()
            .find(|p| p.consistency.is_some())
            .expect("a built-in preset with consistency");
        let pct = with.consistency.as_ref().unwrap().max_best_day_pct;
        let mut settings = crate::picker::Settings::default();
        settings.prop_rules.insert(
            "Eval two".into(),
            serde_json::from_value(serde_json::json!({ "preset": with.name })).unwrap(),
        );
        let both = consistency_rules(&accounts, Some(&settings));
        assert_eq!(both.get("a2"), Some(&pct), "by name, from settings");
        assert_eq!(both.get("a1"), Some(&40));
        assert_eq!(both.get("a3"), None);
    }
}
