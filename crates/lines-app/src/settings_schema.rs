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
    Risk,
    Accounts,
    Data,
    Keys,
    Diagnostics,
}

impl Section {
    pub const ALL: [Section; 9] = [
        Section::Appearance,
        Section::Charts,
        Section::Dom,
        Section::Trading,
        Section::Risk,
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
            Section::Risk => "Risk",
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
    /// PAPER, SIM or LIVE (a broker's accounts carry their own kind).
    pub kind: &'static str,
    pub rules: Option<String>,
    /// The prop rules in force now, and looser ones waiting for the next trade date.
    pub rules_in_force: Option<lines_trading::rules::RulePreset>,
    pub rules_pending: Option<lines_trading::rules::PendingRules>,
    /// The guardrails in force now, and looser ones waiting for the next trade date.
    pub guard_in_force: lines_trading::Guardrails,
    pub guard_pending: Option<lines_trading::Guardrails>,
    /// A guardrail lockout in force: why ("6 of 6 trades today").
    pub lock: Option<String>,
    /// A prop-rule breach restricting the account: what ("drawdown reached").
    pub breach: Option<String>,
    /// The account's size (its starting balance, whole dollars): prop presets are for it.
    pub account_size: Option<i64>,
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

    /// An undo of look field `field`: that field back as it was saved and flagged (`then`),
    /// every other field as it is `now` (later changes stay).
    pub fn undo_field(
        field: LookField,
        now: (Appearance, Appearance),
        then: (Appearance, Appearance),
    ) -> (Appearance, Appearance) {
        let ((mut s, mut f), (s0, f0)) = (now, then);
        match field {
            LookField::Theme => (s.theme, f.theme) = (s0.theme, f0.theme),
            LookField::Palette => (s.palette, f.palette) = (s0.palette, f0.palette),
            LookField::Hollow => (s.hollow, f.hollow) = (s0.hollow, f0.hollow),
        }
        (s, f)
    }
}

/// The look's fields (a look flag can hold each); the settings that set them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LookField {
    Theme,
    Palette,
    Hollow,
}

impl LookField {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const ALL: [LookField; 3] = [LookField::Theme, LookField::Palette, LookField::Hollow];

