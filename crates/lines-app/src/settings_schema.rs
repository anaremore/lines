//! The settings schema (decision 0021, roadmap 6c): one table of [`Setting`]s drives the
//! Settings window (its sections, rows and search), the palette commands and where each value
//! is stored. A setting is data: its name, section, scope, safety class, kind (and so its
//! validation), description, and how it reads and writes [`Prefs`]. No setting has a
//! hand-built form.
//!
//! GPUI-free: the window and the palette read this table; `crate::settings_view` applies a
//! change's effects (theme, guardrails through the trading core, …).

use serde::{Deserialize, Serialize};

use crate::picker::Settings;

/// The Settings window's sections, in order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Appearance,
    Charts,
    Dom,
    Trading,
    Accounts,
    Data,
    Keys,
    Diagnostics,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::Appearance,
        Section::Charts,
        Section::Dom,
        Section::Trading,
        Section::Accounts,
        Section::Data,
        Section::Keys,
        Section::Diagnostics,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Charts => "Charts",
            Section::Dom => "DOM",
            Section::Trading => "Trading",
            Section::Accounts => "Accounts",
            Section::Data => "Data",
            Section::Keys => "Keys",
            Section::Diagnostics => "Diagnostics",
        }
    }
}

/// Where a setting applies: always shown beside it (decision 0021: no hidden inheritance).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every window and pane (`settings.json`).
    App,
    /// This workspace (saved with it).
    Workspace,
    /// Panes created from now on; existing panes keep their own.
    NewPanes,
    /// One trading account (`settings.json`, keyed by account).
    Account,
    /// One account and product.
    AccountProduct,
    /// Shown, not set here (a fact, a CLI flag, or another place).
    ReadOnly,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::App => "App",
            Scope::Workspace => "Workspace",
            Scope::NewPanes => "New panes",
            Scope::Account => "Account",
            Scope::AccountProduct => "Account · product",
            Scope::ReadOnly => "Read-only",
        }
    }
}

/// Trading-affecting settings are marked in the window and loosening them asks first
/// (decision 0021).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Safety {
    Normal,
    Trading,
}

/// A setting's value as the window, the palette and the tests see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    /// An index into the setting's choices.
    Choice(usize),
    /// A whole number; `None` is "off" (no limit) where [`Kind::Number`] allows it.
    Number(Option<i64>),
    /// Read-only text, or an editor's summary.
    Text(String),
}

/// What kind of control a setting has, and so how it validates.
#[derive(Copy, Clone, Debug)]
pub enum Kind {
    Toggle,
    /// One of a few named values (a segmented control; also one palette command each).
    Choice(&'static [&'static str]),
    /// A whole number in `min..=max`; `optional`: empty means off.
    Number {
        min: i64,
        max: i64,
        optional: bool,
        /// Shown after the number ("$", "min", "contracts").
        unit: &'static str,
    },
    /// An inline editor opens (bracket presets).
    Editor,
    /// Text only.
    Info,
}

/// Facts the window shows but doesn't own, gathered when it opens.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub store_path: String,
    /// `None`: this source keeps no store.
    pub store_budget_gb: Option<f64>,
    pub depth_products: Vec<String>,
    pub accounts: Vec<AccountFact>,
    pub config_dir: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountFact {
    /// The account's id.
    pub id: String,
    pub name: String,
    pub paper: bool,
    pub rules: Option<String>,
}

/// Everything settings read and write: the app's `settings.json` and this workspace's own.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prefs {
    pub app: Settings,
    pub ws: WorkspacePrefs,
}

/// The settings a workspace saves with itself.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspacePrefs {
    pub hide_hints: bool,
    pub latency_hud: bool,
}

/// Which account (and product) per-account settings mean, chosen in the window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    /// The account's id (bracket presets are keyed by it).
    pub account: Option<String>,
    /// Its name (guardrails are keyed by it, as `settings.json` has always had them).
    pub account_name: Option<String>,
    pub product: Option<String>,
}

impl Target {
    /// This account's saved guardrails (none: the default), wherever they are saved.
    pub fn guardrails(&self, p: &Prefs) -> lines_trading::Guardrails {
        self.guard_key(&p.app.guardrails)
            .and_then(|k| p.app.guardrails.get(&k).cloned())
            .unwrap_or_default()
    }

    /// Where this account's guardrails are saved: under its name, else its id (as startup
    /// reads them), else the name for a new entry.
    fn guard_key(
        &self,
        saved: &std::collections::BTreeMap<String, lines_trading::Guardrails>,
    ) -> Option<String> {
        [&self.account_name, &self.account]
            .into_iter()
            .flatten()
            .find(|k| saved.contains_key(*k))
            .or(self.account_name.as_ref().or(self.account.as_ref()))
            .cloned()
    }
}

type Get = fn(&Prefs, &Target, &Facts) -> Value;
type Set = fn(&mut Prefs, &Target, Value) -> Result<(), &'static str>;

pub struct Setting {
    /// Stable id (palette commands, tests, the window's state).
    pub key: &'static str,
    pub section: Section,
    pub name: &'static str,
    pub description: &'static str,
    pub scope: Scope,
    pub safety: Safety,
    pub kind: Kind,
    pub get: Get,
    /// `None`: read-only.
    pub set: Option<Set>,
    /// The value a reset gives (`None`: no reset, e.g. read-only or an editor).
    pub default: Option<Value>,
}

// ---- stored values ------------------------------------------------------------------------

/// Appearance in `settings.json` (optional fields; absent = the built-in default). A CLI
/// flag, when given, wins for that run.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appearance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<PaletteName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hollow: Option<HollowName>,
}

impl Appearance {
    pub fn is_default(&self) -> bool {
        *self == Appearance::default()
    }

    /// `self` (what is saved) with this run's look flags over it.
    pub fn with_flags(self, flags: Appearance) -> Appearance {
        Appearance {
            theme: flags.theme.or(self.theme),
            palette: flags.palette.or(self.palette),
            hollow: flags.hollow.or(self.hollow),
        }
    }

    /// After a change from `before` to `after` (both as shown, flags included): what to save
    /// (only the fields that changed, over what was `saved`) and the flags that still hold
    /// (a field the trader set drops its flag).
    pub fn changed(
        saved: Appearance,
        flags: Appearance,
        before: Appearance,
        after: Appearance,
    ) -> (Appearance, Appearance) {
        // Compared as shown (absent = the default): re-picking the value a default-valued
        // flag shows changes nothing, so it never writes over what is saved.
        let mut s = saved;
        let mut f = flags;
        if after.theme.unwrap_or_default() != before.theme.unwrap_or_default() {
            s.theme = after.theme;
            f.theme = None;
        }
        if after.palette.unwrap_or_default() != before.palette.unwrap_or_default() {
            s.palette = after.palette;
            f.palette = None;
        }
        if after.hollow.unwrap_or_default() != before.hollow.unwrap_or_default() {
            s.hollow = after.hollow;
            f.hollow = None;
        }
        (s, f)
    }

    /// An undo of the look field `key` ("appearance.theme", …): that field back as it was
    /// saved and flagged (`then`), every other field as it is `now` (later changes stay).
    pub fn undo_field(
        key: &str,
        now: (Appearance, Appearance),
        then: (Appearance, Appearance),
    ) -> (Appearance, Appearance) {
        let ((mut s, mut f), (s0, f0)) = (now, then);
        match key {
            "appearance.theme" => (s.theme, f.theme) = (s0.theme, f0.theme),
            "appearance.palette" => (s.palette, f.palette) = (s0.palette, f0.palette),
            "appearance.hollow" => (s.hollow, f.hollow) = (s0.hollow, f0.hollow),
            _ => {}
        }
        (s, f)
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaletteName {
    #[default]
    Standard,
    Colorblind,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HollowName {
    #[default]
    Up,
    Down,
}

/// The theme an appearance asks for.
pub fn theme_for(a: &Appearance) -> crate::theme::Theme {
    use crate::theme::{HollowBodies, Palette, Theme};
    let palette = match a.palette.unwrap_or_default() {
        PaletteName::Standard => Palette::Standard,
        PaletteName::Colorblind => Palette::Colorblind,
    };
    let mut t = match a.theme.unwrap_or_default() {
        ThemeName::Dark => Theme::dark(palette),
        ThemeName::Light => Theme::light(palette),
    };
    t.hollow = match a.hollow.unwrap_or_default() {
        HollowName::Up => HollowBodies::Up,
        HollowName::Down => HollowBodies::Down,
    };
    t
}

/// Defaults for panes created from now on (optional fields).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPanes {
    /// A new chart's timeframe (`Timeframe`'s text, e.g. "5m"); absent: 1m.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeframe: Option<String>,
    /// A new DOM's ticks per row; absent: 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dom_grouping: Option<u32>,
    /// A new DOM starts with the price trail on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dom_trail: bool,
    /// A new DOM's order quantity; absent: 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_qty: Option<u32>,
}

impl NewPanes {
    pub fn is_default(&self) -> bool {
        *self == NewPanes::default()
    }
}

/// Grouping choices offered (the DOM's G key cycles the same).
pub const GROUPINGS: [u32; 5] = [1, 2, 5, 10, 25];
const GROUPING_NAMES: [&str; 5] = ["1", "2", "5", "10", "25"];
const TIMEFRAME_NAMES: [&str; 12] = [
    "1m", "2m", "3m", "5m", "10m", "15m", "30m", "1h", "2h", "4h", "D", "W",
];

fn choice(names: &[&str], current: &str) -> usize {
    names.iter().position(|n| *n == current).unwrap_or(0)
}

fn want_bool(v: Value) -> Result<bool, &'static str> {
    match v {
        Value::Bool(b) => Ok(b),
        _ => Err("expected on or off"),
    }
}

fn want_choice(v: Value, n: usize) -> Result<usize, &'static str> {
    match v {
        Value::Choice(i) if i < n => Ok(i),
        _ => Err("not one of the choices"),
    }
}

/// Validate a number for `kind` (exact: out-of-range is refused, never clamped).
pub fn check_number(kind: Kind, v: Option<i64>) -> Result<Option<i64>, &'static str> {
    let Kind::Number {
        min, max, optional, ..
    } = kind
    else {
        return Err("not a number setting");
    };
    match v {
        None if optional => Ok(None),
        None => Err("a value is needed"),
        Some(n) if n < min => Err("too small"),
        Some(n) if n > max => Err("too large"),
        Some(n) => Ok(Some(n)),
    }
}

/// Parse what the trader typed into a number field ("1,500", "$1,500", "" = off).
pub fn parse_number(s: &str) -> Result<Option<i64>, &'static str> {
    let t: String = s
        .chars()
        .filter(|c| !matches!(c, ',' | '$' | ' ' | '_'))
        .collect();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>().map(Some).map_err(|_| "whole numbers only")
}

// ---- guardrails (per account; tighten now, loosen from the next trade date) --------------

fn guard<'a>(p: &'a Prefs, t: &Target) -> Option<&'a lines_trading::Guardrails> {
    t.guard_key(&p.app.guardrails)
        .and_then(|a| p.app.guardrails.get(&a))
}

fn guard_mut<'a>(
    p: &'a mut Prefs,
    t: &Target,
) -> Result<&'a mut lines_trading::Guardrails, &'static str> {
    let a = t
        .guard_key(&p.app.guardrails)
        .ok_or("choose an account first")?;
    Ok(p.app.guardrails.entry(a).or_default())
}