    /// The look field setting `key` sets, if it sets one.
    pub fn of(key: &str) -> Option<LookField> {
        match key {
            "appearance.theme" => Some(LookField::Theme),
            "appearance.palette" => Some(LookField::Palette),
            "appearance.hollow" => Some(LookField::Hollow),
            _ => None,
        }
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

/// An account's prop rules in `settings.json`: a preset (by its name, as
/// `lines_trading::rules::presets` names it; absent: no prop rules) and optional overrides
/// of single values (absent: the preset's). Optional per account: an account without an
/// entry keeps the rules it has.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropRulesDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Whole dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_loss_limit: Option<i64>,
    /// Whole dollars (the preset's mode and lock stay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_drawdown: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_micro_contracts: Option<u32>,
    /// Whole dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_target: Option<i64>,
}

/// The presets the Risk section offers, in `lines_trading::rules::presets` order, by a short
/// name ("None" first).
pub const PRESET_CHOICES: &[&str] = &["None", "50K intraday", "50K EOD", "100K funded"];

impl PropRulesDoc {
    /// The rules this asks for: the preset with its overrides (`Ok(None)`: no prop rules).
    pub fn resolve(&self) -> Result<Option<lines_trading::rules::RulePreset>, String> {
        let Some(name) = &self.preset else {
            return Ok(None);
        };
        let mut p = lines_trading::rules::presets()
            .into_iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| format!("no prop-rule preset named {name:?}"))?;
        if let Some(v) = self.daily_loss_limit {
            p.daily_loss_limit = Some(v);
        }
        if let (Some(v), Some(dd)) = (self.trailing_drawdown, p.trailing_drawdown.as_mut()) {
            dd.amount = v;
        }
        if let Some(v) = self.max_micro_contracts {
            p.max_micro_contracts = Some(v);
        }
        if let Some(v) = self.profit_target {
            p.profit_target = Some(v);
        }
        Ok(Some(p))
    }
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
const TIMEFRAME_NAMES: [&str; 23] = [
    "1m", "2m", "3m", "5m", "10m", "15m", "30m", "1h", "2h", "4h", "D", "W", "100t", "233t",
    "500t", "1000t", "2000t", "15s", "30s", "500v", "1000v", "4r", "10r",
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

/// Set one prop-rule override for the account (empty: the preset's value).
fn set_prop(p: &mut Prefs, t: &Target, key: &str, v: Value) -> Result<(), &'static str> {
    let Value::Number(n) = v else {
        return Err("expected a number");
    };
    let k = [&t.account_name, &t.account]
        .into_iter()
        .flatten()
        .find(|k| p.app.prop_rules.contains_key(*k))
        .or(t.account_name.as_ref().or(t.account.as_ref()))
        .cloned()
        .ok_or("choose an account first")?;
    let doc = p.app.prop_rules.entry(k).or_default();
    if doc.preset.is_none() {
        return Err("choose a preset first");
    }
    match key {
        "prop.daily_loss" => doc.daily_loss_limit = n,
        "prop.drawdown" => doc.trailing_drawdown = n,
        "prop.max_micros" => doc.max_micro_contracts = n.map(|n| n as u32),
        "prop.profit_target" => doc.profit_target = n,
        _ => return Err("no such prop rule"),
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
            Sec::Risk,
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
            Sec::Risk,
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
            Sec::Risk,
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
            Sec::Risk,
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
            Sec::Risk,
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
            Sec::Risk,
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
        // Risk: lockouts and breaches, as the core reports them.
        s(
            "guard.status",
            Sec::Risk,
            "Guardrail lockout",
            "While locked out you can tighten your guardrails; loosening waits until the session resets.",
            Scope::Account,
            Trading,
            Info,
            |_, t, f| {
                Value::Text(match fact(f, t).and_then(|a| a.lock.clone()) {
                    Some(why) => format!("🔒 Locked out: {why}"),
                    None => "Not locked out".to_owned(),
                })
            },
            None,
        ),
        s(
            "prop.status",
            Sec::Risk,
            "Prop rules",
            "Approximations, not a firm's official rules. During a breach rules can be tightened, not loosened, until the session resets.",
            Scope::Account,
            Trading,
            Info,
            |_, t, f| {
                Value::Text(match fact(f, t).and_then(|a| a.breach.clone()) {
                    Some(why) => format!("🔒 Breached: {why}"),
                    None => "No breach".to_owned(),
                })
            },
            None,
        ),
        s(
            "prop.preset",
            Sec::Risk,
            "Prop preset",
            "The preset's rules for this account; the rows below override single values. The account size is fixed.",
            Scope::Account,
            Trading,
            Choice(PRESET_CHOICES),
            |p, t, f| {
                let name = match prop_doc(p, t) {
                    Some(d) => d.preset.clone(),
                    None => {
                        fact(f, t).and_then(|a| a.rules_in_force.as_ref().map(|r| r.name.clone()))
                    }
                };
                let presets = lines_trading::rules::presets();
                Value::Choice(
                    name.and_then(|n| presets.iter().position(|p| p.name == n))
                        .map_or(0, |k| k + 1),
                )
            },
            Some(|p, t, v| {
                let i = want_choice(v, PRESET_CHOICES.len())?;
                let key = t
                    .account_name
                    .clone()
                    .or(t.account.clone())
                    .ok_or("choose an account first")?;
                let preset = (i > 0).then(|| lines_trading::rules::presets()[i - 1].name.clone());
                // A new preset starts from its own values (overrides belong to the old one).
                p.app.prop_rules.insert(
                    key,
                    PropRulesDoc {
                        preset,
                        ..Default::default()
                    },
                );
                Ok(())
            }),
        ),
        s(
            "prop.daily_loss",
            Sec::Risk,
            "Prop daily loss limit",
            "Whole dollars per trade date. Empty: the preset's.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 1_000_000,
                optional: true,
                unit: "$",
            },
            |p, t, f| {
                Value::Number(
                    prop_field("prop.daily_loss", prop_rules_shown(p, t, f).as_ref()).flatten(),
                )
            },
            Some(|p, t, v| set_prop(p, t, "prop.daily_loss", v)),
        ),
        s(
            "prop.drawdown",
            Sec::Risk,
            "Prop trailing drawdown",
            "Whole dollars (the preset's mode stays). Empty: the preset's.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 1_000_000,
                optional: true,
                unit: "$",
            },
            |p, t, f| {
                Value::Number(
                    prop_field("prop.drawdown", prop_rules_shown(p, t, f).as_ref()).flatten(),
                )
            },
            Some(|p, t, v| set_prop(p, t, "prop.drawdown", v)),
        ),
        s(
            "prop.max_micros",
            Sec::Risk,
            "Prop max micro contracts",
            "One standard contract counts as 10. Empty: the preset's.",
            Scope::Account,
            Trading,
            Number {
                min: 1,
                max: 100_000,
                optional: true,
                unit: "micros",
            },
            |p, t, f| {
                Value::Number(
                    prop_field("prop.max_micros", prop_rules_shown(p, t, f).as_ref()).flatten(),
                )
            },
            Some(|p, t, v| set_prop(p, t, "prop.max_micros", v)),
        ),
        s(
            "prop.profit_target",
            Sec::Risk,
            "Prop profit target",
            "Whole dollars; shown in the DOM's rule meters, doesn't restrict orders. Empty: the preset's.",
            Scope::Account,
            Normal,
            Number {
                min: 1,
                max: 10_000_000,
                optional: true,
                unit: "$",
            },
            |p, t, f| {
                Value::Number(
                    prop_field("prop.profit_target", prop_rules_shown(p, t, f).as_ref()).flatten(),
                )
            },
            Some(|p, t, v| set_prop(p, t, "prop.profit_target", v)),
        ),
        // Accounts.
        s(
            "accounts.list",
            Sec::Accounts,
            "Trading accounts",
            "Paper accounts, plus each broker's (read-only while observed).",
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
                            s.push_str(match a.kind {
                                "PAPER" => " (paper)",
                                "SIM" => " (sim)",
                                _ => " (live)",
                            });
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
        // Prop rows: the caller compares the resolved rules (`prop_loosens`).
        k if k.starts_with("prop.") => false,
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

/// This account's prop-rule entry in settings (by name, else id), if any.
fn prop_doc<'a>(p: &'a Prefs, t: &Target) -> Option<&'a PropRulesDoc> {
    [&t.account_name, &t.account]
        .into_iter()
        .flatten()
        .find_map(|k| p.app.prop_rules.get(k))
}