/// Whether `new` loosens anything compared with `old` (a field looser, or removed): the
/// window asks first and says it takes effect at the next trade date (the core's rule).
pub fn loosens(old: &lines_trading::Guardrails, new: &lines_trading::Guardrails) -> bool {
    // Tighter-of-both equals the old exactly when nothing in `new` is looser.
    old.tightest(new) != *new
}

fn drop_empty_guardrails(p: &mut Prefs, t: &Target) {
    if let Some(a) = t.guard_key(&p.app.guardrails)
        && p.app
            .guardrails
            .get(&a)
            .is_some_and(|g| *g == lines_trading::Guardrails::default())
    {
        p.app.guardrails.remove(&a);
    }
}

macro_rules! guard_field {
    ($get:ident, $set:ident, $field:ident, $ty:ty) => {
        fn $get(p: &Prefs, t: &Target, _: &Facts) -> Value {
            Value::Number(guard(p, t).and_then(|g| g.$field).map(|v| v as i64))
        }
        fn $set(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
            let Value::Number(n) = v else {
                return Err("expected a number");
            };
            guard_mut(p, t)?.$field = n.map(|n| n as $ty);
            drop_empty_guardrails(p, t);
            Ok(())
        }
    };
}
guard_field!(g_loss, s_loss, max_daily_loss, i64);
guard_field!(g_pos, s_pos, max_position, i64);
guard_field!(g_trades, s_trades, max_trades_per_day, u32);
guard_field!(g_close, s_close, no_entries_before_close_min, u32);

fn g_streak(p: &Prefs, t: &Target, _: &Facts) -> Value {
    Value::Number(
        guard(p, t)
            .and_then(|g| g.losing_streak)
            .map(|s| s.losses as i64),
    )
}
fn s_streak(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Number(n) = v else {
        return Err("expected a number");
    };
    let g = guard_mut(p, t)?;
    g.losing_streak = n.map(|n| lines_trading::LosingStreak {
        losses: n as u32,
        cooldown_min: g.losing_streak.map_or(15, |s| s.cooldown_min),
    });
    drop_empty_guardrails(p, t);
    Ok(())
}
fn g_cool(p: &Prefs, t: &Target, _: &Facts) -> Value {
    Value::Number(
        guard(p, t)
            .and_then(|g| g.losing_streak)
            .map(|s| s.cooldown_min as i64),
    )
}
fn s_cool(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Number(n) = v else {
        return Err("expected a number");
    };
    let g = guard_mut(p, t)?;
    match (g.losing_streak.as_mut(), n) {
        (Some(s), Some(m)) => s.cooldown_min = m as u32,
        (None, Some(_)) => return Err("set the losing streak first"),
        (_, None) => return Err("a cooldown is needed while a streak is set"),
    }
    Ok(())
}

// ---- the table ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
const fn s(
    key: &'static str,
    section: Section,
    name: &'static str,
    description: &'static str,
    scope: Scope,
    safety: Safety,
    kind: Kind,
    get: Get,
    set: Option<Set>,
) -> Setting {
    Setting {
        key,
        section,
        name,
        description,
        scope,
        safety,
        kind,
        get,
        set,
        default: None,
    }
}

fn with_default(mut st: Setting, v: Value) -> Setting {
    st.default = Some(v);
    st
}

use Kind::*;
use Safety::{Normal, Trading};
use Section as Sec;

const THEMES: &[&str] = &["Dark", "Light"];
const PALETTES: &[&str] = &["Standard", "Colorblind"];
const HOLLOWS: &[&str] = &["Up candles", "Down candles"];
const PNL: &[&str] = &["Off", "Ticks", "Currency", "Both"];
const UNITS: &[&str] = &["Ticks", "Points"];

fn pnl_modes() -> [crate::pnl::PnlDisplay; 4] {
    use crate::pnl::PnlDisplay as P;
    [P::Off, P::Ticks, P::Currency, P::Both]
}

/// Every setting, in section order.
pub fn all() -> &'static [Setting] {
    static ALL: std::sync::OnceLock<Vec<Setting>> = std::sync::OnceLock::new();
    ALL.get_or_init(table)
}

#[allow(clippy::too_many_lines)]
fn table() -> Vec<Setting> {
    vec![
        // Appearance.
        with_default(
            s(
                "appearance.theme",
                Sec::Appearance,
                "Theme",
                "Colors for every window.",
                Scope::App,
                Normal,
                Choice(THEMES),
                |p, _, _| Value::Choice(p.app.appearance.theme.unwrap_or_default() as usize),
                Some(|p, _, v| {
                    let i = want_choice(v, 2)?;
                    p.app.appearance.theme = Some(if i == 0 {
                        ThemeName::Dark
                    } else {
                        ThemeName::Light
                    })
                    .filter(|t| *t != ThemeName::default());
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        with_default(
            s(
                "appearance.palette",
                Sec::Appearance,
                "Palette",
                "Up and down colors. Colorblind uses blue and orange; direction also shows by shape.",
                Scope::App,
                Normal,
                Choice(PALETTES),
                |p, _, _| Value::Choice(p.app.appearance.palette.unwrap_or_default() as usize),
                Some(|p, _, v| {
                    let i = want_choice(v, 2)?;
                    p.app.appearance.palette = (i == 1).then_some(PaletteName::Colorblind);
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        with_default(
            s(
                "appearance.hollow",
                Sec::Appearance,
                "Hollow candles",
                "Which direction's candle bodies are hollow (the other is filled), so direction never relies on color alone.",
                Scope::App,
                Normal,
                Choice(HOLLOWS),
                |p, _, _| Value::Choice(p.app.appearance.hollow.unwrap_or_default() as usize),
                Some(|p, _, v| {
                    let i = want_choice(v, 2)?;
                    p.app.appearance.hollow = (i == 1).then_some(HollowName::Down);
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        with_default(
            s(
                "appearance.hints",
                Sec::Appearance,
                "Hint bar",
                "Key hints along the bottom of the window (F1).",
                Scope::Workspace,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(!p.ws.hide_hints),
                Some(|p, _, v| {
                    p.ws.hide_hints = !want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(true),
        ),
        // Charts.
        with_default(
            s(
                "charts.timeframe",
                Sec::Charts,
                "Bar period",
                "The timeframe a new chart starts with.",
                Scope::NewPanes,
                Normal,
                Choice(&TIMEFRAME_NAMES),
                |p, _, _| {
                    Value::Choice(choice(
                        &TIMEFRAME_NAMES,
                        p.app.new_panes.timeframe.as_deref().unwrap_or("1m"),
                    ))
                },
                Some(|p, _, v| {
                    let i = want_choice(v, TIMEFRAME_NAMES.len())?;
                    p.app.new_panes.timeframe = (i != 0).then(|| TIMEFRAME_NAMES[i].to_owned());
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        s(
            "charts.studies",
            Sec::Charts,
            "Studies",
            "What a new chart shows: set with \"Save as default for new charts\" in a chart's studies menu (Ctrl+I).",
            Scope::NewPanes,
            Normal,
            Info,
            |p, _, _| {
                Value::Text(if p.app.chart_defaults.is_some() {
                    "The saved default set".to_owned()
                } else {
                    "None".to_owned()
                })
            },
            None,
        ),
        // DOM.
        with_default(
            s(
                "dom.grouping",
                Sec::Dom,
                "Grouping",
                "Ticks per row in a new DOM (G cycles them in a DOM).",
                Scope::NewPanes,
                Normal,
                Choice(&GROUPING_NAMES),
                |p, _, _| {
                    let g = p.app.new_panes.dom_grouping.unwrap_or(1);
                    Value::Choice(GROUPINGS.iter().position(|x| *x == g).unwrap_or(0))
                },
                Some(|p, _, v| {
                    let i = want_choice(v, GROUPINGS.len())?;
                    p.app.new_panes.dom_grouping = (i != 0).then_some(GROUPINGS[i]);
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        with_default(
            s(
                "dom.trail",
                Sec::Dom,
                "Trail in new DOMs",
                "A new DOM starts with the price trail column on.",
                Scope::NewPanes,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(p.app.new_panes.dom_trail),
                Some(|p, _, v| {
                    p.app.new_panes.dom_trail = want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(false),
        ),
        s(
            "dom.columns",
            Sec::Dom,
            "Columns",
            "Shown, hidden and widths per DOM: right-click a column header (or the palette's \"Columns:\" commands).",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, _| Value::Text("Per DOM, from its column header".to_owned()),
            None,
        ),
        // Trading.
        with_default(
            s(
                "trading.pnl",
                Sec::Trading,
                "P&L display",
                "How positions and the trading panel show profit and loss.",
                Scope::App,
                Normal,
                Choice(PNL),
                |p, _, _| {
                    Value::Choice(
                        pnl_modes()
                            .iter()
                            .position(|m| *m == p.app.pnl_display)
                            .unwrap_or(3),
                    )
                },
                Some(|p, _, v| {
                    p.app.pnl_display = pnl_modes()[want_choice(v, 4)?];
                    Ok(())
                }),
            ),
            Value::Choice(3),
        ),
        with_default(
            s(
                "trading.disable_on_focus_loss",
                Sec::Trading,
                "Disable trading when a DOM loses focus",
                "Off by default: the trading switch stays as you set it.",
                Scope::App,
                Trading,
                Toggle,
                |p, _, _| Value::Bool(p.app.trading_prefs.disarm_on_focus_loss),
                Some(|p, _, v| {
                    p.app.trading_prefs.disarm_on_focus_loss = want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(false),
        ),
        with_default(
            s(
                "trading.disable_on_account_change",
                Sec::Trading,
                "Disable trading on an account change",
                "Off by default.",
                Scope::App,
                Trading,
                Toggle,
                |p, _, _| Value::Bool(p.app.trading_prefs.disarm_on_account_change),
                Some(|p, _, v| {
                    p.app.trading_prefs.disarm_on_account_change = want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(false),
        ),
        with_default(
            s(
                "trading.disable_on_contract_change",
                Sec::Trading,
                "Disable trading on a contract change",
                "Off by default (a roll, a picker switch, a linked pane).",
                Scope::App,
                Trading,
                Toggle,
                |p, _, _| Value::Bool(p.app.trading_prefs.disarm_on_contract_change),
                Some(|p, _, v| {
                    p.app.trading_prefs.disarm_on_contract_change = want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(false),
        ),
        with_default(
            s(
                "trading.default_qty",
                Sec::Trading,
                "Default quantity",
                "A new DOM's order size.",
                Scope::NewPanes,
                Trading,
                Number {
                    min: 1,
                    max: 999,
                    optional: false,
                    unit: "",
                },
                |p, _, _| Value::Number(Some(p.app.new_panes.default_qty.unwrap_or(1) as i64)),
                Some(|p, _, v| {
                    let n = match v {
                        Value::Number(n) => n,
                        _ => return Err("expected a number"),
                    };
                    let n = check_number(
                        Number {
                            min: 1,
                            max: 999,
                            optional: false,
                            unit: "",
                        },
                        n,
                    )?
                    .unwrap_or(1);
                    p.app.new_panes.default_qty = (n != 1).then_some(n as u32);
                    Ok(())
                }),
            ),
            Value::Number(Some(1)),
        ),
        with_default(
            s(
                "trading.bracket_units",
                Sec::Trading,
                "Bracket units",
                "Show bracket distances in ticks or points (stored in ticks, exact).",
                Scope::Account,
                Normal,
                Choice(UNITS),
                |p, t, _| {
                    let u = t.account.as_deref().map_or(Default::default(), |a| {
                        crate::brackets::units_for(&p.app.bracket_presets, a)
                    });
                    Value::Choice(usize::from(u == crate::brackets::BracketUnits::Points))
                },
                Some(|p, t, v| {
                    let a = t.account.clone().ok_or("choose an account first")?;
                    let i = want_choice(v, 2)?;
                    p.app.bracket_presets.entry(a).or_default().units = if i == 1 {
                        crate::brackets::BracketUnits::Points
                    } else {
                        crate::brackets::BracketUnits::Ticks
                    };
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        s(
            "trading.brackets",
            Sec::Trading,
            "Bracket presets",
            "Stop and target pairs per account and product. The DOM panel's Edit… edits the same list in place.",
            Scope::AccountProduct,
            Trading,
            Editor,
            |p, t, _| {
                let (Some(a), Some(prod)) = (&t.account, &t.product) else {
                    return Value::Text("Choose an account and product".to_owned());
                };
                let list = crate::brackets::presets_for(&p.app.bracket_presets, a, prod);
                Value::Text(if list.is_empty() {
                    "None".to_owned()
                } else {
                    list.iter()
                        .map(|b| format!("{}/{}", b.stop_ticks, b.target_ticks))
                        .collect::<Vec<_>>()
                        .join(", ")
                        + " ticks"
                })
            },
            None,
        ),
        s(
            "guard.max_daily_loss",
            Sec::Trading,
            "Daily loss limit",
            "Your own limit per trade date, in whole dollars (realized and open, fees included). Empty: none.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 1_000_000,
                optional: true,
                unit: "$",
            },
            g_loss,
            Some(s_loss),
        ),
        s(
            "guard.max_position",
            Sec::Trading,
            "Max position",
            "Contracts per product, worst case (position plus open orders on one side). Empty: none.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 10_000,
                optional: true,
                unit: "contracts",
            },
            g_pos,
            Some(s_pos),
        ),
        s(
            "guard.max_trades",
            Sec::Trading,
            "Max trades per day",
            "Round trips per trade date. Empty: none.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 10_000,
                optional: true,
                unit: "trades",
            },
            g_trades,
            Some(s_trades),
        ),
        s(
            "guard.losing_streak",
            Sec::Trading,
            "Cooldown after losses",
            "Losing trades in a row that start a cooldown. Empty: none.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 100,
                optional: true,
                unit: "losses",
            },
            g_streak,
            Some(s_streak),
        ),
        s(
            "guard.cooldown",
            Sec::Trading,
            "Cooldown length",
            "How long entries wait after the losing streak, in minutes.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 24 * 60,
                optional: false,
                unit: "min",
            },
            g_cool,
            Some(s_cool),
        ),
        s(
            "guard.no_entries_before_close",
            Sec::Trading,
            "No entries before the close",
            "No new entries this many minutes before the session closes. Empty: none.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 600,
                optional: true,
                unit: "min",
            },
            g_close,
            Some(s_close),
        ),
        // Accounts.
        s(
            "accounts.list",
            Sec::Accounts,
            "Trading accounts",
            "Paper accounts for now; live ones come with the connectors.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| {
                Value::Text(if f.accounts.is_empty() {
                    "Trading is not running".to_owned()
                } else {
                    f.accounts
                        .iter()
                        .map(|a| {
                            let mut s = a.name.clone();
                            if a.paper {
                                s.push_str(" (paper)");
                            }
                            if let Some(r) = &a.rules {
                                s.push_str(" · ");
                                s.push_str(r);
                            }
                            s
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            },
            None,
        ),
        // Data.
        s(
            "data.store",
            Sec::Data,
            "Store location",
            "Where recorded market data lives. Set with --store for now.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| Value::Text(f.store_path.clone()),
            None,
        ),
        s(
            "data.budget",
            Sec::Data,
            "Store budget",
            "The oldest trade dates are removed beyond it. Set with --store-budget-gb for now.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| {
                Value::Text(
                    f.store_budget_gb
                        .map_or_else(|| "None (no store)".to_owned(), |g| format!("{g} GiB")),
                )
            },
            None,
        ),
        s(
            "data.depth",
            Sec::Data,
            "Depth recorded for",
            "Products whose order book is stored. Set with --store-depth for now.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| {
                Value::Text(if f.depth_products.is_empty() {
                    "None".to_owned()
                } else {
                    f.depth_products.join(", ")
                })
            },
            None,
        ),
        // Keys.
        s(
            "keys.list",
            Sec::Keys,
            "Key bindings",
            "Every key and its command (editing comes later).",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, _| Value::Text(String::new()),
            None,
        ),
        // Diagnostics.
        with_default(
            s(
                "diagnostics.latency_hud",
                Sec::Diagnostics,
                "Latency HUD",
                "Feed, paint, order and ack latency in chart headers.",
                Scope::Workspace,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(p.ws.latency_hud),
                Some(|p, _, v| {
                    p.ws.latency_hud = want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(false),
        ),
        s(
            "diagnostics.folder",
            Sec::Diagnostics,
            "Settings and logs folder",
            "Export diagnostics (palette) bundles the logs with secrets removed.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| Value::Text(f.config_dir.clone()),
            None,
        ),
    ]
}

impl Setting {
    pub fn read_only(&self) -> bool {
        self.set.is_none()
    }

    pub fn choices(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::Choice(c) => c,
            _ => &[],
        }
    }

    /// Set `v`, validated by the setting's kind first.
    pub fn apply(&self, p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
        let set = self.set.ok_or("read-only")?;
        let v = match (self.kind, v) {
            (Kind::Number { .. }, Value::Number(n)) => Value::Number(check_number(self.kind, n)?),
            (Kind::Choice(c), Value::Choice(i)) if i >= c.len() => {
                return Err("not one of the choices");
            }
            (_, v) => v,
        };
        set(p, t, v)
    }

    /// Whether this setting's value differs from its default (the window's ↺).
    pub fn changed(&self, p: &Prefs, t: &Target, f: &Facts) -> bool {
        self.default
            .as_ref()
            .is_some_and(|d| *d != (self.get)(p, t, f))
    }

    /// The text a palette command or a search result shows for a value.
    pub fn value_text(&self, v: &Value) -> String {
        match (self.kind, v) {
            (Kind::Choice(c), Value::Choice(i)) => c.get(*i).copied().unwrap_or("?").to_owned(),
            (_, Value::Bool(b)) => if *b { "on" } else { "off" }.to_owned(),
            (Kind::Number { unit, .. }, Value::Number(Some(n))) => {
                let n = grouped(*n);
                match unit {
                    "$" => format!("${n}"),
                    "" => n,
                    u => format!("{n} {u}"),
                }
            }
            (_, Value::Number(None)) => "None".to_owned(),
            (_, Value::Text(t)) => t.clone(),
            _ => String::new(),
        }
    }
}

/// Whether changing setting `st` from `old` to `new` loosens trading risk (decision 0021:
/// it needs an explicit yes): a guardrail made looser or removed (`g0` to `g1`, the
/// account's guardrails before and after), an opt-in "disable trading" turned off, a larger
/// default quantity.
pub fn loosening(
    st: &Setting,
    old: &Value,
    new: &Value,
    g0: &lines_trading::Guardrails,
    g1: &lines_trading::Guardrails,
) -> bool {
    match st.key {
        k if k.starts_with("guard.") => loosens(g0, g1),
        k if k.starts_with("trading.disable_") => {
            matches!((old, new), (Value::Bool(true), Value::Bool(false)))
        }
        "trading.default_qty" => matches!(
            (old, new),
            (Value::Number(Some(a)), Value::Number(Some(b))) if b > a
        ),
        _ => false,
    }
}

/// Thousands separators: 1500 → "1,500".
pub fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn by_key(key: &str) -> Option<&'static Setting> {
    all().iter().find(|s| s.key == key)
}

pub fn index_of(key: &str) -> Option<usize> {
    all().iter().position(|s| s.key == key)
}

/// Settings matching a search, in table order: every word must appear in the name, the
/// description or the section (case ignored). Empty: none.
pub fn search(query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    all()
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let hay = format!(
                "{} {} {} {}",
                s.name,
                s.description,
                s.section.name(),
                s.choices().join(" ")
            )
            .to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .map(|(i, _)| i)
        .collect()
}

/// A setting's palette command: a choice or toggle sets that value directly ("Appearance:
/// theme Light"); any other setting opens the Settings window at it ("Settings: Daily loss
/// limit…"). Every setting has at least one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SettingCmd {
    pub index: u16,
    /// For choices: the choice; toggles: 0 off, 1 on; `u8::MAX`: open the window there.
    pub value: u8,
}

pub const OPEN: u8 = u8::MAX;

/// Settings the palette already sets through commands of their own.
const HAS_COMMAND: [&str; 3] = ["trading.pnl", "appearance.hints", "diagnostics.latency_hud"];

impl SettingCmd {
    pub fn setting(self) -> Option<&'static Setting> {
        all().get(self.index as usize)
    }

    /// The value this command sets, if it sets one.
    pub fn value(self) -> Option<Value> {
        let st = self.setting()?;
        match st.kind {
            _ if self.value == OPEN => None,
            Kind::Choice(_) => Some(Value::Choice(self.value as usize)),
            Kind::Toggle => Some(Value::Bool(self.value == 1)),
            _ => None,
        }
    }

    pub fn label(self) -> String {
        let Some(st) = self.setting() else {
            return "Settings".to_owned();
        };
        match self.value() {
            Some(v) => format!("{}: {} {}", st.section.name(), st.name, st.value_text(&v)),
            None => format!("Settings: {}…", st.name),
        }
    }
}

/// Every setting's palette commands.
pub fn palette() -> Vec<SettingCmd> {
    let mut v = Vec::new();
    for (i, st) in all().iter().enumerate() {
        let index = i as u16;
        match st.kind {
            // Settings with palette commands of their own (P&L display: …, Toggle key
            // hints, Toggle latency HUD): those are their palette commands.
            _ if HAS_COMMAND.contains(&st.key) => {}
            Kind::Choice(c) if !st.read_only() && st.scope != Scope::Account => {
                v.extend((0..c.len()).map(|k| SettingCmd {
                    index,
                    value: k as u8,
                }));
            }
            Kind::Toggle if !st.read_only() => {
                v.push(SettingCmd { index, value: 1 });
                v.push(SettingCmd { index, value: 0 });
            }
            _ => v.push(SettingCmd { index, value: OPEN }),
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn acct() -> Target {
        Target {
            account: Some("paper-eval".into()),
            account_name: None,
            product: Some("ES".into()),
        }
    }

    /// The new-chart timeframe choices are exactly the charts' timeframes, as they parse.
    #[test]
    fn timeframe_choices_parse_to_every_timeframe() {
        let parsed: Vec<_> = TIMEFRAME_NAMES
            .iter()
            .map(|n| lines_core::Timeframe::parse(n).expect(n))
            .collect();
        assert_eq!(parsed, lines_core::Timeframe::ALL.to_vec());
        assert_eq!(GROUPINGS.len(), GROUPING_NAMES.len());
    }

    #[test]
    fn keys_are_unique_and_every_section_has_settings() {
        let keys: HashSet<_> = all().iter().map(|s| s.key).collect();
        assert_eq!(keys.len(), all().len());
        for sec in Section::ALL {
            assert!(all().iter().any(|s| s.section == sec), "{sec:?}");
        }
        // Table order is section order (the window walks it once).
        let order: Vec<usize> = all()
            .iter()
            .map(|s| Section::ALL.iter().position(|x| *x == s.section).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn every_setting_has_a_palette_command() {
        let cmds = palette();
        for (i, st) in all().iter().enumerate() {
            assert!(
                HAS_COMMAND.contains(&st.key) || cmds.iter().any(|c| c.index as usize == i),
                "{}",
                st.key
            );
        }
        let labels: HashSet<_> = cmds.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), cmds.len(), "labels are distinct");
        assert!(labels.contains("Appearance: Theme Light"));
        assert!(labels.contains("Charts: Bar period 15m"));
        assert!(labels.contains("Settings: Daily loss limit…"));
    }

    #[test]
    fn defaults_read_back_and_reset() {
        let p = Prefs::default();
        let f = Facts::default();
        let t = acct();
        for st in all() {
            if let Some(d) = &st.default {
                assert_eq!(&(st.get)(&p, &t, &f), d, "{}", st.key);
                assert!(!st.changed(&p, &t, &f));
            }
        }
        // Set, then reset to the default: nothing is written (defaults stay absent).
        let mut q = p.clone();
        let theme = by_key("appearance.theme").unwrap();
        theme.apply(&mut q, &t, Value::Choice(1)).unwrap();
        assert!(theme.changed(&q, &t, &f));
        theme
            .apply(&mut q, &t, theme.default.clone().unwrap())
            .unwrap();
        assert_eq!(q, p);
    }

    #[test]
    fn numbers_validate_exactly() {
        let mut p = Prefs::default();
        let t = acct();
        let loss = by_key("guard.max_daily_loss").unwrap();
        assert_eq!(
            loss.apply(&mut p, &t, Value::Number(Some(0))),
            Err("too small")
        );
        loss.apply(&mut p, &t, Value::Number(Some(1500))).unwrap();
        assert_eq!(p.app.guardrails["paper-eval"].max_daily_loss, Some(1500));
        loss.apply(&mut p, &t, Value::Number(None)).unwrap();
        assert!(p.app.guardrails.is_empty(), "an empty set is dropped");
        assert_eq!(parse_number("$1,500"), Ok(Some(1500)));
        assert_eq!(parse_number(""), Ok(None));
        assert!(parse_number("1.5").is_err());
        let qty = by_key("trading.default_qty").unwrap();
        assert_eq!(
            qty.apply(&mut p, &t, Value::Number(None)),
            Err("a value is needed")
        );
        // Per-account settings need an account.
        assert!(
            loss.apply(&mut p, &Target::default(), Value::Number(Some(5)))
                .is_err()
        );
    }

    #[test]
    fn loosening_is_told_apart_from_tightening() {
        let old = lines_trading::Guardrails {
            max_daily_loss: Some(1000),
            max_position: Some(5),
            ..Default::default()
        };
        let tighter = lines_trading::Guardrails {
            max_position: Some(3),
            ..old.clone()
        };
        assert!(!loosens(&old, &tighter));
        let looser = lines_trading::Guardrails {
            max_daily_loss: Some(1500),
            ..old.clone()
        };
        assert!(loosens(&old, &looser));
        let removed = lines_trading::Guardrails {
            max_daily_loss: None,
            ..old.clone()
        };
        assert!(loosens(&old, &removed), "removing a limit loosens");
    }

    /// Look flags hold for the run; a field the trader sets is saved and drops its flag, the
    /// others stay as saved (a flag is never saved).
    #[test]
    fn look_flags_are_this_runs_only() {
        let saved = Appearance::default();
        let flags = Appearance {
            theme: Some(ThemeName::Light),
            ..Default::default()
        };
        let shown = saved.with_flags(flags);
        assert_eq!(shown.theme, Some(ThemeName::Light));
        let after = Appearance {
            hollow: Some(HollowName::Down),
            ..shown
        };
        let (s, f) = Appearance::changed(saved, flags, shown, after);
        assert_eq!(s.theme, None, "the flag isn't saved");
        assert_eq!(s.hollow, Some(HollowName::Down));
        assert_eq!(f.theme, Some(ThemeName::Light), "and still holds");
        assert_eq!(s.with_flags(f).theme, Some(ThemeName::Light));
        let after = Appearance {
            theme: Some(ThemeName::Dark),
            ..shown
        };
        let (s, f) = Appearance::changed(saved, flags, shown, after);
        assert_eq!((s.theme, f.theme), (Some(ThemeName::Dark), None));
    }

    /// Re-picking the value a default-valued flag shows saves nothing over a saved value;
    /// an undo restores only its field, never saving a flag.
    #[test]
    fn re_picks_and_undos_never_save_a_flag() {
        // Saved light, run with --theme dark (the default), the trader picks Dark.
        let saved = Appearance {
            theme: Some(ThemeName::Light),
            ..Default::default()
        };
        let flags = Appearance {
            theme: Some(ThemeName::Dark),
            ..Default::default()
        };
        let shown = saved.with_flags(flags);
        let picked = Appearance {
            theme: None,
            ..shown
        };
        let (s, f) = Appearance::changed(saved, flags, shown, picked);
        assert_eq!((s, f), (saved, flags), "nothing changed, nothing saved");

        // Run with --theme light, nothing saved: Dark, then palette Colorblind, then undo
        // the theme: the theme and its flag come back; the palette change stays.
        let saved = Appearance::default();
        let flags = Appearance {
            theme: Some(ThemeName::Light),
            ..Default::default()
        };
        let then = (saved, flags);
        let shown = saved.with_flags(flags);
        let (s1, f1) = Appearance::changed(
            saved,
            flags,
            shown,
            Appearance {
                theme: None,
                ..shown
            },
        );
        let shown1 = s1.with_flags(f1);
        let (s2, f2) = Appearance::changed(
            s1,
            f1,
            shown1,
            Appearance {
                palette: Some(PaletteName::Colorblind),
                ..shown1
            },
        );
        let (s3, f3) = Appearance::undo_field("appearance.theme", (s2, f2), then);
        assert_eq!(s3.theme, None, "the flag's Light is never saved");
        assert_eq!(f3.theme, Some(ThemeName::Light), "it holds again");
        assert_eq!(
            s3.palette,
            Some(PaletteName::Colorblind),
            "the later change stays"
        );
    }

    #[test]
    fn trading_loosenings_need_a_yes() {
        let g = lines_trading::Guardrails::default();
        let st = |k| by_key(k).unwrap();
        let (on, off) = (Value::Bool(true), Value::Bool(false));
        assert!(loosening(
            st("trading.disable_on_focus_loss"),
            &on,
            &off,
            &g,
            &g
        ));
        assert!(!loosening(
            st("trading.disable_on_focus_loss"),
            &off,
            &on,
            &g,
            &g
        ));
        let (one, fifty) = (Value::Number(Some(1)), Value::Number(Some(50)));
        assert!(loosening(st("trading.default_qty"), &one, &fifty, &g, &g));
        assert!(!loosening(st("trading.default_qty"), &fifty, &one, &g, &g));
        assert!(!loosening(
            st("appearance.theme"),
            &Value::Choice(0),
            &Value::Choice(1),
            &g,
            &g
        ));
        let tight = lines_trading::Guardrails {
            max_daily_loss: Some(500),
            ..Default::default()
        };
        let loss = st("guard.max_daily_loss");
        assert!(
            loosening(loss, &one, &fifty, &tight, &g),
            "removing a limit"
        );
        assert!(!loosening(loss, &one, &fifty, &g, &tight));
    }

    /// Guardrails saved under the account's id (as startup accepts) are read and edited there.
    #[test]
    fn guardrails_saved_by_id_are_found() {
        let mut p = Prefs::default();
        p.app.guardrails.insert(
            "paper-eval".into(),
            lines_trading::Guardrails {
                max_daily_loss: Some(1000),
                ..Default::default()
            },
        );
        let t = Target {
            account: Some("paper-eval".into()),
            account_name: Some("Paper 50K eval".into()),
            product: None,
        };
        let loss = by_key("guard.max_daily_loss").unwrap();
        assert_eq!(
            (loss.get)(&p, &t, &Facts::default()),
            Value::Number(Some(1000))
        );
        loss.apply(&mut p, &t, Value::Number(Some(800))).unwrap();
        assert_eq!(p.app.guardrails.len(), 1, "no second entry under the name");
        assert_eq!(p.app.guardrails["paper-eval"].max_daily_loss, Some(800));
        // What the core is sent for this account: the edited entry, never an empty one.
        assert_eq!(t.guardrails(&p).max_daily_loss, Some(800));
    }

    /// Every existing field still round-trips through the versioned format with the flattened
    /// unknown-keys map beside it.
    #[test]
    fn a_full_settings_file_round_trips() {
        let mut s = Settings {
            recents: vec![1, 2],
            favourites: vec![3],
            chart_defaults: Some(Default::default()),
            pnl_display: crate::pnl::PnlDisplay::Ticks,
            ..Default::default()
        };
        s.trading_prefs.disarm_on_focus_loss = true;
        s.guardrails.insert(
            "Paper".into(),
            lines_trading::Guardrails {
                max_daily_loss: Some(1000),
                losing_streak: Some(lines_trading::LosingStreak {
                    losses: 3,
                    cooldown_min: 15,
                }),
                ..Default::default()
            },
        );
        s.bracket_presets
            .entry("paper".into())
            .or_default()
            .products
            .insert(
                "ES".into(),
                vec![crate::brackets::NamedBracket {
                    name: "scalp".into(),
                    stop_ticks: 8,
                    target_ticks: 16,
                }],
            );
        s.appearance.theme = Some(ThemeName::Light);
        s.new_panes.dom_grouping = Some(5);
        s.unknown.insert("later".into(), serde_json::json!([1, 2]));
        let bytes = s.encode();
        let dir = std::env::temp_dir().join(format!("lines-settings-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, &bytes).unwrap();
        let (got, report, _) = Settings::load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(report, None);
        assert_eq!(got, s);
    }

    #[test]
    fn search_finds_by_name_description_section_and_choice() {
        let names = |q: &str| -> Vec<&str> { search(q).iter().map(|&i| all()[i].key).collect() };
        assert!(names("bracket").contains(&"trading.brackets"));
        assert!(names("bracket").contains(&"trading.bracket_units"));
        assert!(names("colorblind").contains(&"appearance.palette"));
        assert!(names("dom grouping").contains(&"dom.grouping"));
        assert!(names("").is_empty());
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn saved_form_omits_defaults_and_keeps_unknown_keys() {
        let mut p = Prefs::default();
        let t = acct();
        by_key("appearance.palette")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(1))
            .unwrap();
        by_key("dom.grouping")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(2))
            .unwrap();
        let json = String::from_utf8(p.app.encode()).unwrap();
        assert!(json.contains("\"palette\": \"colorblind\""), "{json}");
        assert!(json.contains("\"dom_grouping\": 5"), "{json}");
        assert!(!json.contains("theme"), "defaults aren't written: {json}");
        // A newer version's key survives a load and a save.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut o = v.as_object().unwrap().clone();
        o.insert("from_the_future".into(), serde_json::json!({"x": 1}));
        let back: Settings = serde_json::from_value(serde_json::Value::Object(o)).unwrap();
        let again = String::from_utf8(back.encode()).unwrap();
        assert!(again.contains("from_the_future"), "{again}");
        assert_eq!(back.appearance.palette, Some(PaletteName::Colorblind));
    }
}