/// The account's facts (what the trading core reports).
fn fact<'a>(f: &'a Facts, t: &Target) -> Option<&'a AccountFact> {
    f.accounts
        .iter()
        .find(|a| Some(&a.id) == t.account.as_ref())
}

/// The prop rules the Risk rows show for this account: its settings entry resolved, else the
/// rules in force.
pub fn prop_rules_shown(
    p: &Prefs,
    t: &Target,
    f: &Facts,
) -> Option<lines_trading::rules::RulePreset> {
    match prop_doc(p, t) {
        Some(d) => d.resolve().ok().flatten(),
        None => fact(f, t).and_then(|a| a.rules_in_force.clone()),
    }
}

/// Before a prop-rule change for an account that has no settings entry yet: start one from
/// the rules in force (their preset, and any value that differs from it as an override), so
/// an override builds on what the trader sees.
pub fn seed_prop_doc(p: &mut Prefs, t: &Target, f: &Facts) {
    if prop_doc(p, t).is_some() {
        return;
    }
    let (Some(key), Some(r)) = (
        t.account_name.clone().or(t.account.clone()),
        fact(f, t).and_then(|a| a.rules_in_force.clone()),
    ) else {
        return;
    };
    let Some(base) = lines_trading::rules::presets()
        .into_iter()
        .find(|b| b.name == r.name)
    else {
        return;
    };
    let differs = |a: Option<i64>, b: Option<i64>| (a != b).then_some(a).flatten();
    p.app.prop_rules.insert(
        key,
        PropRulesDoc {
            preset: Some(r.name.clone()),
            daily_loss_limit: differs(r.daily_loss_limit, base.daily_loss_limit),
            trailing_drawdown: differs(
                r.trailing_drawdown.as_ref().map(|d| d.amount),
                base.trailing_drawdown.as_ref().map(|d| d.amount),
            ),
            max_micro_contracts: differs(
                r.max_micro_contracts.map(i64::from),
                base.max_micro_contracts.map(i64::from),
            )
            .map(|n| n as u32),
            profit_target: differs(r.profit_target, base.profit_target),
        },
    );
}

/// Whether the prop rules asked for in `after` loosen those in `before` for this account (a
/// loosening asks first and waits for the next trade date).
pub fn prop_loosens(before: &Prefs, after: &Prefs, t: &Target, f: &Facts) -> bool {
    let (a, b) = (
        prop_rules_shown(before, t, f),
        prop_rules_shown(after, t, f),
    );
    lines_trading::rules::tightest_rules(a.as_ref(), b.as_ref()) != b
}

/// A guardrail or prop-rule field, as the core reports it, for the status line.
fn guard_field(key: &str, g: &lines_trading::Guardrails) -> Option<Option<i64>> {
    Some(match key {
        "guard.max_daily_loss" => g.max_daily_loss,
        "guard.max_position" => g.max_position,
        "guard.max_trades" => g.max_trades_per_day.map(i64::from),
        "guard.losing_streak" => g.losing_streak.map(|s| i64::from(s.losses)),
        "guard.cooldown" => g.losing_streak.map(|s| i64::from(s.cooldown_min)),
        "guard.no_entries_before_close" => g.no_entries_before_close_min.map(i64::from),
        _ => return None,
    })
}

fn prop_field(key: &str, r: Option<&lines_trading::rules::RulePreset>) -> Option<Option<i64>> {
    Some(match key {
        "prop.daily_loss" => r.and_then(|r| r.daily_loss_limit),
        "prop.drawdown" => r.and_then(|r| r.trailing_drawdown.as_ref().map(|d| d.amount)),
        "prop.max_micros" => r.and_then(|r| r.max_micro_contracts.map(i64::from)),
        "prop.profit_target" => r.and_then(|r| r.profit_target),
        _ => return None,
    })
}

/// The row's line about what the trading core has in force, when it differs from the value
/// shown: "In force $1,000 now · $1,500 from the next session" (a looser value waiting), or
/// "In force 1 now" (set elsewhere, e.g. a script, or not sent yet). Nothing when they agree.
pub fn status(st: &Setting, p: &Prefs, t: &Target, f: &Facts) -> Option<String> {
    let a = fact(f, t)?;
    let (now, next) = if st.key.starts_with("guard.") {
        (
            guard_field(st.key, &a.guard_in_force)?,
            a.guard_pending
                .as_ref()
                .and_then(|g| guard_field(st.key, g)),
        )
    } else if st.key.starts_with("prop.") {
        (
            prop_field(st.key, a.rules_in_force.as_ref())?,
            a.rules_pending
                .as_ref()
                .and_then(|r| prop_field(st.key, r.rules.as_ref())),
        )
    } else {
        return None;
    };
    let shown = match (st.get)(p, t, f) {
        Value::Number(n) => n,
        _ => return None,
    };
    let text = |n: Option<i64>| st.value_text(&Value::Number(n));
    match next {
        Some(next) if next != now => Some(format!(
            "In force {} now · {} from the next session",
            text(now),
            text(next)
        )),
        _ if now != shown => Some(format!("In force {} now", text(now))),
        _ => None,
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

    /// Absent means the default, and the defaults are exactly the app's look when nothing is
    /// saved and no flag is given (the old `--theme dark --palette standard --hollow up`): the
    /// as-shown compare in `Appearance::changed` relies on it.
    #[test]
    fn absent_look_fields_are_the_apps_fallback() {
        use crate::theme::{HollowBodies, Palette, Theme};
        assert_eq!(ThemeName::default(), ThemeName::Dark);
        assert_eq!(PaletteName::default(), PaletteName::Standard);
        assert_eq!(HollowName::default(), HollowName::Up);
        assert_eq!(Palette::default(), Palette::Standard);
        assert_eq!(HollowBodies::default(), HollowBodies::Up);
        let mut fallback = Theme::dark(Palette::Standard);
        fallback.hollow = HollowBodies::Up;
        assert_eq!(theme_for(&Appearance::default()), fallback);
        // Each field's absent value reads as its default, and the window's reset value is it.
        let p = Prefs::default();
        let (t, f) = (Target::default(), Facts::default());
        for field in LookField::ALL {
            let st = all()
                .iter()
                .find(|s| LookField::of(s.key) == Some(field))
                .expect("a setting per look field");
            assert_eq!((st.get)(&p, &t, &f), Value::Choice(0), "{}", st.key);
            assert_eq!(st.default, Some(Value::Choice(0)), "{}", st.key);
        }
    }

    /// Every look setting in the table is a look field, and an undo of it changes that
    /// field (and no other).
    #[test]
    fn every_look_setting_undoes_its_own_field() {
        let mut seen = Vec::new();
        for st in all().iter().filter(|s| s.section == Section::Appearance) {
            let Some(field) = LookField::of(st.key) else {
                assert_eq!(st.scope, Scope::Workspace, "{}: not a look field", st.key);
                continue;
            };
            seen.push(field);
            // `then`: this field saved non-default and flagged; `now`: everything default.
            let mut then_s = Appearance::default();
            let mut then_f = Appearance::default();
            match field {
                LookField::Theme => {
                    then_s.theme = Some(ThemeName::Light);
                    then_f.theme = Some(ThemeName::Dark);
                }
                LookField::Palette => {
                    then_s.palette = Some(PaletteName::Colorblind);
                    then_f.palette = Some(PaletteName::Standard);
                }
                LookField::Hollow => {
                    then_s.hollow = Some(HollowName::Down);
                    then_f.hollow = Some(HollowName::Up);
                }
            }
            let now = (Appearance::default(), Appearance::default());
            let (s, f) = Appearance::undo_field(field, now, (then_s, then_f));
            assert_eq!((s, f), (then_s, then_f), "{}", st.key);
            // Other fields are left as they are now.
            for other in LookField::ALL.into_iter().filter(|o| *o != field) {
                let (s2, _) = Appearance::undo_field(other, now, (then_s, then_f));
                assert_eq!(s2, Appearance::default(), "{} via {other:?}", st.key);
            }
        }
        assert_eq!(
            seen,
            LookField::ALL.to_vec(),
            "every look field has its setting"
        );
    }

    /// Re-picking the value a default-valued flag shows saves nothing over a saved value;
    /// an undo restores only its field, never saving a flag.
    #[test]
    fn re_picks_and_undos_never_save_a_flag() {
        // Every field: saved non-default, run with the default as a flag, pick the shown
        // (default) value: nothing is saved over, the flag stays.
        let cases = [
            (
                Appearance {
                    theme: Some(ThemeName::Light),
                    ..Default::default()
                },
                Appearance {
                    theme: Some(ThemeName::Dark),
                    ..Default::default()
                },
            ),
            (
                Appearance {
                    palette: Some(PaletteName::Colorblind),
                    ..Default::default()
                },
                Appearance {
                    palette: Some(PaletteName::Standard),
                    ..Default::default()
                },
            ),
            (
                Appearance {
                    hollow: Some(HollowName::Down),
                    ..Default::default()
                },
                Appearance {
                    hollow: Some(HollowName::Up),
                    ..Default::default()
                },
            ),
        ];
        for (saved, flags) in cases {
            let shown = saved.with_flags(flags);
            // The setters store the default as absent.
            let picked = Appearance {
                theme: shown.theme.filter(|t| *t != ThemeName::default()),
                palette: shown.palette.filter(|p| *p != PaletteName::default()),
                hollow: shown.hollow.filter(|h| *h != HollowName::default()),
            };
            let (s, f) = Appearance::changed(saved, flags, shown, picked);
            assert_eq!((s, f), (saved, flags), "{saved:?} {flags:?}");
        }
        // Nothing saved, no flag, the default picked explicitly: nothing to save.
        let none = Appearance::default();
        let explicit = Appearance {
            theme: Some(ThemeName::Dark),
            palette: Some(PaletteName::Standard),
            hollow: Some(HollowName::Up),
        };
        assert_eq!(
            Appearance::changed(none, none, none, explicit),
            (none, none),
            "the default picked over the default changes nothing"
        );

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
        let (s3, f3) = Appearance::undo_field(LookField::Theme, (s2, f2), then);
        assert_eq!(s3.theme, None, "the flag's Light is never saved");
        assert_eq!(f3.theme, Some(ThemeName::Light), "it holds again");
        assert_eq!(
            s3.palette,
            Some(PaletteName::Colorblind),
            "the later change stays"
        );
    }

    /// The preset choices follow `lines_trading::rules::presets`, in order.
    #[test]
    fn preset_choices_match_the_presets() {
        let presets = lines_trading::rules::presets();
        assert_eq!(PRESET_CHOICES.len(), presets.len() + 1);
        assert_eq!(PRESET_CHOICES[0], "None");
        for (short, p) in PRESET_CHOICES[1..].iter().zip(&presets) {
            let size = &short[..short.find(' ').unwrap()];
            assert!(p.name.starts_with(size), "{short} vs {}", p.name);
        }
        assert!(presets[0].name.contains("intraday") && PRESET_CHOICES[1].contains("intraday"));
        assert!(presets[1].name.contains("end-of-day") && PRESET_CHOICES[2].contains("EOD"));
    }

    fn risk_target() -> Target {
        Target {
            account: Some("paper-eval".into()),
            account_name: Some("Paper 50K eval".into()),
            product: None,
        }
    }

    /// A preset plus single overrides, saved by account name and resolved exactly; a row
    /// shows the override, else the preset's value; an override needs a preset.
    #[test]
    fn prop_rules_are_a_preset_and_overrides() {
        let mut p = Prefs::default();
        let (t, f) = (risk_target(), Facts::default());
        let loss = by_key("prop.daily_loss").unwrap();
        assert_eq!(
            loss.apply(&mut p, &t, Value::Number(Some(800))),
            Err("choose a preset first")
        );
        by_key("prop.preset")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(2))
            .unwrap();
        assert_eq!(
            (loss.get)(&p, &t, &f),
            Value::Number(Some(1_000)),
            "the preset's"
        );
        loss.apply(&mut p, &t, Value::Number(Some(800))).unwrap();
        assert_eq!((loss.get)(&p, &t, &f), Value::Number(Some(800)));
        let doc = &p.app.prop_rules["Paper 50K eval"];
        let r = doc.resolve().unwrap().unwrap();
        assert_eq!(r.daily_loss_limit, Some(800));
        assert_eq!(r.max_micro_contracts, Some(50), "the rest is the preset's");
        // Back to the preset's value.
        loss.apply(&mut p, &t, Value::Number(None)).unwrap();
        assert_eq!((loss.get)(&p, &t, &f), Value::Number(Some(1_000)));
        // None: no prop rules.
        by_key("prop.preset")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(0))
            .unwrap();
        assert_eq!(p.app.prop_rules["Paper 50K eval"].resolve(), Ok(None));
        // An unknown preset name (another version's) doesn't resolve.
        let odd = PropRulesDoc {
            preset: Some("200K whatever".into()),
            ..Default::default()
        };
        assert!(odd.resolve().is_err());
    }

    /// An account whose rules came from its starting preset (no settings entry): an override
    /// starts from those rules.
    #[test]
    fn an_override_starts_from_the_rules_in_force() {
        let t = risk_target();
        let mut f = Facts::default();
        let eod = lines_trading::rules::presets()[1].clone();
        f.accounts.push(AccountFact {
            id: "paper-eval".into(),
            name: "Paper 50K eval".into(),
            rules_in_force: Some(lines_trading::rules::RulePreset {
                max_micro_contracts: Some(20),
                ..eod.clone()
            }),
            ..Default::default()
        });
        let mut p = Prefs::default();
        let loss = by_key("prop.daily_loss").unwrap();
        assert_eq!((loss.get)(&p, &t, &f), Value::Number(Some(1_000)));
        seed_prop_doc(&mut p, &t, &f);
        loss.apply(&mut p, &t, Value::Number(Some(800))).unwrap();
        let r = p.app.prop_rules["Paper 50K eval"]
            .resolve()
            .unwrap()
            .unwrap();
        assert_eq!(r.name, eod.name);
        assert_eq!(r.daily_loss_limit, Some(800));
        assert_eq!(
            r.max_micro_contracts,
            Some(20),
            "the in-force override is kept"
        );
    }

    /// Raising a prop limit or removing the rules loosens (asks, waits); lowering doesn't.
    #[test]
    fn prop_loosening_is_told_apart() {
        let (t, f) = (risk_target(), Facts::default());
        let mut a = Prefs::default();
        by_key("prop.preset")
            .unwrap()
            .apply(&mut a, &t, Value::Choice(2))
            .unwrap();
        let loss = by_key("prop.daily_loss").unwrap();
        let mut lower = a.clone();
        loss.apply(&mut lower, &t, Value::Number(Some(500)))
            .unwrap();
        assert!(!prop_loosens(&a, &lower, &t, &f));
        let mut higher = a.clone();
        loss.apply(&mut higher, &t, Value::Number(Some(1_500)))
            .unwrap();
        assert!(prop_loosens(&a, &higher, &t, &f));
        let mut none = a.clone();
        by_key("prop.preset")
            .unwrap()
            .apply(&mut none, &t, Value::Choice(0))
            .unwrap();
        assert!(
            prop_loosens(&a, &none, &t, &f),
            "removing the rules loosens"
        );
    }

    /// The row's line says what is in force when the core holds a looser value back.
    #[test]
    fn rows_say_what_is_in_force_and_what_waits() {
        let t = risk_target();
        let mut f = Facts::default();
        f.accounts.push(AccountFact {
            id: "paper-eval".into(),
            name: "Paper 50K eval".into(),
            guard_in_force: lines_trading::Guardrails {
                max_daily_loss: Some(1_000),
                ..Default::default()
            },
            guard_pending: Some(lines_trading::Guardrails {
                max_daily_loss: Some(1_500),
                ..Default::default()
            }),
            lock: Some("6 of 6 trades today".into()),
            ..Default::default()
        });
        assert_eq!(
            status(
                by_key("guard.max_daily_loss").unwrap(),
                &Prefs::default(),
                &t,
                &f
            )
            .as_deref(),
            Some("In force $1,000 now · $1,500 from the next session")
        );
        assert_eq!(
            status(
                by_key("guard.max_position").unwrap(),
                &Prefs::default(),
                &t,
                &f
            ),
            None
        );
        let lock = by_key("guard.status").unwrap();
        assert_eq!(
            (lock.get)(&Prefs::default(), &t, &f),
            Value::Text("🔒 Locked out: 6 of 6 trades today".into())
        );
        // Nothing waiting, but settings say none while the core has $1,000 (set elsewhere):
        // the row says what is in force.
        f.accounts[0].guard_pending = None;
        let daily = by_key("guard.max_daily_loss").unwrap();
        assert_eq!(
            status(daily, &Prefs::default(), &t, &f).as_deref(),
            Some("In force $1,000 now")
        );
        // And nothing when they agree.
        let mut p = Prefs::default();
        daily.apply(&mut p, &t, Value::Number(Some(1_000))).unwrap();
        assert_eq!(status(daily, &p, &t, &f), None);
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
        s.prop_rules.insert(
            "Paper".into(),
            PropRulesDoc {
                preset: Some(lines_trading::rules::presets()[1].name.clone()),
                daily_loss_limit: Some(800),
                ..Default::default()
            },
        );
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
