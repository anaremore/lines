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

/// The Settings window's sections, in order, under the sidebar's groups ([`Section::group`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    /// App: updates, the time zone.
    General,
    Appearance,
    Keys,
    /// Panes.
    Charts,
    Dom,
    /// Trading: app-wide order defaults (and the bracket presets, per account and product).
    OrderEntry,
    /// Everything about one account: guardrails, the news blackout, prop rules, reset.
    Accounts,
    /// Data: the sources (decision 0030).
    Connections,
    DataFolder,
    /// The version, rolling back an update (decision 0031) and diagnostics.
    About,
}

impl Section {
    pub const ALL: [Section; 10] = [
        Section::General,
        Section::Appearance,
        Section::Keys,
        Section::Charts,
        Section::Dom,
        Section::OrderEntry,
        Section::Accounts,
        Section::Connections,
        Section::DataFolder,
        Section::About,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Keys => "Keys",
            Section::Charts => "Charts",
            Section::Dom => "DOM",
            Section::OrderEntry => "Order entry",
            Section::Accounts => "Accounts",
            Section::Connections => "Connections",
            Section::DataFolder => "Data folder",
            Section::About => "About",
        }
    }

    /// The sidebar group it sits under (About stands alone).
    pub fn group(self) -> &'static str {
        match self {
            Section::General | Section::Appearance | Section::Keys => "APP",
            Section::Charts | Section::Dom => "PANES",
            Section::OrderEntry | Section::Accounts => "TRADING",
            Section::Connections | Section::DataFolder => "DATA",
            Section::About => "",
        }
    }

    /// The name `--settings` takes: lowercase, words joined by '-' ("order-entry").
    #[cfg_attr(not(any(test, feature = "screenshot")), allow(dead_code))]
    pub fn slug(self) -> String {
        self.name().to_lowercase().replace(' ', "-")
    }

    /// A section by its name or slug (case, spaces, '-' and '_' ignored), or by a name the
    /// window had before its sections were regrouped: `risk` → Accounts, `trading` → Order
    /// entry, `diagnostics` → About, `data` → Data folder.
    #[cfg_attr(not(any(test, feature = "screenshot")), allow(dead_code))]
    pub fn parse(s: &str) -> Option<Section> {
        let norm = |x: &str| -> String {
            x.chars()
                .filter(|c| !matches!(c, ' ' | '-' | '_'))
                .flat_map(char::to_lowercase)
                .collect()
        };
        let want = norm(s);
        if let Some(sec) = Section::ALL.into_iter().find(|x| norm(x.name()) == want) {
            return Some(sec);
        }
        match want.as_str() {
            "risk" => Some(Section::Accounts),
            "trading" => Some(Section::OrderEntry),
            "diagnostics" => Some(Section::About),
            "data" => Some(Section::DataFolder),
            _ => None,
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
    /// The quiet word a row shows after its label where its value isn't app-wide (decision
    /// 0021: no hidden inheritance). `None` for app-wide settings (the default), per-account
    /// ones (the account picker above them says which account) and read-only facts (shown,
    /// not set: nothing inherits them).
    pub fn label(self) -> Option<&'static str> {
        match self {
            Scope::App | Scope::Account | Scope::AccountProduct | Scope::ReadOnly => None,
            Scope::Workspace => Some("This workspace"),
            Scope::NewPanes => Some("New panes"),
        }
    }
}

/// The Connections section's list of sources (`connections_view` draws it).
pub const CONNECTIONS_LIST: &str = "connections.list";
/// The public sources' switches: rows for search and the palette, drawn on their cards.
pub const SOURCE_SWITCHES: [&str; 3] = ["data.coinbase", "data.kraken", "data.databento"];

/// Trading-affecting settings: loosening them asks first (decision 0021). The window shows it
/// once, by their place (the sidebar's Trading group), not with a mark on every row.
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
    /// The products chosen in a [`Kind::Products`] setting (catalog symbols, and any saved
    /// symbol this build doesn't know).
    List(Vec<String>),
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
    /// Any number of catalog products, one check per product (`settings_view` renders a chip
    /// per product; Left/Right move between them, Space toggles).
    Products,
    /// Text only.
    Info,
}

/// Facts the window shows but doesn't own, gathered when it opens.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// Settings › Data folder's folder (`crate::data_dir::facts`).
    pub folder: crate::data_dir::FolderFacts,
    /// The store settings this run started with, and the store flags given.
    pub store_start: crate::data_dir::StorePrefs,
    pub store_flags: crate::data_dir::StoreFlags,
    pub accounts: Vec<AccountFact>,
    pub config_dir: String,
    /// This build's version.
    pub version: String,
    /// Whether this build can update itself (it has the project's update key).
    pub can_update: bool,
    /// The version a rollback returns to, if the last update kept one.
    pub rollback: Option<String>,
    /// This run's look flags (`--theme`, `--palette`, `--hollow`): a look row one rules shows
    /// the flag's value and has no ● or ↺ (nothing is saved).
    pub look_flags: Appearance,
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
    /// Theme: System. Follow the OS appearance (`Some(true)`); `theme` keeps the last explicit
    /// choice, which a build without System still reads. A separate field rather than a third
    /// `theme` value, so an older build loads the file unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_os: Option<bool>,
    /// The light appearance's palette; absent: `palette`'s (one palette for both, as before
    /// there were two). `palette` is the dark appearance's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_palette: Option<PaletteName>,
}

impl Appearance {
    pub fn is_default(&self) -> bool {
        *self == Appearance::default()
    }

    /// `self` (what is saved) with this run's look flags over it. A theme flag sets both
    /// `theme` and `follow_os`, so it holds over a saved System too.
    pub fn with_flags(self, flags: Appearance) -> Appearance {
        Appearance {
            theme: flags.theme.or(self.theme),
            palette: flags.palette.or(self.palette),
            hollow: flags.hollow.or(self.hollow),
            follow_os: flags.follow_os.or(self.follow_os),
            // `--palette` sets both; with no light palette of its own saved, light shows as
            // "Same as dark" (the same colors, and what the trader set).
            light_palette: match flags.light_palette {
                Some(l) if flags.palette == Some(l) && self.light_palette.is_none() => None,
                Some(l) => Some(l),
                None => self.light_palette,
            },
        }
    }

    /// The Theme setting's choice: System when following the OS, else the saved theme.
    pub fn theme_choice(&self) -> ThemeChoice {
        if self.follow_os == Some(true) {
            return ThemeChoice::System;
        }
        match self.theme.unwrap_or_default() {
            ThemeName::Dark => ThemeChoice::Dark,
            ThemeName::Light => ThemeChoice::Light,
        }
    }

    /// Set the Theme choice. System keeps `theme` (the last explicit choice); the defaults
    /// are stored as absent.
    pub fn set_theme_choice(&mut self, c: ThemeChoice) {
        match c {
            ThemeChoice::System => self.follow_os = Some(true),
            ThemeChoice::Dark | ThemeChoice::Light => {
                self.follow_os = None;
                self.theme = (c == ThemeChoice::Light).then_some(ThemeName::Light);
            }
        }
    }

    /// The palette the dark or the light appearance shows.
    pub fn palette_for(&self, dark: bool) -> PaletteName {
        if dark {
            self.palette
        } else {
            self.light_palette.or(self.palette)
        }
        .unwrap_or_default()
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
        if after.theme_choice() != before.theme_choice() {
            // System keeps what is saved as the explicit theme (never a flag's).
            if after.theme_choice() == ThemeChoice::System {
                s.follow_os = Some(true);
            } else {
                s.theme = after.theme;
                s.follow_os = None;
            }
            (f.theme, f.follow_os) = (None, None);
        }
        if after.palette.unwrap_or_default() != before.palette.unwrap_or_default() {
            s.palette = after.palette;
            f.palette = None;
            // `--palette` holds over both palettes; with no light palette of its own saved,
            // light follows the dark one the trader just set ("Same as dark").
            if s.light_palette.is_none() {
                f.light_palette = None;
            }
        }
        if after.hollow.unwrap_or_default() != before.hollow.unwrap_or_default() {
            s.hollow = after.hollow;
            f.hollow = None;
        }
        // Absent ("same as dark") and an explicit palette differ, so compared as stored.
        if after.light_palette != before.light_palette {
            s.light_palette = after.light_palette;
            f.light_palette = None;
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
            LookField::Theme => {
                (s.theme, f.theme) = (s0.theme, f0.theme);
                (s.follow_os, f.follow_os) = (s0.follow_os, f0.follow_os);
            }
            LookField::LightPalette => {
                (s.light_palette, f.light_palette) = (s0.light_palette, f0.light_palette);
            }
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
    LightPalette,
    Hollow,
}

impl LookField {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const ALL: [LookField; 4] = [
        LookField::Theme,
        LookField::Palette,
        LookField::LightPalette,
        LookField::Hollow,
    ];

    /// Whether `a` sets this field.
    pub fn set_in(self, a: &Appearance) -> bool {
        match self {
            // `--theme system` sets `follow_os`, not `theme`.
            LookField::Theme => a.theme.is_some() || a.follow_os.is_some(),
            LookField::Palette => a.palette.is_some(),
            LookField::LightPalette => a.light_palette.is_some(),
            LookField::Hollow => a.hollow.is_some(),
        }
    }

    /// The look field setting `key` sets, if it sets one.
    pub fn of(key: &str) -> Option<LookField> {
        match key {
            "appearance.theme" => Some(LookField::Theme),
            "appearance.palette" => Some(LookField::Palette),
            "appearance.light_palette" => Some(LookField::LightPalette),
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

/// The Theme setting (Settings › Appearance, its palette commands, `--theme`): stored as
/// `Appearance::theme` and `Appearance::follow_os`, never itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    Dark,
    Light,
    /// Follow the OS appearance, live.
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System];
}

/// What a report of the OS appearance does (a window's report, or the one read at startup).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OsReport {
    /// Not taken: a screenshot (it never follows the OS), or nothing changed.
    Ignore,
    /// Remembered for when System is chosen; nothing shown changes.
    Remember,
    /// Remembered, and every window changes to it (Theme: System).
    Apply,
}

/// The decision for an OS appearance report: `frozen` (screenshot mode) never takes one; a
/// report of the appearance already known changes nothing; otherwise it is remembered, and
/// applied when the choice (flags included) is System.
pub fn os_report(frozen: bool, known_dark: bool, dark: bool, choice: ThemeChoice) -> OsReport {
    if frozen || known_dark == dark {
        OsReport::Ignore
    } else if choice == ThemeChoice::System {
        OsReport::Apply
    } else {
        OsReport::Remember
    }
}

/// Under Theme: System, what is in effect ("Now: Dark · Standard"); nothing for Dark or Light.
pub fn system_now(a: &Appearance, os_dark: bool) -> Option<String> {
    (a.theme_choice() == ThemeChoice::System).then(|| {
        format!(
            "Now: {} · {}",
            if os_dark { "Dark" } else { "Light" },
            PALETTES[a.palette_for(os_dark) as usize]
        )
    })
}

/// For the palette rows (`appearance.palette`, `appearance.light_palette`): whether the
/// row's value is the one shown now. Dark's is in use in dark, and in light while Light
/// palette is Same as dark; light's only in light. `None` for any other setting.
pub fn palette_in_use(key: &str, a: &Appearance, os_dark: bool) -> Option<bool> {
    let dark = match a.theme_choice() {
        ThemeChoice::Dark => true,
        ThemeChoice::Light => false,
        ThemeChoice::System => os_dark,
    };
    match key {
        "appearance.palette" => Some(dark || a.light_palette.is_none()),
        "appearance.light_palette" => Some(!dark),
        _ => None,
    }
}

/// A choice's label as shown: Light palette's "Same as dark" names what that is now.
pub fn choice_label(
    key: &str,
    j: usize,
    name: &'static str,
    a: &Appearance,
) -> std::borrow::Cow<'static, str> {
    if key == "appearance.light_palette" && j == 0 {
        format!("{name} ({})", PALETTES[a.palette_for(true) as usize]).into()
    } else {
        name.into()
    }
}

/// What the OS appearance is, as far as the app knows: dark until a window reports it
/// (screenshots never ask, so System resolves to dark there unless `--appearance` says).
static OS_DARK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The OS appearance the app last heard (see [`set_os_dark`]).
pub fn os_dark() -> bool {
    OS_DARK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record the OS appearance; true if it changed.
pub fn set_os_dark(dark: bool) -> bool {
    OS_DARK.swap(dark, std::sync::atomic::Ordering::Relaxed) != dark
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

/// The theme an appearance asks for, System resolved by the OS appearance the app last
/// heard ([`os_dark`]).
pub fn theme_for(a: &Appearance) -> crate::theme::Theme {
    resolve(a, os_dark())
}

/// The theme an appearance asks for when the OS appearance is dark (`os_dark`) or light:
/// the choice picks the appearance, the appearance its own palette.
pub fn resolve(a: &Appearance, os_dark: bool) -> crate::theme::Theme {
    use crate::theme::{HollowBodies, Palette, Theme};
    let dark = match a.theme_choice() {
        ThemeChoice::Dark => true,
        ThemeChoice::Light => false,
        ThemeChoice::System => os_dark,
    };
    let palette = match a.palette_for(dark) {
        PaletteName::Standard => Palette::Standard,
        PaletteName::Colorblind => Palette::Colorblind,
    };
    let mut t = if dark {
        Theme::dark(palette)
    } else {
        Theme::light(palette)
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
/// name ("Off", no prop rules, first).
pub const PRESET_CHOICES: &[&str] = &["Off", "50K intraday", "50K EOD", "100K funded"];

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
    /// The order size of a product with no size of its own (`Settings::order_sizes`), in every
    /// pane; absent: 1.
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

// ---- largest order (per paper account; applies to the next order) ------------------------

impl Target {
    /// This account's saved largest order, wherever it is saved (name, else id).
    pub fn max_order(&self, p: &Prefs) -> Option<i64> {
        [&self.account_name, &self.account]
            .into_iter()
            .flatten()
            .find_map(|k| p.app.max_order_qty.get(k).copied())
    }
}

fn g_max_order(p: &Prefs, t: &Target, f: &Facts) -> Value {
    // Paper accounts only: a broker account shows none (the broker's limits are its own).
    let broker = f
        .accounts
        .iter()
        .any(|a| Some(&a.id) == t.account.as_ref() && !a.paper);
    Value::Number(t.max_order(p).filter(|_| !broker))
}

fn s_max_order(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Number(n) = v else {
        return Err("expected a number");
    };
    let key = [&t.account_name, &t.account]
        .into_iter()
        .flatten()
        .find(|k| p.app.max_order_qty.contains_key(*k))
        .or(t.account_name.as_ref().or(t.account.as_ref()))
        .cloned()
        .ok_or("choose an account")?;
    match n {
        Some(n) if n < 1 => return Err("at least 1"),
        Some(n) => {
            p.app.max_order_qty.insert(key, n);
        }
        None => {
            p.app.max_order_qty.remove(&key);
        }
    }
    Ok(())
}

// ---- guardrails (per account; tighten now, loosen from the next trade date) --------------

fn guard<'a>(p: &'a Prefs, t: &Target) -> Option<&'a lines_trading::Guardrails> {
    t.guard_key(&p.app.guardrails)
        .and_then(|a| p.app.guardrails.get(&a))
}

/// The news blackout's choices (index 0: none), in tightening order.
pub const NEWS_CHOICES: &[&str] = &["Off", "Warn", "Block", "Flatten"];
const NEWS_ACTIONS: [lines_trading::news::NewsAction; 3] = [
    lines_trading::news::NewsAction::Warn,
    lines_trading::news::NewsAction::Block,
    lines_trading::news::NewsAction::Flatten,
];

fn news(p: &Prefs, t: &Target) -> Option<lines_trading::news::NewsBlackout> {
    guard(p, t).and_then(|g| g.news)
}

fn g_news(p: &Prefs, t: &Target, _: &Facts) -> Value {
    Value::Choice(news(p, t).map_or(0, |n| {
        1 + NEWS_ACTIONS
            .iter()
            .position(|a| *a == n.action)
            .unwrap_or(0)
    }))
}
fn s_news(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Choice(k) = v else {
        return Err("expected a choice");
    };
    let g = guard_mut(p, t)?;
    g.news = match (k, g.news) {
        (0, _) => None,
        (k, Some(n)) => Some(lines_trading::news::NewsBlackout {
            action: NEWS_ACTIONS[k - 1],
            ..n
        }),
        (k, None) => Some(lines_trading::news::NewsBlackout::new(NEWS_ACTIONS[k - 1])),
    };
    drop_empty_guardrails(p, t);
    Ok(())
}
fn news_mut<'a>(
    p: &'a mut Prefs,
    t: &Target,
) -> Result<&'a mut lines_trading::news::NewsBlackout, &'static str> {
    guard_mut(p, t)?
        .news
        .as_mut()
        .ok_or("turn the news blackout on first")
}
fn g_news_before(p: &Prefs, t: &Target, _: &Facts) -> Value {
    Value::Number(news(p, t).map(|n| i64::from(n.before_min)))
}
fn s_news_before(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Number(Some(m)) = v else {
        return Err("expected minutes");
    };
    news_mut(p, t)?.before_min = m as u32;
    Ok(())
}
fn g_news_after(p: &Prefs, t: &Target, _: &Facts) -> Value {
    Value::Number(news(p, t).map(|n| i64::from(n.after_min)))
}
fn s_news_after(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Number(Some(m)) = v else {
        return Err("expected minutes");
    };
    news_mut(p, t)?.after_min = m as u32;
    Ok(())
}
fn g_news_medium(p: &Prefs, t: &Target, _: &Facts) -> Value {
    Value::Bool(news(p, t).is_some_and(|n| n.include_medium))
}
fn s_news_medium(p: &mut Prefs, t: &Target, v: Value) -> Result<(), &'static str> {
    let Value::Bool(b) = v else {
        return Err("expected on or off");
    };
    news_mut(p, t)?.include_medium = b;
    Ok(())
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

/// A new losing streak's cooldown, minutes (also what a reset of the cooldown gives).
const COOLDOWN_MIN: i64 = 15;

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
        cooldown_min: g
            .losing_streak
            .map_or(COOLDOWN_MIN as u32, |s| s.cooldown_min),
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

/// Trade history on new charts (`on`), kept in the defaults for new charts: off adds the
/// switch to them (creating them if there are none); on removes it, and defaults that then
/// hold nothing go (new charts start empty either way).
fn set_new_chart_trades(defaults: &mut Option<crate::study_menu::StudySet>, on: bool) {
    match defaults {
        Some(set) => {
            set.trades_off = !on;
            if *set == crate::study_menu::StudySet::default() {
                *defaults = None;
            }
        }
        None if !on => {
            *defaults = Some(crate::study_menu::StudySet {
                trades_off: true,
                ..Default::default()
            });
        }
        None => {}
    }
}

fn with_default(mut st: Setting, v: Value) -> Setting {
    st.default = Some(v);
    st
}

use Kind::*;
use Safety::{Normal, Trading};
use Section as Sec;

const THEMES: &[&str] = &["Dark", "Light", "System"];
const PALETTES: &[&str] = &["Standard", "Colorblind"];
const LIGHT_PALETTES: &[&str] = &["Same as dark", "Standard", "Colorblind"];
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
        with_default(
            s(
                "general.updates",
                Sec::General,
                "Updates",
                "Automatic: download a newer release in the background and ask to restart (never during a trade). Notify only: say a newer release exists. Off: no request at all.",
                Scope::App,
                Normal,
                Choice(&crate::update::UpdateMode::LABELS),
                |p, _, _| Value::Choice(p.app.updates.index()),
                Some(|p, _, v| {
                    let i = want_choice(v, crate::update::UpdateMode::ALL.len())?;
                    p.app.updates = crate::update::UpdateMode::ALL[i];
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        // Appearance.
        with_default(
            s(
                "appearance.theme",
                Sec::Appearance,
                "Theme",
                "System follows the OS light or dark mode.",
                Scope::App,
                Normal,
                Choice(THEMES),
                |p, _, _| Value::Choice(p.app.appearance.theme_choice() as usize),
                Some(|p, _, v| {
                    let i = want_choice(v, ThemeChoice::ALL.len())?;
                    p.app.appearance.set_theme_choice(ThemeChoice::ALL[i]);
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        with_default(
            s(
                "appearance.palette",
                Sec::Appearance,
                "Dark palette",
                "Up and down colors in the dark theme. Colorblind uses blue and orange; direction also shows by shape.",
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
                "appearance.light_palette",
                Sec::Appearance,
                "Light palette",
                "Up and down colors in the light theme.",
                Scope::App,
                Normal,
                Choice(LIGHT_PALETTES),
                |p, _, _| {
                    Value::Choice(match p.app.appearance.light_palette {
                        None => 0,
                        Some(PaletteName::Standard) => 1,
                        Some(PaletteName::Colorblind) => 2,
                    })
                },
                Some(|p, _, v| {
                    let i = want_choice(v, LIGHT_PALETTES.len())?;
                    p.app.appearance.light_palette = [
                        None,
                        Some(PaletteName::Standard),
                        Some(PaletteName::Colorblind),
                    ][i];
                    Ok(())
                }),
            ),
            Value::Choice(0),
        ),
        with_default(
            s(
                "appearance.hollow",
                Sec::Charts,
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
                "appearance.ui_zoom",
                Sec::Appearance,
                "UI zoom",
                "The size of text, rows, headers, menus and panels in every window (Ctrl+= / Ctrl+− / Ctrl+0). The chart's own time and price zoom stay on the wheel and drag.",
                Scope::App,
                Normal,
                Choice(&crate::zoom::LABELS),
                |p, _, _| {
                    Value::Choice(crate::zoom::index(
                        p.app.ui_zoom.unwrap_or(crate::zoom::DEFAULT),
                    ))
                },
                Some(|p, _, v| {
                    let i = want_choice(v, crate::zoom::STEPS.len())?;
                    p.app.ui_zoom = crate::zoom::saved(crate::zoom::STEPS[i]);
                    Ok(())
                }),
            ),
            Value::Choice(2),
        ),
        with_default(
            s(
                crate::display_zone::KEY,
                Sec::General,
                "Time zone",
                "Times in charts, time & sales, the journal and strategies. Sessions, the VWAP reset and expiries stay on the exchange's clock.",
                Scope::App,
                Normal,
                Editor,
                |p, _, _| {
                    Value::Text(crate::display_zone::describe_saved(
                        p.app.time_zone.as_deref(),
                    ))
                },
                Some(|p, _, v| {
                    let Value::Text(shown) = v else {
                        return Err("not a time zone");
                    };
                    p.app.time_zone = crate::display_zone::saved_of(&shown)?;
                    Ok(())
                }),
            ),
            Value::Text(crate::display_zone::describe_saved(None)),
        ),
        with_default(
            s(
                crate::fonts::KEY,
                Sec::Appearance,
                "UI font",
                "Text in menus, headers, labels and dialogs, in every window. Prices, sizes, times and P&L stay in IBM Plex Mono. A font that is no longer installed shows IBM Plex Sans.",
                Scope::App,
                Normal,
                Editor,
                |p, _, _| Value::Text(crate::fonts::describe_saved(p.app.ui_font.as_deref())),
                Some(|p, _, v| {
                    let Value::Text(shown) = v else {
                        return Err("not a font");
                    };
                    p.app.ui_font = crate::fonts::saved_of(&shown)?;
                    Ok(())
                }),
            ),
            Value::Text(crate::fonts::describe_saved(None)),
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
        with_default(
            s(
                "charts.trades",
                Sec::Charts,
                "Trade history on new charts",
                "When on, a new chart shows your closed trades on its contract (Alt+E on a chart).",
                Scope::NewPanes,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(!p.app.chart_defaults.as_ref().is_some_and(|s| s.trades_off)),
                Some(|p, _, v| {
                    set_new_chart_trades(&mut p.app.chart_defaults, want_bool(v)?);
                    Ok(())
                }),
            ),
            Value::Bool(true),
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
                Value::Text(
                    if p.app
                        .chart_defaults
                        .as_ref()
                        .is_some_and(|s| s.has_studies())
                    {
                        "The saved default set".to_owned()
                    } else {
                        "Off".to_owned()
                    },
                )
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
        with_default(
            s(
                "dom.join_buttons",
                Sec::Dom,
                "Join buttons",
                "Join bid and Join ask in the DOM's trading panel.",
                Scope::App,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(!p.app.hide_join_buttons),
                Some(|p, _, v| {
                    p.app.hide_join_buttons = !want_bool(v)?;
                    Ok(())
                }),
            ),
            Value::Bool(true),
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
                Sec::OrderEntry,
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
                Sec::OrderEntry,
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
                Sec::OrderEntry,
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
                Sec::OrderEntry,
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
                Sec::OrderEntry,
                "Default quantity",
                "The order size of a product you haven't set one for, in every open pane.",
                Scope::App,
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
                Sec::OrderEntry,
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
            Sec::OrderEntry,
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
        with_default(
            s(
                "guard.max_daily_loss",
                Sec::Accounts,
                "Daily loss limit",
                "Your own limit per trade date, in whole dollars (realized and open, fees included). Empty: off.",
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
            Value::Number(None),
        ),
        with_default(
            s(
                "guard.max_position",
                Sec::Accounts,
                "Max position",
                "Contracts per contract month (ES Dec and ES Mar are each capped), worst case: position plus open orders on one side. Empty: off.",
                Scope::Account,
                Trading,
                Number {
                    min: 1,
                    max: 10_000,
                    optional: true,
                    // A cap in each contract (the core's `Guardrails::max_position`, checked
                    // per `ContractKey`), never an account total ("2 per contract").
                    unit: "per contract",
                },
                g_pos,
                Some(s_pos),
            ),
            Value::Number(None),
        ),
        with_default(
            s(
                "guard.max_trades",
                Sec::Accounts,
                "Max trades per day",
                "Round trips per trade date. Empty: off.",
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
            Value::Number(None),
        ),
        with_default(
            s(
                "guard.losing_streak",
                Sec::Accounts,
                "Cooldown after losses",
                "Losing trades in a row that start a cooldown. Empty: off.",
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
            Value::Number(None),
        ),
        s(
            "guard.cooldown",
            Sec::Accounts,
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
        with_default(
            s(
                "guard.no_entries_before_close",
                Sec::Accounts,
                "No entries before the close",
                "No new entries this many minutes before the session closes. Empty: off.",
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
            Value::Number(None),
        ),
        // Risk: the news blackout (roadmap 8b) around scheduled economic releases.
        with_default(
            s(
                "guard.news",
                Sec::Accounts,
                "News blackout",
                "Around high-importance releases that move the account's product (CPI, the jobs report, FOMC; official schedules bundled with Lines): Warn shows it; Block refuses new entries; Flatten also closes positions in affected products when the window opens. Exits are never blocked. While the calendar can't answer (out of date), Block and Flatten refuse entries.",
                Scope::Account,
                Trading,
                Choice(NEWS_CHOICES),
                g_news,
                Some(s_news),
            ),
            Value::Choice(0),
        ),
        s(
            "guard.news_before",
            Sec::Accounts,
            "News blackout: minutes before",
            "The blackout starts this many minutes before the release.",
            Scope::Account,
            Trading,
            Number {
                min: 0,
                max: 120,
                optional: false,
                unit: "min",
            },
            g_news_before,
            Some(s_news_before),
        ),
        s(
            "guard.news_after",
            Sec::Accounts,
            "News blackout: minutes after",
            "The blackout ends this many minutes after the release.",
            Scope::Account,
            Trading,
            Number {
                min: 0,
                max: 120,
                optional: false,
                unit: "min",
            },
            g_news_after,
            Some(s_news_after),
        ),
        s(
            "guard.news_medium",
            Sec::Accounts,
            "News blackout: medium releases too",
            "The blackout also covers PPI, GDP and PCE (charts mark them either way).",
            Scope::Account,
            Trading,
            Toggle,
            g_news_medium,
            Some(s_news_medium),
        ),
        // Risk: lockouts and breaches, as the core reports them.
        s(
            "guard.status",
            Sec::Accounts,
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
            Sec::Accounts,
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
        with_default(
            s(
                "prop.preset",
                Sec::Accounts,
                "Prop preset",
                "The preset's rules for this account; the rows below override single values. The account size is fixed.",
                Scope::Account,
                Trading,
                Choice(PRESET_CHOICES),
                |p, t, f| {
                    let name = match prop_doc(p, t) {
                        Some(d) => d.preset.clone(),
                        None => fact(f, t)
                            .and_then(|a| a.rules_in_force.as_ref().map(|r| r.name.clone())),
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
                    let preset =
                        (i > 0).then(|| lines_trading::rules::presets()[i - 1].name.clone());
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
            Value::Choice(0),
        ),
        // The overrides below are folded behind this row (the window's Override…).
        s(
            PROP_OVERRIDES_KEY,
            Sec::Accounts,
            "Prop rule values",
            "The preset's values in force: daily loss, trailing drawdown, max micros, profit target. Override… changes single values.",
            Scope::Account,
            Trading,
            Editor,
            |p, t, f| Value::Text(prop_values_text(prop_rules_shown(p, t, f).as_ref())),
            None,
        ),
        with_default(
            s(
                "prop.daily_loss",
                Sec::Accounts,
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
            Value::Number(None),
        ),
        with_default(
            s(
                "prop.drawdown",
                Sec::Accounts,
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
            Value::Number(None),
        ),
        with_default(
            s(
                "prop.max_micros",
                Sec::Accounts,
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
            Value::Number(None),
        ),
        with_default(
            s(
                "prop.profit_target",
                Sec::Accounts,
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
                        prop_field("prop.profit_target", prop_rules_shown(p, t, f).as_ref())
                            .flatten(),
                    )
                },
                Some(|p, t, v| set_prop(p, t, "prop.profit_target", v)),
            ),
            Value::Number(None),
        ),
        // Accounts.
        s(
            "accounts.list",
            Sec::Accounts,
            "Account",
            "The account the page shows (Alt+A or its picker steps through them): paper accounts, plus each broker's (read-only while observed), and its rules.",
            Scope::ReadOnly,
            Normal,
            Info,
            |p, t, f| {
                Value::Text(if f.accounts.is_empty() {
                    "Trading is not running".to_owned()
                } else {
                    match fact(f, t) {
                        None => "Choose an account".to_owned(),
                        // The rules as this page sets them (else as the core has them).
                        Some(a) => match prop_rules_shown(p, t, f)
                            .map(|r| r.name)
                            .or_else(|| a.rules.clone())
                        {
                            Some(r) => format!("{} · {r}", a.name),
                            None => format!("{} · no prop rules", a.name),
                        },
                    }
                })
            },
            None,
        ),
        with_default(
            s(
                MAX_ORDER_KEY,
                Sec::Accounts,
                "Largest order",
                "The most contracts one new order (or a resize) may have on this paper account; the core refuses a larger one from every way in: DOM, chart, trade panel, keys, strategies. Exits that only close the position are never held to it. Empty: no limit. Paper accounts only.",
                Scope::Account,
                Trading,
                Number {
                    min: 1,
                    max: 100_000,
                    optional: true,
                    unit: "contracts",
                },
                g_max_order,
                Some(s_max_order),
            ),
            Value::Number(None),
        ),
        s(
            RESET_KEY,
            Sec::Accounts,
            "Reset paper account",
            "Flattens and cancels everything on it, then back to its starting balance: today's P&L and prop progress cleared, guardrails and any lockout kept. The old file is kept as a dated .bak. Paper only.",
            Scope::Account,
            Trading,
            Editor,
            |_, t, f| {
                let a = f
                    .accounts
                    .iter()
                    .find(|a| Some(&a.id) == t.account.as_ref());
                Value::Text(match a {
                    None => "Choose an account".to_owned(),
                    Some(a) if !a.paper => crate::paper_reset::broker_refusal(&a.name),
                    Some(a) => match a.account_size {
                        Some(n) => format!("{}: back to ${}", a.name, grouped(n)),
                        None => a.name.clone(),
                    },
                })
            },
            None,
        ),
        s(
            DATA_FOLDER_KEY,
            Sec::DataFolder,
            "Data folder",
            "Where the store, recordings, paper accounts, journal, logs and backups live. A new folder is used after restart; moving copies and checks everything, then removes the old copy. Not while a position or order is open.",
            Scope::App,
            Normal,
            Editor,
            |_, _, f| Value::Text(f.folder.text()),
            None,
        ),
        with_default(
            s(
                "data.store",
                Sec::DataFolder,
                "Local store",
                "Keeps live feeds' trades and best bid/ask per contract in the data folder, for the bars at startup and replays.",
                Scope::App,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(p.app.store != Some(false)),
                Some(|p, _, v| {
                    p.app.store = (!want_bool(v)?).then_some(false);
                    Ok(())
                }),
            ),
            Value::Bool(true),
        ),
        with_default(
            s(
                "data.budget",
                Sec::DataFolder,
                "Store budget",
                "Beyond it, the oldest trade dates are removed (after lowering it, at the next start).",
                Scope::App,
                Normal,
                Number {
                    min: 1,
                    max: 100_000,
                    optional: false,
                    unit: "GiB",
                },
                |p, _, _| Value::Number(Some(budget_shown(p.app.store_budget_gb))),
                Some(|p, _, v| {
                    let Value::Number(Some(n)) = v else {
                        return Err("a value is needed");
                    };
                    let default = crate::data_dir::DEFAULT_BUDGET_GB as i64;
                    p.app.store_budget_gb = (n != default).then_some(n as f64);
                    Ok(())
                }),
            ),
            Value::Number(Some(crate::data_dir::DEFAULT_BUDGET_GB as i64)),
        ),
        with_default(
            s(
                "data.depth",
                Sec::DataFolder,
                "Depth recorded for",
                "Products whose order book is stored too (large).",
                Scope::App,
                Normal,
                Products,
                |p, _, _| Value::List(p.app.store_depth.clone().unwrap_or_default()),
                Some(|p, _, v| {
                    let Value::List(l) = v else {
                        return Err("expected products");
                    };
                    let mut kept: Vec<String> = Vec::new();
                    for sym in l {
                        if !kept.iter().any(|k| k.eq_ignore_ascii_case(&sym)) {
                            kept.push(sym);
                        }
                    }
                    p.app.store_depth = (!kept.is_empty()).then_some(kept);
                    Ok(())
                }),
            ),
            Value::List(Vec::new()),
        ),
        // Connections: the sources (drawn as cards by `connections_view`, each with its switch;
        // the two switches below stay rows for search and the palette).
        s(
            CONNECTIONS_LIST,
            Sec::Connections,
            "Data sources",
            "Each source connects only when a pane shows one of its symbols. A source's switch applies when Lines restarts; its settings, the next time it connects. Secrets are kept in the system's keychain.",
            Scope::App,
            Normal,
            Info,
            |_, _, _| Value::Text(String::new()),
            None,
        ),
        with_default(
            s(
                "data.coinbase",
                Sec::Connections,
                "Coinbase",
                "Public crypto prices (BTC/USD, ETH/USD). Connects only while a pane shows one; offline, those panes say so. Applies when Lines next starts.",
                Scope::App,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(source_on(p, "coinbase")),
                Some(|p, _, v| set_source(p, "coinbase", want_bool(v)?)),
            ),
            Value::Bool(true),
        ),
        with_default(
            s(
                "data.kraken",
                Sec::Connections,
                "Kraken",
                "Public crypto prices, spot and perpetuals. Connects only while a pane shows one; offline, those panes say so. Applies when Lines next starts.",
                Scope::App,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(source_on(p, "kraken")),
                Some(|p, _, v| set_source(p, "kraken", want_bool(v)?)),
            ),
            Value::Bool(true),
        ),
        with_default(
            s(
                "data.databento",
                Sec::Connections,
                "Databento",
                "CME futures through your own Databento account, billed by Databento. Off until your API key is set up (DATABENTO_API_KEY, or the OS keychain entry lines-databento); without it Lines makes no Databento requests. Applies when Lines next starts.",
                Scope::App,
                Normal,
                Toggle,
                |p, _, _| Value::Bool(source_on(p, crate::DATABENTO)),
                Some(|p, _, v| set_source(p, crate::DATABENTO, want_bool(v)?)),
            ),
            Value::Bool(true),
        ),
        s(
            "data.databento_key",
            Sec::Connections,
            "Databento key",
            "Whether this run found your Databento API key (DATABENTO_API_KEY, or the OS keychain entry lines-databento; a key added later counts from the next start). The key itself is never shown.",
            Scope::ReadOnly,
            Normal,
            Info,
            |p, _, _| {
                // The startup lookup's answer; never a lookup from here.
                Value::Text(
                    match (crate::databento_known(), source_on(p, crate::DATABENTO)) {
                        (Some(true), _) => "Set up",
                        (Some(false), _) => "Not set up",
                        (None, false) => "Off",
                        (None, true) => "Checked when a run uses Databento",
                    }
                    .to_owned(),
                )
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
                Sec::About,
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
            Sec::About,
            "Settings and logs folder",
            "Export diagnostics (palette) bundles the logs with secrets removed.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| Value::Text(f.config_dir.clone()),
            None,
        ),
        s(
            "about.version",
            Sec::About,
            "Version",
            "Updates are signed and checked before they run; this build updates itself only if it carries the project's update key.",
            Scope::ReadOnly,
            Normal,
            Info,
            |_, _, f| {
                Value::Text(if f.can_update {
                    format!("Lines v{}", f.version)
                } else {
                    format!("Lines v{} (this build doesn't update itself)", f.version)
                })
            },
            None,
        ),
        s(
            ROLLBACK_KEY,
            Sec::About,
            "Roll back",
            "Go back to the version the last update replaced, with the settings, workspaces and trading files it had. Waits while anything is trading.",
            Scope::ReadOnly,
            Normal,
            Editor,
            |_, _, f| {
                Value::Text(match &f.rollback {
                    Some(v) => format!("v{v} is kept"),
                    None => "No earlier version is kept".to_owned(),
                })
            },
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

    /// Whether this setting differs from its default (the window's ● on its row and its
    /// section). A prop override differs while it is set (its default is the preset's value).
    pub fn changed(&self, p: &Prefs, t: &Target, f: &Facts) -> bool {
        match self.key {
            // The summary line: nothing of its own to reset; each override has its ●.
            PROP_OVERRIDES_KEY => false,
            k if PROP_OVERRIDES.contains(&k) => prop_override(prop_doc(p, t), k).is_some(),
            _ => self
                .reset_value(p, t)
                .is_some_and(|d| d != (self.get)(p, t, f)),
        }
    }

    /// The value a reset (↺, Ctrl+R) gives now; `None`: nothing to reset to. Mostly the
    /// default; a part of a guardrail that is off has none (the cooldown without a losing
    /// streak, the news window without the blackout), and one that is on resets to the value
    /// it starts with; a prop override resets to the preset's value while it is set.
    pub fn reset_value(&self, p: &Prefs, t: &Target) -> Option<Value> {
        let fresh = lines_trading::news::NewsBlackout::new(NEWS_ACTIONS[0]);
        match self.key {
            "guard.cooldown" => guard(p, t)
                .and_then(|g| g.losing_streak)
                .map(|_| Value::Number(Some(COOLDOWN_MIN))),
            "guard.news_before" => news(p, t).map(|_| Value::Number(Some(fresh.before_min.into()))),
            "guard.news_after" => news(p, t).map(|_| Value::Number(Some(fresh.after_min.into()))),
            "guard.news_medium" => news(p, t).map(|_| Value::Bool(fresh.include_medium)),
            k if PROP_OVERRIDES.contains(&k) => prop_override(prop_doc(p, t), k)
                .is_some()
                .then_some(Value::Number(None)),
            _ => self.default.clone(),
        }
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
            (_, Value::Number(None)) => "Off".to_owned(),
            (_, Value::Text(t)) => t.clone(),
            (_, Value::List(l)) if l.is_empty() => "None".to_owned(),
            (_, Value::List(l)) => l.join(", "),
            _ => String::new(),
        }
    }
}

/// Whether changing setting `st` from `old` to `new` loosens trading risk (decision 0021:
/// it needs an explicit yes): a guardrail made looser or removed (`g0` to `g1`, the
/// account's guardrails before and after), an opt-in "disable trading" turned off, a larger
/// default quantity, a paper account's largest order raised or removed.
pub fn loosening(
    st: &Setting,
    old: &Value,
    new: &Value,
    g0: &lines_trading::Guardrails,
    g1: &lines_trading::Guardrails,
) -> bool {
    if st.safety != Safety::Trading {
        return false;
    }
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
        // A paper account's largest order raised or removed (decision 0021: raising max size).
        MAX_ORDER_KEY => match (old, new) {
            (Value::Number(Some(a)), Value::Number(Some(b))) => b > a,
            (Value::Number(Some(_)), Value::Number(None)) => true,
            _ => false,
        },
        _ => false,
    }
}

/// A part of a guardrail shown while the guardrail is off: `None` hides it (the cooldown
/// without a losing streak: its line reads just "Off"); `Some(n)` shows `n` muted, the value
/// it starts with when turned on (the news window while the blackout is off).
pub fn dormant(st: &Setting, p: &Prefs, t: &Target) -> Option<Option<i64>> {
    let fresh = lines_trading::news::NewsBlackout::new(NEWS_ACTIONS[0]);
    match st.key {
        "guard.cooldown" if guard(p, t).and_then(|g| g.losing_streak).is_none() => Some(None),
        "guard.news_before" if news(p, t).is_none() => Some(Some(fresh.before_min.into())),
        "guard.news_after" if news(p, t).is_none() => Some(Some(fresh.after_min.into())),
        _ => None,
    }
}

/// Whether changing `st` from `before` to `after` (its value `old` to `new`) loosens trading
/// risk for account `t`: the window's rows, a reset, an undo and the palette all ask through
/// this, with the account's real guardrails and prop rules.
pub fn change_loosens(
    st: &Setting,
    (old, new): (&Value, &Value),
    (before, after): (&Prefs, &Prefs),
    t: &Target,
    f: &Facts,
) -> bool {
    loosening(st, old, new, &t.guardrails(before), &t.guardrails(after))
        || (st.key.starts_with("prop.") && prop_loosens(before, after, t, f))
}

/// How many lines a search's matches fill (two matches on one line count once).
pub fn line_count(hits: &[usize]) -> usize {
    let mut firsts: Vec<&str> = hits
        .iter()
        .filter_map(|&i| places()[i].and_then(|p| p.line.keys().next()))
        .collect();
    firsts.dedup();
    firsts.len()
}

/// The value setting `st` shows once `after` is in force, as words: what a confirmation or a
/// note names as the new value (a prop override reset to `Off` is the preset's value).
pub fn shown_after(st: &Setting, after: &Prefs, t: &Target, f: &Facts) -> String {
    st.value_text(&(st.get)(after, t, f))
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
        "guard.news_before" => g.news.map(|n| i64::from(n.before_min)),
        "guard.news_after" => g.news.map(|n| i64::from(n.after_min)),
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

/// The prop overrides, folded behind the prop values row ([`PROP_OVERRIDES_KEY`]).
pub const PROP_OVERRIDES: [&str; 4] = [
    "prop.daily_loss",
    "prop.drawdown",
    "prop.max_micros",
    "prop.profit_target",
];
/// Settings › Accounts › Prop rules: the preset's values, and Override… (the overrides).
pub const PROP_OVERRIDES_KEY: &str = "prop.overrides";

/// An override saved in the account's prop-rule entry (`None`: the preset's value).
fn prop_override(doc: Option<&PropRulesDoc>, key: &str) -> Option<i64> {
    let d = doc?;
    match key {
        "prop.daily_loss" => d.daily_loss_limit,
        "prop.drawdown" => d.trailing_drawdown,
        "prop.max_micros" => d.max_micro_contracts.map(i64::from),
        "prop.profit_target" => d.profit_target,
        _ => None,
    }
}

/// The prop values row: "$1,000 · $2,000 EOD · 50 · $3,000" (daily loss, trailing drawdown
/// and its mode, max micros, target; "Off" where the rules have none), or "No preset".
fn prop_values_text(r: Option<&lines_trading::rules::RulePreset>) -> String {
    let Some(r) = r else {
        return "No preset".to_owned();
    };
    let money = |n: Option<i64>| n.map_or_else(|| "Off".to_owned(), |n| format!("${}", grouped(n)));
    let dd = r.trailing_drawdown.as_ref().map_or_else(
        || "Off".to_owned(),
        |d| {
            let mode = match d.mode {
                lines_trading::rules::DrawdownMode::EndOfDay => " EOD",
                lines_trading::rules::DrawdownMode::Intraday => " intraday",
            };
            format!("${}{mode}", grouped(d.amount))
        },
    );
    format!(
        "{} · {dd} · {} · {}",
        money(r.daily_loss_limit),
        r.max_micro_contracts
            .map_or_else(|| "Off".to_owned(), |n| grouped(n.into())),
        money(r.profit_target)
    )
}

/// The row's line about what the trading core has in force, when it differs from the value
/// shown: "In force $1,000 now · $1,500 from the next session" (a looser value waiting), or
/// "In force 1 now" (set elsewhere, e.g. a script, or not sent yet). Nothing when they agree.
pub fn status(st: &Setting, p: &Prefs, t: &Target, f: &Facts) -> Option<String> {
    if matches!(st.section, Section::DataFolder | Section::Connections) {
        return data_status(st, p, f);
    }
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

/// Whether a public source is on (absent: on; decision 0030).
fn source_on(p: &Prefs, name: &str) -> bool {
    p.app
        .sources
        .get(name)
        .is_none_or(crate::sources::SourcePrefs::on)
}

/// Turn a public source on (the default: no entry) or off.
fn set_source(p: &mut Prefs, name: &str, on: bool) -> Result<(), &'static str> {
    if on {
        p.app.sources.remove(name);
    } else {
        p.app.sources.insert(
            name.to_owned(),
            crate::sources::SourcePrefs {
                enabled: Some(false),
            },
        );
    }
    Ok(())
}

/// Settings › Data folder (its buttons are the window's own).
pub const DATA_FOLDER_KEY: &str = "data.folder";

/// The budget a row shows: whole GiB (a hand-edited fraction rounds; one that can't be used
/// shows the default, as the store uses it).
fn budget_shown(saved: Option<f64>) -> i64 {
    saved
        .filter(|g| g.is_finite() && *g > 0.0)
        .map_or(crate::data_dir::DEFAULT_BUDGET_GB as i64, |g| {
            (g.round() as i64).max(1)
        })
}

/// The products a depth row offers: every catalog symbol, then saved ones this build doesn't
/// know (`false`: shown as unavailable).
pub fn depth_choices(saved: &[String]) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = lines_feed::catalog::all()
        .iter()
        .map(|p| (p.symbol.to_owned(), true))
        .collect();
    for s in saved {
        if !v.iter().any(|(k, _)| k.eq_ignore_ascii_case(s)) {
            v.push((s.clone(), false));
        }
    }
    v
}

/// `list` with `sym` added, or removed if there (case ignored).
pub fn toggled(list: &[String], sym: &str) -> Vec<String> {
    if list.iter().any(|s| s.eq_ignore_ascii_case(sym)) {
        list.iter()
            .filter(|s| !s.eq_ignore_ascii_case(sym))
            .cloned()
            .collect()
    } else {
        let mut v = list.to_vec();
        v.push(sym.to_owned());
        v
    }
}

/// "2.5 GiB", "4 GiB".
fn gib(g: f64) -> String {
    if g.fract() == 0.0 {
        format!("{g:.0} GiB")
    } else {
        format!("{g} GiB")
    }
}

fn list_text(l: &[String]) -> String {
    if l.is_empty() {
        "None".to_owned()
    } else {
        l.join(", ")
    }
}

/// The flag ruling a Data row this run, and the value it gives.
fn data_flag(st: &Setting, f: &Facts) -> Option<(&'static str, Value)> {
    let flags = &f.store_flags;
    match st.key {
        "data.store" if flags.no_store => Some(("--no-store", Value::Bool(false))),
        "data.store" if flags.store_dir => Some(("--store-dir", Value::Bool(true))),
        "data.budget" => flags
            .budget_gb
            .map(|g| ("--store-budget-gb", Value::Number(Some(g.round() as i64)))),
        "data.depth" => flags
            .depth
            .clone()
            .map(|d| ("--store-depth", Value::List(d))),
        _ => None,
    }
}

/// The value a Data row shows while a flag rules it this run (its control is dimmed and can't
/// be changed); `None`: the saved value, editable.
pub fn flag_value(st: &Setting, f: &Facts) -> Option<Value> {
    data_flag(st, f).map(|(_, v)| v)
}

/// A flag-ruled row's value as words: "Off", "2.5 GiB", "ES, NQ".
pub fn flag_text(st: &Setting, f: &Facts) -> Option<String> {
    let (_, v) = data_flag(st, f)?;
    Some(match (st.key, v) {
        ("data.budget", _) => gib(f.store_flags.budget_gb.unwrap_or_default()),
        (_, Value::Bool(b)) => if b { "On" } else { "Off" }.to_owned(),
        (_, Value::List(l)) => list_text(&l),
        (_, v) => st.value_text(&v),
    })
}

/// A Data row's line: the flag in force this run ("Off for this run (--no-store) · saved: On"),
/// else "Applies after restart" when the saved value isn't the one this run started with.
fn data_status(st: &Setting, p: &Prefs, f: &Facts) -> Option<String> {
    use crate::data_dir::StorePrefs;
    if st.key == DATA_FOLDER_KEY {
        let store = f
            .store_flags
            .store_dir_path
            .as_ref()
            .map(|d| format!("Its store is at {} for this run (--store-dir)", d.display()));
        return match (f.folder.status(), store) {
            (Some(a), Some(b)) => Some(format!("{a} · {b}")),
            (a, b) => a.or(b),
        };
    }
    if let Some((flag, _)) = data_flag(st, f) {
        let saved = match (st.get)(p, &Target::default(), f) {
            Value::Bool(b) => if b { "On" } else { "Off" }.to_owned(),
            Value::Number(Some(n)) => gib(n as f64),
            Value::List(l) => list_text(&l),
            v => st.value_text(&v),
        };
        let now = flag_text(st, f).unwrap_or_default();
        return Some(format!("{now} for this run ({flag}) · saved: {saved}"));
    }
    let now = StorePrefs::of(&p.app);
    let start = &f.store_start;
    let differs = match st.key {
        "data.store" => (now.store != Some(false)) != (start.store != Some(false)),
        "data.budget" => budget_shown(now.budget_gb) != budget_shown(start.budget_gb),
        "data.depth" => {
            let norm = |d: &Option<Vec<String>>| {
                let mut v: Vec<String> =
                    d.iter().flatten().map(|s| s.to_ascii_uppercase()).collect();
                v.sort();
                v
            };
            norm(&now.depth) != norm(&start.depth)
        }
        _ => false,
    };
    differs.then(|| "Applies after restart".to_owned())
}

/// Settings › Accounts › Reset paper account (its buttons are the window's own).
pub const RESET_KEY: &str = "accounts.reset";
/// Settings › Accounts › Largest order.
pub const MAX_ORDER_KEY: &str = "accounts.max_order";
/// Settings › About › Roll back (a button, decision 0031).
pub const ROLLBACK_KEY: &str = "about.rollback";

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

// ---- the pages ---------------------------------------------------------------------------

/// One piece of a page's line: words between controls, or a setting's control (`true`: with
/// its unit, "15 min"; `false`: the number alone, the words around it say what it counts).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Word(&'static str),
    Field(&'static str, bool),
}

/// One line of a page: a label, then its parts. Most lines are one setting (its label is the
/// setting's name unless given); a few combine two ("Cooldown after 3 losses, for 15 min").
#[derive(Copy, Clone, Debug)]
pub struct Line {
    /// Empty: the first setting's name.
    pub label: &'static str,
    pub parts: &'static [Part],
    /// Indented under the line above (it qualifies it: the news window under the blackout).
    pub sub: bool,
}

/// A titled group of lines on a page ("LIMITS"); an empty title shows none.
#[derive(Copy, Clone, Debug)]
pub struct Group {
    pub title: &'static str,
    /// The group's settings are per account and product: its title carries both pickers.
    pub product: bool,
    pub lines: &'static [Line],
}

const fn one(key: &'static str) -> Part {
    Part::Field(key, true)
}

macro_rules! line {
    ($key:expr) => {
        Line {
            label: "",
            parts: &[one($key)],
            sub: false,
        }
    };
    ($label:expr, $key:expr) => {
        Line {
            label: $label,
            parts: &[one($key)],
            sub: false,
        }
    };
    (sub $label:expr, $key:expr) => {
        Line {
            label: $label,
            parts: &[one($key)],
            sub: true,
        }
    };
}

const fn group(title: &'static str, lines: &'static [Line]) -> Group {
    Group {
        title,
        product: false,
        lines,
    }
}

const GENERAL: &[Group] = &[group(
    "",
    &[line!("general.updates"), line!(crate::display_zone::KEY)],
)];
const APPEARANCE: &[Group] = &[group(
    "",
    &[
        line!("appearance.theme"),
        line!("appearance.palette"),
        line!("appearance.light_palette"),
        line!("appearance.ui_zoom"),
        line!(crate::fonts::KEY),
    ],
)];
const KEYS: &[Group] = &[group("", &[line!("keys.list")])];
const CHARTS: &[Group] = &[group(
    "",
    &[
        line!("charts.timeframe"),
        line!("charts.trades"),
        line!("charts.studies"),
        line!("appearance.hollow"),
    ],
)];
const DOM: &[Group] = &[group(
    "",
    &[
        line!("dom.grouping"),
        line!("dom.trail"),
        line!("dom.join_buttons"),
        line!("dom.columns"),
    ],
)];
const ORDER_ENTRY: &[Group] = &[
    group(
        "DEFAULTS",
        &[line!("trading.default_qty"), line!("trading.pnl")],
    ),
    Group {
        title: "BRACKETS",
        product: true,
        lines: &[
            line!("Presets", "trading.brackets"),
            line!("Units", "trading.bracket_units"),
        ],
    },
    group(
        "TURN TRADING OFF WHEN",
        &[
            line!("A DOM loses focus", "trading.disable_on_focus_loss"),
            line!("The account changes", "trading.disable_on_account_change"),
            line!("The contract changes", "trading.disable_on_contract_change"),
        ],
    ),
];
const ACCOUNTS: &[Group] = &[
    group(
        "LIMITS",
        &[
            line!("Lockout", "guard.status"),
            line!("guard.max_daily_loss"),
            line!("guard.max_position"),
            line!(MAX_ORDER_KEY),
            line!("guard.max_trades"),
            Line {
                label: "Cooldown",
                parts: &[
                    Part::Word("after"),
                    Part::Field("guard.losing_streak", false),
                    Part::Word("losses, for"),
                    one("guard.cooldown"),
                ],
                sub: false,
            },
            line!("guard.no_entries_before_close"),
        ],
    ),
    group(
        "NEWS BLACKOUT",
        &[
            line!("During high-impact releases", "guard.news"),
            Line {
                label: "Window",
                parts: &[
                    one("guard.news_before"),
                    Part::Word("before,"),
                    one("guard.news_after"),
                    Part::Word("after"),
                ],
                sub: true,
            },
            line!(sub "Medium releases too", "guard.news_medium"),
        ],
    ),
    group(
        "PROP RULES",
        &[
            line!("Breach", "prop.status"),
            line!("Preset", "prop.preset"),
            line!(sub "Daily loss · Drawdown · Max micros · Target", PROP_OVERRIDES_KEY),
            line!(sub "", "prop.daily_loss"),
            line!(sub "", "prop.drawdown"),
            line!(sub "", "prop.max_micros"),
            line!(sub "", "prop.profit_target"),
        ],
    ),
    group("ACCOUNT", &[line!("accounts.list"), line!(RESET_KEY)]),
];
/// Decision 0030: the sources' cards (`connections_view`, drawn under the list's line). The
/// switches are on their cards; their lines here only place them for search and the palette.
const CONNECTIONS: &[Group] = &[group(
    "",
    &[
        line!(CONNECTIONS_LIST),
        line!("data.coinbase"),
        line!("data.kraken"),
        line!("data.databento"),
        line!("data.databento_key"),
    ],
)];
const DATA_FOLDER: &[Group] = &[group(
    "",
    &[
        line!(DATA_FOLDER_KEY),
        line!("data.store"),
        line!("data.budget"),
        line!("data.depth"),
    ],
)];
const ABOUT: &[Group] = &[
    group("", &[line!("about.version"), line!(ROLLBACK_KEY)]),
    group(
        "DIAGNOSTICS",
        &[
            line!("diagnostics.latency_hud"),
            line!("diagnostics.folder"),
        ],
    ),
];

/// A section's page: its groups and lines, in order. Every setting is on exactly one line of
/// its own section's page (tested).
pub fn page(s: Section) -> &'static [Group] {
    match s {
        Section::General => GENERAL,
        Section::Appearance => APPEARANCE,
        Section::Keys => KEYS,
        Section::Charts => CHARTS,
        Section::Dom => DOM,
        Section::OrderEntry => ORDER_ENTRY,
        Section::Accounts => ACCOUNTS,
        Section::Connections => CONNECTIONS,
        Section::DataFolder => DATA_FOLDER,
        Section::About => ABOUT,
    }
}

impl Line {
    /// The settings on this line, in order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.parts.iter().filter_map(|p| match p {
            Part::Field(k, _) => Some(*k),
            Part::Word(_) => None,
        })
    }
}

/// Where a setting sits: its section, group title and line.
#[derive(Copy, Clone, Debug)]
pub struct Place {
    pub section: Section,
    pub group: &'static str,
    pub line: Line,
    /// Its position over every page, in window order.
    pub order: usize,
}

/// Every setting's place, by table index (`None`: on no page; tested never to happen).
pub fn places() -> &'static [Option<Place>] {
    static PLACES: std::sync::OnceLock<Vec<Option<Place>>> = std::sync::OnceLock::new();
    PLACES.get_or_init(|| {
        let mut v = vec![None; all().len()];
        let mut order = 0;
        for section in Section::ALL {
            for g in page(section) {
                for line in g.lines {
                    for k in line.keys() {
                        if let Some(i) = index_of(k) {
                            v[i] = Some(Place {
                                section,
                                group: g.title,
                                line: *line,
                                order,
                            });
                            order += 1;
                        }
                    }
                }
            }
        }
        v
    })
}

/// The label a line shows: its own, else its first setting's name.
pub fn line_label(line: &Line) -> &'static str {
    if line.label.is_empty() {
        line.keys().next().and_then(by_key).map_or("", |st| st.name)
    } else {
        line.label
    }
}

/// Settings matching a search, in window order (section, then place on its page): every word
/// must appear in the name, the label on its page, the description, the section or a choice
/// (case ignored). Empty: off.
pub fn search(query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<usize> = all()
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            let label = places()[*i].as_ref().map_or("", |p| p.line.label);
            let hay = format!(
                "{} {} {} {} {} {}",
                s.name,
                label,
                s.description,
                s.section.name(),
                old_name(s.section),
                s.choices().join(" ")
            )
            .to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .map(|(i, _)| i)
        .collect();
    hits.sort_by_key(|&i| places()[i].map_or(usize::MAX, |p| p.order));
    hits
}

/// The name a section's rows had before the regrouping (search still finds them by it).
fn old_name(s: Section) -> &'static str {
    match s {
        Section::Accounts => "risk",
        Section::OrderEntry => "trading",
        Section::About => "diagnostics",
        Section::Connections | Section::DataFolder => "data",
        _ => "",
    }
}

/// A search result's header: its section, and its group when it has one ("Accounts ›
/// Limits"). Every result sits under the header of its place (`settings_view`).
pub fn search_header(i: usize) -> String {
    match places().get(i).copied().flatten() {
        Some(p) if !p.group.is_empty() => {
            let g = p.group.to_lowercase();
            let mut c = g.chars();
            let first = c.next().map(|x| x.to_ascii_uppercase());
            format!(
                "{} › {}{}",
                p.section.name(),
                first.map(String::from).unwrap_or_default(),
                c.as_str()
            )
        }
        Some(p) => p.section.name().to_owned(),
        None => String::new(),
    }
}

/// The marks on a row: ● (it differs from its default) and whether ↺ resets it (a reset
/// value exists and differs). A row a flag rules this run (a Data folder flag, a look flag)
/// has neither: it isn't changed here, and a reset would save over nothing.
pub fn marks(st: &Setting, p: &Prefs, t: &Target, f: &Facts) -> (bool, bool) {
    let look_flag = LookField::of(st.key).is_some_and(|l| l.set_in(&f.look_flags));
    if look_flag || flag_value(st, f).is_some() || !st.changed(p, t, f) {
        return (false, false);
    }
    let reset = st.reset_value(p, t).is_some_and(|d| d != (st.get)(p, t, f));
    (true, reset)
}

/// Whether any setting in `s` differs from its default (the ● on its sidebar entry), for the
/// account and product chosen.
pub fn section_changed(s: Section, p: &Prefs, t: &Target, f: &Facts) -> bool {
    all()
        .iter()
        .filter(|st| st.section == s)
        .any(|st| marks(st, p, t, f).0)
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
const HAS_COMMAND: [&str; 2] = ["trading.pnl", "diagnostics.latency_hud"];

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

    /// Settings › Charts, trade history on new charts: on by default; off is kept in the
    /// defaults for new charts (created if none), on takes it out again, leaving studies as
    /// they were and no empty defaults behind.
    #[test]
    fn trade_history_on_new_charts() {
        let st = by_key("charts.trades").unwrap();
        assert_eq!(st.section, Section::Charts);
        assert_eq!(st.default, Some(Value::Bool(true)));
        let mut d = None;
        set_new_chart_trades(&mut d, true);
        assert_eq!(d, None, "on with no defaults: nothing saved");
        set_new_chart_trades(&mut d, false);
        assert!(d.as_ref().is_some_and(|s| s.trades_off && !s.has_studies()));
        set_new_chart_trades(&mut d, true);
        assert_eq!(d, None, "back on: no empty defaults left");
        // With studies saved, the switch leaves them.
        let studies = crate::study_menu::StudySet {
            profile: crate::profile::ProfileConfig {
                mode: crate::profile::ProfileMode::Session,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut d = Some(studies.clone());
        set_new_chart_trades(&mut d, false);
        set_new_chart_trades(&mut d, true);
        assert_eq!(d, Some(studies));
    }

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
    }

    /// Every setting is on exactly one line of exactly one page, its own section's: none is
    /// dropped by the regrouping, none shows twice, and every key a page names exists.
    #[test]
    fn every_setting_is_on_one_page_line() {
        let mut seen: Vec<&str> = Vec::new();
        for sec in Section::ALL {
            for g in page(sec) {
                for line in g.lines {
                    assert!(line.keys().count() >= 1);
                    for k in line.keys() {
                        let st = by_key(k).unwrap_or_else(|| panic!("{k} is on a page"));
                        assert_eq!(st.section, sec, "{k}");
                        assert!(!seen.contains(&k), "{k} shows twice");
                        seen.push(k);
                    }
                }
            }
        }
        assert_eq!(seen.len(), all().len(), "every setting has a line");
        assert!(places().iter().all(Option::is_some));
    }

    /// Where every key of the window before the regrouping lands (keys never change; only
    /// their section does).
    #[test]
    fn every_old_key_lands_in_its_new_section() {
        use Section as S;
        let want: &[(&str, Section)] = &[
            ("general.updates", S::General),
            ("appearance.time_zone", S::General),
            ("appearance.theme", S::Appearance),
            ("appearance.palette", S::Appearance),
            ("appearance.ui_zoom", S::Appearance),
            ("keys.list", S::Keys),
            ("charts.timeframe", S::Charts),
            ("charts.studies", S::Charts),
            ("appearance.hollow", S::Charts),
            ("dom.grouping", S::Dom),
            ("dom.trail", S::Dom),
            ("dom.join_buttons", S::Dom),
            ("dom.columns", S::Dom),
            ("trading.default_qty", S::OrderEntry),
            ("trading.pnl", S::OrderEntry),
            ("trading.brackets", S::OrderEntry),
            ("trading.bracket_units", S::OrderEntry),
            ("trading.disable_on_focus_loss", S::OrderEntry),
            ("trading.disable_on_account_change", S::OrderEntry),
            ("trading.disable_on_contract_change", S::OrderEntry),
            ("guard.status", S::Accounts),
            ("guard.max_daily_loss", S::Accounts),
            ("guard.max_position", S::Accounts),
            ("accounts.max_order", S::Accounts),
            ("guard.max_trades", S::Accounts),
            ("guard.losing_streak", S::Accounts),
            ("guard.cooldown", S::Accounts),
            ("guard.no_entries_before_close", S::Accounts),
            ("guard.news", S::Accounts),
            ("guard.news_before", S::Accounts),
            ("guard.news_after", S::Accounts),
            ("guard.news_medium", S::Accounts),
            ("prop.status", S::Accounts),
            ("prop.preset", S::Accounts),
            ("prop.daily_loss", S::Accounts),
            ("prop.drawdown", S::Accounts),
            ("prop.max_micros", S::Accounts),
            ("prop.profit_target", S::Accounts),
            ("accounts.list", S::Accounts),
            ("accounts.reset", S::Accounts),
            ("connections.list", S::Connections),
            ("data.coinbase", S::Connections),
            ("data.kraken", S::Connections),
            ("data.databento", S::Connections),
            ("data.databento_key", S::Connections),
            ("data.folder", S::DataFolder),
            ("data.store", S::DataFolder),
            ("data.budget", S::DataFolder),
            ("data.depth", S::DataFolder),
            ("diagnostics.latency_hud", S::About),
            ("diagnostics.folder", S::About),
            ("about.version", S::About),
            ("about.rollback", S::About),
        ];
        for (k, sec) in want {
            assert_eq!(by_key(k).map(|s| s.section), Some(*sec), "{k}");
        }
        // Keys added since, in window order: the UI font (decision 0036, on Appearance), trade
        // history on new charts (Charts), and the only key the regrouping adds, the prop values
        // line the overrides fold behind (a button, nothing saved).
        let new: Vec<&str> = all()
            .iter()
            .map(|s| s.key)
            .filter(|k| !want.iter().any(|(w, _)| w == k))
            .collect();
        assert_eq!(
            new,
            ["appearance.ui_font", "charts.trades", PROP_OVERRIDES_KEY]
        );
        assert_eq!(by_key("charts.trades").map(|s| s.section), Some(S::Charts));
        assert_eq!(
            by_key("appearance.ui_font").map(|s| s.section),
            Some(S::Appearance)
        );
        assert!(by_key(PROP_OVERRIDES_KEY).unwrap().read_only());
    }

    /// `--settings` takes every section by its name or slug, and the names the window had
    /// before (risk, trading, diagnostics, data).
    #[test]
    fn sections_parse_by_name_slug_and_old_name() {
        for s in Section::ALL {
            assert_eq!(Section::parse(s.name()), Some(s));
            assert_eq!(Section::parse(&s.slug()), Some(s));
            assert_eq!(Section::parse(&s.slug().to_uppercase()), Some(s));
        }
        assert_eq!(Section::parse("order-entry"), Some(Section::OrderEntry));
        assert_eq!(Section::parse("order entry"), Some(Section::OrderEntry));
        assert_eq!(Section::parse("data-folder"), Some(Section::DataFolder));
        assert_eq!(Section::parse("risk"), Some(Section::Accounts));
        assert_eq!(Section::parse("trading"), Some(Section::OrderEntry));
        assert_eq!(Section::parse("diagnostics"), Some(Section::About));
        assert_eq!(Section::parse("data"), Some(Section::DataFolder));
        assert_eq!(Section::parse("accounts"), Some(Section::Accounts));
        assert_eq!(Section::parse("nope"), None);
        // The sidebar's groups, in order, each run unbroken.
        let groups: Vec<&str> = Section::ALL.iter().map(|s| s.group()).collect();
        assert_eq!(
            groups,
            [
                "APP", "APP", "APP", "PANES", "PANES", "TRADING", "TRADING", "DATA", "DATA", ""
            ]
        );
    }

    /// The Accounts page: one picker (the page's), groups Limits, News blackout, Prop rules,
    /// Account; nothing on it is per product (the product picker shows only on Order entry's
    /// Brackets group).
    #[test]
    fn the_accounts_page_has_four_groups_and_no_product_picker() {
        let titles: Vec<&str> = page(Section::Accounts).iter().map(|g| g.title).collect();
        assert_eq!(titles, ["LIMITS", "NEWS BLACKOUT", "PROP RULES", "ACCOUNT"]);
        assert!(page(Section::Accounts).iter().all(|g| !g.product));
        for g in page(Section::Accounts) {
            for k in g.lines.iter().flat_map(|l| l.keys()) {
                assert_ne!(by_key(k).unwrap().scope, Scope::AccountProduct, "{k}");
            }
        }
        // Per-product rows have their pickers where they are, and only there.
        for sec in Section::ALL {
            for g in page(sec) {
                let per_product = g
                    .lines
                    .iter()
                    .flat_map(|l| l.keys())
                    .any(|k| by_key(k).unwrap().scope == Scope::AccountProduct);
                assert_eq!(g.product, per_product, "{sec:?} {}", g.title);
            }
        }
    }

    /// The cooldown and the news window are one line of two settings each: the line reads and
    /// writes the same keys as before (the losing streak and its cooldown; minutes before and
    /// after).
    #[test]
    fn combined_lines_read_and_write_the_same_keys() {
        let line_of = |k: &str| places()[index_of(k).unwrap()].unwrap().line;
        let cool = line_of("guard.losing_streak");
        assert_eq!(
            cool.keys().collect::<Vec<_>>(),
            ["guard.losing_streak", "guard.cooldown"]
        );
        assert_eq!(line_label(&cool), "Cooldown");
        let window = line_of("guard.news_before");
        assert_eq!(
            window.keys().collect::<Vec<_>>(),
            ["guard.news_before", "guard.news_after"]
        );
        assert!(window.sub);
        let t = acct();
        let f = Facts::default();
        let mut p = Prefs::default();
        let set = |p: &mut Prefs, k: &str, v: Value| by_key(k).unwrap().apply(p, &t, v);
        set(&mut p, "guard.losing_streak", Value::Number(Some(3))).unwrap();
        set(&mut p, "guard.cooldown", Value::Number(Some(20))).unwrap();
        let g = &p.app.guardrails["paper-eval"];
        assert_eq!(
            g.losing_streak,
            Some(lines_trading::LosingStreak {
                losses: 3,
                cooldown_min: 20
            })
        );
        set(&mut p, "guard.news", Value::Choice(2)).unwrap();
        set(&mut p, "guard.news_before", Value::Number(Some(5))).unwrap();
        set(&mut p, "guard.news_after", Value::Number(Some(10))).unwrap();
        let n = p.app.guardrails["paper-eval"].news.unwrap();
        assert_eq!((n.before_min, n.after_min), (5, 10));
        for (k, v) in [
            ("guard.losing_streak", 3),
            ("guard.cooldown", 20),
            ("guard.news_before", 5),
            ("guard.news_after", 10),
        ] {
            assert_eq!(
                (by_key(k).unwrap().get)(&p, &t, &f),
                Value::Number(Some(v)),
                "{k}"
            );
        }
    }

    /// ● and ↺ show exactly when a value differs from its default: a guardrail set, a part
    /// of one changed from the value it starts with, a prop override set; and never on a row
    /// a flag rules this run.
    #[test]
    fn the_changed_marks_follow_the_default() {
        let t = acct();
        let f = Facts::default();
        let mut p = Prefs::default();
        let marks_of = |p: &Prefs, k: &str| marks(by_key(k).unwrap(), p, &t, &f);
        for st in all() {
            assert_eq!(marks(st, &p, &t, &f), (false, false), "{}", st.key);
        }
        for s in Section::ALL {
            assert!(!section_changed(s, &p, &t, &f), "{s:?}");
        }
        by_key("guard.max_daily_loss")
            .unwrap()
            .apply(&mut p, &t, Value::Number(Some(1_000)))
            .unwrap();
        assert_eq!(marks_of(&p, "guard.max_daily_loss"), (true, true));
        assert!(section_changed(Section::Accounts, &p, &t, &f));
        assert!(!section_changed(Section::OrderEntry, &p, &t, &f));
        // Another account's rows are unmarked.
        let other = Target {
            account: Some("other".into()),
            ..acct()
        };
        assert!(!section_changed(Section::Accounts, &p, &other, &f));
        // A losing streak: its cooldown starts at 15 and is unmarked until changed.
        let streak = by_key("guard.losing_streak").unwrap();
        streak.apply(&mut p, &t, Value::Number(Some(3))).unwrap();
        assert_eq!(marks_of(&p, "guard.cooldown"), (false, false));
        let cool = by_key("guard.cooldown").unwrap();
        cool.apply(&mut p, &t, Value::Number(Some(30))).unwrap();
        assert_eq!(marks_of(&p, "guard.cooldown"), (true, true));
        assert_eq!(
            cool.reset_value(&p, &t),
            Some(Value::Number(Some(COOLDOWN_MIN)))
        );
        // No reset for a part whose guardrail is off.
        assert_eq!(
            by_key("guard.news_before").unwrap().reset_value(&p, &t),
            None
        );
        // A look change: marked, and the reset is its default.
        let theme = by_key("appearance.theme").unwrap();
        theme.apply(&mut p, &t, Value::Choice(1)).unwrap();
        assert_eq!(marks_of(&p, "appearance.theme"), (true, true));
        assert!(section_changed(Section::Appearance, &p, &t, &f));
        // A prop override: marked while set (its default is the preset's value); the
        // prop values line has no mark of its own.
        by_key("prop.preset")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(1))
            .unwrap();
        assert_eq!(marks_of(&p, "prop.daily_loss"), (false, false));
        by_key("prop.daily_loss")
            .unwrap()
            .apply(&mut p, &t, Value::Number(Some(700)))
            .unwrap();
        assert_eq!(marks_of(&p, "prop.daily_loss"), (true, true));
        assert_eq!(marks_of(&p, PROP_OVERRIDES_KEY), (false, false));
        by_key("prop.daily_loss")
            .unwrap()
            .apply(&mut p, &t, Value::Number(None))
            .unwrap();
        assert_eq!(marks_of(&p, "prop.daily_loss"), (false, false));
        // A flag rules a Data row this run: no marks, whatever is saved.
        let mut q = Prefs::default();
        by_key("data.store")
            .unwrap()
            .apply(&mut q, &t, Value::Bool(false))
            .unwrap();
        assert_eq!(marks_of(&q, "data.store"), (true, true));
        let mut flagged = Facts::default();
        flagged.store_flags.no_store = true;
        assert_eq!(
            marks(by_key("data.store").unwrap(), &q, &t, &flagged),
            (false, false)
        );
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
        assert!(labels.contains("Charts: Hollow candles Down candles"));
        assert!(labels.contains("Connections: Coinbase off"));
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
        assert_eq!(qty.scope, Scope::App, "it changes open panes' sizes");
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

    /// Theme: System and the per-appearance palettes resolve to a theme as a pure function of
    /// (choice, OS appearance, palettes).
    #[test]
    fn system_theme_and_palettes_resolve() {
        use crate::theme::{Palette, Theme};
        let system = |light_palette, palette| Appearance {
            follow_os: Some(true),
            palette,
            light_palette,
            ..Default::default()
        };
        // System follows the OS; Dark and Light ignore it.
        let a = system(None, None);
        assert_eq!(resolve(&a, true), Theme::dark(Palette::Standard));
        assert_eq!(resolve(&a, false), Theme::light(Palette::Standard));
        let light = Appearance {
            theme: Some(ThemeName::Light),
            ..Default::default()
        };
        for os in [true, false] {
            assert_eq!(
                resolve(&Appearance::default(), os),
                Theme::dark(Palette::Standard)
            );
            assert_eq!(resolve(&light, os), Theme::light(Palette::Standard));
        }
        // One saved palette (as before there were two) is both appearances'.
        let a = system(None, Some(PaletteName::Colorblind));
        assert_eq!(resolve(&a, true), Theme::dark(Palette::Colorblind));
        assert_eq!(resolve(&a, false), Theme::light(Palette::Colorblind));
        // Dark colorblind, light standard.
        let a = system(Some(PaletteName::Standard), Some(PaletteName::Colorblind));
        assert_eq!(resolve(&a, true), Theme::dark(Palette::Colorblind));
        assert_eq!(resolve(&a, false), Theme::light(Palette::Standard));
        // Light colorblind, dark standard; a saved explicit theme under System is kept.
        let a = Appearance {
            theme: Some(ThemeName::Light),
            ..system(Some(PaletteName::Colorblind), None)
        };
        assert_eq!(a.theme_choice(), ThemeChoice::System);
        assert_eq!(resolve(&a, true), Theme::dark(Palette::Standard));
        assert_eq!(resolve(&a, false), Theme::light(Palette::Colorblind));
        // A theme flag holds over a saved System, and `--theme system` over a saved Light.
        let dark_flag = Appearance {
            theme: Some(ThemeName::Dark),
            follow_os: Some(false),
            ..Default::default()
        };
        assert_eq!(a.with_flags(dark_flag).theme_choice(), ThemeChoice::Dark);
        let system_flag = Appearance {
            follow_os: Some(true),
            ..Default::default()
        };
        assert_eq!(
            light.with_flags(system_flag).theme_choice(),
            ThemeChoice::System
        );
    }

    /// The Theme setting: System is its third choice, choosing it keeps the explicit theme
    /// saved (an older build reads that), and choosing Dark or Light again stops following.
    #[test]
    fn theme_setting_stores_system_apart() {
        let st = by_key("appearance.theme").unwrap();
        assert!(matches!(st.kind, Kind::Choice(c) if c == ["Dark", "Light", "System"]));
        let (t, f) = (Target::default(), Facts::default());
        let mut p = Prefs::default();
        st.apply(&mut p, &t, Value::Choice(1)).unwrap();
        st.apply(&mut p, &t, Value::Choice(2)).unwrap();
        assert_eq!((st.get)(&p, &t, &f), Value::Choice(2));
        assert_eq!(p.app.appearance.theme, Some(ThemeName::Light));
        assert_eq!(p.app.appearance.follow_os, Some(true));
        let json = String::from_utf8(p.app.encode()).unwrap();
        assert!(json.contains("\"theme\": \"light\""), "{json}");
        assert!(json.contains("\"follow_os\": true"), "{json}");
        st.apply(&mut p, &t, Value::Choice(0)).unwrap();
        assert_eq!(
            p.app.appearance,
            Appearance::default(),
            "Dark: nothing saved"
        );
        assert!(st.apply(&mut p, &t, Value::Choice(3)).is_err());

        // A change to System through the window's path saves System, never a flag's theme.
        let saved = Appearance::default();
        let flags = Appearance {
            theme: Some(ThemeName::Light),
            follow_os: Some(false),
            ..Default::default()
        };
        let shown = saved.with_flags(flags);
        let mut after = shown;
        after.set_theme_choice(ThemeChoice::System);
        let (s, f) = Appearance::changed(saved, flags, shown, after);
        assert_eq!((s.theme, s.follow_os), (None, Some(true)));
        assert_eq!((f.theme, f.follow_os), (None, None), "the flag drops");
        // And an undo puts both back.
        let (s2, f2) = Appearance::undo_field(LookField::Theme, (s, f), (saved, flags));
        assert_eq!((s2, f2), (saved, flags));
    }

    /// Light palette: "Same as dark" (absent) until set; set apart, it is saved as given
    /// (Standard included), and the dark palette no longer changes it.
    #[test]
    fn light_palette_is_apart_once_set() {
        let dark = by_key("appearance.palette").unwrap();
        let light = by_key("appearance.light_palette").unwrap();
        assert!(
            matches!(light.kind, Kind::Choice(c) if c == ["Same as dark", "Standard", "Colorblind"])
        );
        let (t, f) = (Target::default(), Facts::default());
        let mut p = Prefs::default();
        dark.apply(&mut p, &t, Value::Choice(1)).unwrap();
        assert_eq!(p.app.appearance.palette_for(false), PaletteName::Colorblind);
        light.apply(&mut p, &t, Value::Choice(1)).unwrap();
        assert_eq!((light.get)(&p, &t, &f), Value::Choice(1));
        assert_eq!(p.app.appearance.palette_for(true), PaletteName::Colorblind);
        assert_eq!(p.app.appearance.palette_for(false), PaletteName::Standard);
        let json = String::from_utf8(p.app.encode()).unwrap();
        assert!(json.contains("\"light_palette\": \"standard\""), "{json}");
        dark.apply(&mut p, &t, Value::Choice(0)).unwrap();
        assert_eq!(p.app.appearance.palette_for(false), PaletteName::Standard);
        light.apply(&mut p, &t, Value::Choice(0)).unwrap();
        assert_eq!(p.app.appearance, Appearance::default());
    }

    /// A settings file from before System and the light palette loads unchanged (the new
    /// fields are absent), and saves without them.
    #[test]
    fn old_appearance_loads_unchanged() {
        let old = r#"{"version": 1, "appearance": {"theme": "light", "palette": "colorblind"}}"#;
        let dir = std::env::temp_dir().join(format!("lines-settings-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, old).unwrap();
        let (s, report, writable) = Settings::load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            (report, writable),
            (None, true),
            "read as is, safe to save over"
        );
        assert_eq!(s.appearance.theme, Some(ThemeName::Light));
        assert_eq!(s.appearance.palette, Some(PaletteName::Colorblind));
        assert_eq!(
            (s.appearance.follow_os, s.appearance.light_palette),
            (None, None)
        );
        assert_eq!(s.appearance.theme_choice(), ThemeChoice::Light);
        assert_eq!(
            resolve(&s.appearance, true),
            crate::theme::Theme::light(crate::theme::Palette::Colorblind)
        );
        let json = String::from_utf8(s.encode()).unwrap();
        assert!(
            !json.contains("follow_os") && !json.contains("light_palette"),
            "{json}"
        );
    }

    /// An OS report: a screenshot never takes one, the same appearance changes nothing, and a
    /// change is applied only under System (remembered otherwise). The startup read is the
    /// same decision.
    #[test]
    fn os_reports_apply_only_under_system_and_never_in_screenshots() {
        use OsReport::*;
        for choice in ThemeChoice::ALL {
            for (known, now) in [(true, false), (false, true), (true, true), (false, false)] {
                assert_eq!(os_report(true, known, now, choice), Ignore, "screenshot");
                let want = if known == now {
                    Ignore
                } else if choice == ThemeChoice::System {
                    Apply
                } else {
                    Remember
                };
                assert_eq!(os_report(false, known, now, choice), want, "{choice:?}");
            }
        }
    }

    /// System says what is in effect; the palette rows say which one is in use; Same as dark
    /// names its palette.
    #[test]
    fn system_now_and_palette_in_use() {
        let mut a = Appearance {
            palette: Some(PaletteName::Colorblind),
            ..Default::default()
        };
        assert_eq!(system_now(&a, true), None, "nothing for an explicit theme");
        a.set_theme_choice(ThemeChoice::System);
        assert_eq!(system_now(&a, true).unwrap(), "Now: Dark · Colorblind");
        assert_eq!(system_now(&a, false).unwrap(), "Now: Light · Colorblind");
        let use_ = |a: &Appearance, os| {
            (
                palette_in_use("appearance.palette", a, os),
                palette_in_use("appearance.light_palette", a, os),
            )
        };
        // Light follows dark: dark's row is in use either way.
        assert_eq!(use_(&a, true), (Some(true), Some(false)));
        assert_eq!(use_(&a, false), (Some(true), Some(true)));
        a.light_palette = Some(PaletteName::Standard);
        assert_eq!(system_now(&a, false).unwrap(), "Now: Light · Standard");
        assert_eq!(use_(&a, true), (Some(true), Some(false)));
        assert_eq!(use_(&a, false), (Some(false), Some(true)));
        assert_eq!(palette_in_use("appearance.hollow", &a, true), None);
        assert_eq!(
            choice_label("appearance.light_palette", 0, "Same as dark", &a),
            "Same as dark (Colorblind)"
        );
        assert_eq!(
            choice_label("appearance.light_palette", 1, "Standard", &a),
            "Standard"
        );
    }

    /// `--palette cb`, then the trader sets Dark palette: Standard with no light palette of
    /// their own saved: light follows (the flag's light override drops too).
    #[test]
    fn dark_palette_change_drops_the_flags_light_override() {
        let saved = Appearance::default();
        let flags = Appearance {
            palette: Some(PaletteName::Colorblind),
            light_palette: Some(PaletteName::Colorblind),
            ..Default::default()
        };
        let shown = saved.with_flags(flags);
        let after = Appearance {
            palette: None,
            ..shown
        };
        let (s, f) = Appearance::changed(saved, flags, shown, after);
        assert_eq!((f.palette, f.light_palette), (None, None));
        assert_eq!(s.with_flags(f).palette_for(false), PaletteName::Standard);
        // A saved light palette of its own: the flag's light override stays for the run.
        let saved = Appearance {
            light_palette: Some(PaletteName::Standard),
            ..Default::default()
        };
        let shown = saved.with_flags(flags);
        let after = Appearance {
            palette: None,
            ..shown
        };
        let (_, f) = Appearance::changed(saved, flags, shown, after);
        assert_eq!(f.light_palette, Some(PaletteName::Colorblind));
    }

    /// `--palette cb` with no light palette saved shows Light palette as Same as dark (still
    /// colorblind in light); with one saved, the flag shows over it for the run.
    #[test]
    fn palette_flag_shows_light_as_same_as_dark() {
        let flags = Appearance {
            palette: Some(PaletteName::Colorblind),
            light_palette: Some(PaletteName::Colorblind),
            ..Default::default()
        };
        let shown = Appearance::default().with_flags(flags);
        assert_eq!(shown.light_palette, None);
        assert_eq!(shown.palette_for(false), PaletteName::Colorblind);
        let saved = Appearance {
            light_palette: Some(PaletteName::Standard),
            ..Default::default()
        };
        let shown = saved.with_flags(flags);
        assert_eq!(shown.light_palette, Some(PaletteName::Colorblind));
        // Setting it apart from there saves it and drops the flag's override.
        let shown = Appearance::default().with_flags(flags);
        let after = Appearance {
            light_palette: Some(PaletteName::Standard),
            ..shown
        };
        let (s, f) = Appearance::changed(Appearance::default(), flags, shown, after);
        assert_eq!(
            (s.light_palette, f.light_palette),
            (Some(PaletteName::Standard), None)
        );
        assert_eq!(s.with_flags(f).palette_for(false), PaletteName::Standard);
    }

    /// Every theme entry point offers System: the Settings choice, its palette commands and
    /// the `--theme` flag (no key binds a theme).
    #[test]
    fn every_theme_entry_point_lists_system() {
        let i = all()
            .iter()
            .position(|s| s.key == "appearance.theme")
            .unwrap();
        let cmds: Vec<_> = palette()
            .into_iter()
            .filter(|c| c.index as usize == i)
            .map(|c| c.value)
            .collect();
        assert_eq!(cmds, vec![0, 1, 2], "palette: Theme: Dark, Light, System");
        assert!(search("system").contains(&i));
        use clap::ValueEnum;
        let names: Vec<_> = crate::ThemeKind::value_variants()
            .iter()
            .filter_map(|v| v.to_possible_value())
            .map(|v| v.get_name().to_owned())
            .collect();
        assert_eq!(names, ["dark", "light", "system"]);
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
        for st in all().iter().filter(|s| s.key.starts_with("appearance.")) {
            if [
                crate::display_zone::KEY,
                crate::fonts::KEY,
                "appearance.ui_zoom",
            ]
            .contains(&st.key)
            {
                // Its own settings field (no CLI look flag to keep apart): a plain undo.
                continue;
            }
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
                LookField::LightPalette => {
                    then_s.light_palette = Some(PaletteName::Colorblind);
                    then_f.light_palette = Some(PaletteName::Standard);
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
                follow_os: shown.follow_os.filter(|f| *f),
                light_palette: shown.light_palette,
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
            follow_os: Some(false),
            light_palette: None,
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
        assert_eq!(PRESET_CHOICES[0], "Off");
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

    /// The news blackout rows: minutes need the rule on; changing the action keeps the
    /// minutes; Warn and Off loosen (asked, next session), Flatten tightens; Off drops an
    /// entry left empty.
    #[test]
    fn the_news_blackout_is_a_guardrail() {
        let t = risk_target();
        let mut p = Prefs::default();
        let news = by_key("guard.news").unwrap();
        let before = by_key("guard.news_before").unwrap();
        assert_eq!(
            before.apply(&mut p, &t, Value::Number(Some(5))),
            Err("turn the news blackout on first")
        );
        news.apply(&mut p, &t, Value::Choice(2)).unwrap();
        before.apply(&mut p, &t, Value::Number(Some(5))).unwrap();
        by_key("guard.news_medium")
            .unwrap()
            .apply(&mut p, &t, Value::Bool(true))
            .unwrap();
        let g0 = t.guardrails(&p);
        assert_eq!(
            g0.news.map(|n| (n.action, n.before_min, n.include_medium)),
            Some((lines_trading::news::NewsAction::Block, 5, true))
        );
        let with = |k: usize| {
            let mut q = p.clone();
            news.apply(&mut q, &t, Value::Choice(k)).unwrap();
            q
        };
        let warn = with(1);
        assert_eq!(t.guardrails(&warn).news.map(|n| n.before_min), Some(5));
        assert!(loosens(&g0, &t.guardrails(&warn)));
        assert!(!loosens(&g0, &t.guardrails(&with(3))), "flatten is tighter");
        let off = with(0);
        assert!(loosens(&g0, &t.guardrails(&off)));
        assert!(off.app.guardrails.is_empty(), "an empty entry is dropped");
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

    /// Largest order: saved per account under its name, read back, edited in place, removed
    /// when emptied, never under 1, and round-tripped through `settings.json`.
    #[test]
    fn the_largest_order_setting_round_trips() {
        let mut p = Prefs::default();
        let t = Target {
            account: Some("paper-1".into()),
            account_name: Some("Paper".into()),
            product: None,
        };
        let row = by_key(MAX_ORDER_KEY).unwrap();
        assert_eq!((row.get)(&p, &t, &Facts::default()), Value::Number(None));
        row.apply(&mut p, &t, Value::Number(Some(5))).unwrap();
        assert_eq!(p.app.max_order_qty.get("Paper"), Some(&5));
        assert_eq!(t.max_order(&p), Some(5));
        assert!(row.apply(&mut p, &t, Value::Number(Some(0))).is_err());
        let json = String::from_utf8(p.app.encode()).unwrap();
        let back: crate::picker::Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_order_qty.get("Paper"), Some(&5));
        // Saved under the id (as startup also accepts): edited there, no second entry.
        let mut q = Prefs::default();
        q.app.max_order_qty.insert("paper-1".into(), 3);
        row.apply(&mut q, &t, Value::Number(Some(4))).unwrap();
        assert_eq!(q.app.max_order_qty.len(), 1);
        assert_eq!(q.app.max_order_qty["paper-1"], 4);
        // A broker account shows none, whatever is saved (nothing would enforce it).
        let broker = Facts {
            accounts: vec![AccountFact {
                id: "paper-1".into(),
                paper: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!((row.get)(&q, &t, &broker), Value::Number(None));
        // Emptied: no limit, and nothing saved.
        row.apply(&mut q, &t, Value::Number(None)).unwrap();
        assert!(q.app.max_order_qty.is_empty());
        let none = String::from_utf8(q.app.encode()).unwrap();
        assert!(!none.contains("max_order_qty"), "optional: {none}");
    }

    /// Raising or removing a paper account's largest order loosens (asks first, as the other
    /// limits); lowering or setting one where there was none doesn't.
    #[test]
    fn the_largest_order_loosens_when_raised_or_removed() {
        let row = by_key(MAX_ORDER_KEY).unwrap();
        let g = lines_trading::Guardrails::default();
        let n = |x: Option<i64>| Value::Number(x);
        assert!(loosening(row, &n(Some(5)), &n(Some(10)), &g, &g));
        assert!(
            loosening(row, &n(Some(5)), &n(None), &g, &g),
            "↺ removes it"
        );
        assert!(!loosening(row, &n(Some(10)), &n(Some(5)), &g, &g));
        assert!(!loosening(row, &n(None), &n(Some(5)), &g, &g));
        // Its reset is the removal: the one ↺ press asks.
        assert_eq!(row.default, Some(n(None)));
    }

    /// A change's confirmation names the value the setting will show: a prop override reset
    /// to the preset's (`Off` is stored) reads as the preset's value, not "Off".
    #[test]
    fn a_change_names_the_value_it_leaves() {
        let t = acct();
        let f = Facts::default();
        let mut p = Prefs::default();
        by_key("prop.preset")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(1))
            .unwrap();
        // The preset's trailing drawdown (it has one; its daily loss may be off).
        let daily = by_key("prop.drawdown").unwrap();
        let preset = shown_after(daily, &p, &t, &f);
        assert_ne!(preset, "Off");
        daily.apply(&mut p, &t, Value::Number(Some(700))).unwrap();
        assert_eq!(shown_after(daily, &p, &t, &f), "$700");
        let mut after = p.clone();
        daily
            .apply(&mut after, &t, daily.reset_value(&p, &t).unwrap())
            .unwrap();
        assert_eq!(daily.reset_value(&p, &t), Some(Value::Number(None)));
        assert_eq!(
            shown_after(daily, &after, &t, &f),
            preset,
            "the preset's value"
        );
        // A guardrail removed reads as Off.
        let loss = by_key("guard.max_daily_loss").unwrap();
        let mut q = Prefs::default();
        loss.apply(&mut q, &t, Value::Number(Some(1_000))).unwrap();
        loss.apply(&mut q, &t, Value::Number(None)).unwrap();
        assert_eq!(shown_after(loss, &q, &t, &f), "Off");
    }

    /// A look flag this run (`--theme light`) rules its row: the value shows, but no ● or ↺
    /// (nothing is saved, and a reset would save the default over nothing).
    #[test]
    fn a_look_flag_has_no_marks() {
        let t = acct();
        let mut p = Prefs::default();
        p.app.appearance.theme = Some(ThemeName::Light);
        let theme = by_key("appearance.theme").unwrap();
        assert_eq!(marks(theme, &p, &t, &Facts::default()), (true, true));
        let flagged = Facts {
            look_flags: Appearance {
                theme: Some(ThemeName::Light),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(marks(theme, &p, &t, &flagged), (false, false));
        assert!(!section_changed(Section::Appearance, &p, &t, &flagged));
        // Another look field without a flag keeps its marks.
        p.app.appearance.palette = Some(PaletteName::Colorblind);
        let palette = by_key("appearance.palette").unwrap();
        assert_eq!(marks(palette, &p, &t, &flagged), (true, true));
    }

    /// The section names before the regrouping still find their rows.
    #[test]
    fn old_section_names_still_search() {
        let keys = |q: &str| -> Vec<&str> { search(q).iter().map(|&i| all()[i].key).collect() };
        assert!(keys("risk").contains(&"guard.max_daily_loss"));
        assert!(keys("risk").contains(&"prop.preset"));
        assert!(keys("trading").contains(&"trading.bracket_units"));
        assert!(keys("diagnostics").contains(&"diagnostics.latency_hud"));
        assert!(keys("data").contains(&"data.budget"));
        assert!(keys("data").contains(&"data.kraken"));
    }

    /// The page and search draw a setting on the same line, with the same label: the prop
    /// overrides by their own names (never a second "Daily loss limit"), the cooldown as
    /// "Cooldown".
    #[test]
    fn page_and_search_use_one_label() {
        let label = |k: &str| line_label(&places()[index_of(k).unwrap()].unwrap().line);
        assert_eq!(label("guard.max_daily_loss"), "Daily loss limit");
        assert_eq!(label("prop.daily_loss"), "Prop daily loss limit");
        assert_eq!(label("guard.cooldown"), "Cooldown");
        let mut labels: Vec<&str> = Section::ALL
            .iter()
            .flat_map(|s| page(*s).iter())
            .flat_map(|g| g.lines.iter())
            .map(line_label)
            .collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "every line's label is its own");
    }

    /// The palette's toggles loosen as the rows do: turning the news blackout's medium
    /// releases off on an account that has them on asks first (it did not, with no
    /// guardrails passed); turning it on doesn't.
    #[test]
    fn palette_changes_loosen_with_the_real_guardrails() {
        let t = acct();
        let f = Facts::default();
        let mut before = Prefs::default();
        by_key("guard.news")
            .unwrap()
            .apply(&mut before, &t, Value::Choice(2))
            .unwrap();
        let medium = by_key("guard.news_medium").unwrap();
        medium.apply(&mut before, &t, Value::Bool(true)).unwrap();
        let mut after = before.clone();
        medium.apply(&mut after, &t, Value::Bool(false)).unwrap();
        let (old, new) = (Value::Bool(true), Value::Bool(false));
        assert!(change_loosens(
            medium,
            (&old, &new),
            (&before, &after),
            &t,
            &f
        ));
        assert!(
            !loosening(medium, &old, &new, &Default::default(), &Default::default()),
            "the old palette path saw nothing to ask"
        );
        assert!(!change_loosens(
            medium,
            (&new, &old),
            (&after, &before),
            &t,
            &f
        ));
        // A prop override raised asks too.
        let mut p0 = Prefs::default();
        by_key("prop.preset")
            .unwrap()
            .apply(&mut p0, &t, Value::Choice(1))
            .unwrap();
        let dd = by_key("prop.drawdown").unwrap();
        let mut p1 = p0.clone();
        dd.apply(&mut p1, &t, Value::Number(Some(99_999))).unwrap();
        let (a, b) = ((dd.get)(&p0, &t, &f), (dd.get)(&p1, &t, &f));
        assert!(change_loosens(dd, (&a, &b), (&p0, &p1), &t, &f));
    }

    /// While a guardrail is off its parts rest: the cooldown is hidden (its line reads just
    /// "Off"), the news window shows the minutes it starts with; once on, both are live.
    #[test]
    fn parts_rest_while_their_guardrail_is_off() {
        let t = acct();
        let mut p = Prefs::default();
        let cool = by_key("guard.cooldown").unwrap();
        let before = by_key("guard.news_before").unwrap();
        assert_eq!(dormant(cool, &p, &t), Some(None));
        assert_eq!(dormant(before, &p, &t), Some(Some(2)));
        by_key("guard.losing_streak")
            .unwrap()
            .apply(&mut p, &t, Value::Number(Some(3)))
            .unwrap();
        by_key("guard.news")
            .unwrap()
            .apply(&mut p, &t, Value::Choice(1))
            .unwrap();
        assert_eq!(dormant(cool, &p, &t), None);
        assert_eq!(dormant(before, &p, &t), None);
        assert_eq!(
            dormant(by_key("guard.max_daily_loss").unwrap(), &p, &t),
            None
        );
    }

    /// A search counts lines, as the page shows them: the cooldown's two settings are one.
    #[test]
    fn search_counts_lines() {
        let hits = search("cooldown");
        assert_eq!(hits.len(), 2);
        assert_eq!(line_count(&hits), 1);
        assert_eq!(line_count(&[]), 0);
    }

    /// Max position is a cap per contract, as the core checks it (`ContractKey`: product and
    /// month): its value says so ("2 per contract"), never read as an account total or a
    /// product total. The other Accounts limits are per account.
    #[test]
    fn max_position_says_per_contract() {
        let pos = by_key("guard.max_position").unwrap();
        assert_eq!(pos.value_text(&Value::Number(Some(2))), "2 per contract");
        assert_eq!(pos.value_text(&Value::Number(None)), "Off");
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
        // A page's own label finds its setting; results come in window order.
        assert!(names("loses focus").contains(&"trading.disable_on_focus_loss"));
        let order: Vec<usize> = names("a")
            .iter()
            .map(|k| places()[index_of(k).unwrap()].unwrap().order)
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
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
        assert!(
            !json.contains("hide_join_buttons"),
            "Join buttons shown: absent: {json}"
        );
        by_key("dom.join_buttons")
            .unwrap()
            .apply(&mut p, &t, Value::Bool(false))
            .unwrap();
        let hidden = String::from_utf8(p.app.encode()).unwrap();
        assert!(hidden.contains("\"hide_join_buttons\": true"), "{hidden}");
        by_key("dom.join_buttons")
            .unwrap()
            .apply(&mut p, &t, Value::Bool(true))
            .unwrap();
        let shown = String::from_utf8(p.app.encode()).unwrap();
        assert!(!shown.contains("hide_join_buttons"), "{shown}");
        // A newer version's key survives a load and a save.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut o = v.as_object().unwrap().clone();
        o.insert("from_the_future".into(), serde_json::json!({"x": 1}));
        let back: Settings = serde_json::from_value(serde_json::Value::Object(o)).unwrap();
        let again = String::from_utf8(back.encode()).unwrap();
        assert!(again.contains("from_the_future"), "{again}");
        assert_eq!(back.appearance.palette, Some(PaletteName::Colorblind));
    }

    /// Settings › Data folder's store fields: absent by default, written only when changed, a depth
    /// symbol this build doesn't know kept through a load and a save.
    #[test]
    fn data_rows_save_optional_fields() {
        let mut p = Prefs::default();
        let t = acct();
        let json = String::from_utf8(p.app.encode()).unwrap();
        for k in ["\"store\"", "store_budget_gb", "store_depth"] {
            assert!(!json.contains(k), "absent by default: {json}");
        }
        by_key("data.store")
            .unwrap()
            .apply(&mut p, &t, Value::Bool(false))
            .unwrap();
        by_key("data.budget")
            .unwrap()
            .apply(&mut p, &t, Value::Number(Some(20)))
            .unwrap();
        assert!(
            by_key("data.budget")
                .unwrap()
                .apply(&mut p, &t, Value::Number(Some(0)))
                .is_err(),
            "at least 1 GiB"
        );
        let depth = by_key("data.depth").unwrap();
        depth
            .apply(&mut p, &t, Value::List(vec!["ES".into(), "ZZZ".into()]))
            .unwrap();
        let json = String::from_utf8(p.app.encode()).unwrap();
        assert!(json.contains("\"store\": false"), "{json}");
        assert!(json.contains("\"store_budget_gb\": 20.0"), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.store_depth, Some(vec!["ES".into(), "ZZZ".into()]));
        let q = Prefs {
            app: back,
            ..Prefs::default()
        };
        assert_eq!(
            (depth.get)(&q, &t, &Facts::default()),
            Value::List(vec!["ES".into(), "ZZZ".into()])
        );
        // The unknown one is offered (to remove), marked unavailable.
        let choices = depth_choices(&["ES".into(), "ZZZ".into()]);
        assert_eq!(choices.last(), Some(&("ZZZ".to_owned(), false)));
        assert!(choices.contains(&("ES".to_owned(), true)));
        assert_eq!(
            toggled(&["ES".into(), "ZZZ".into()], "zzz"),
            vec!["ES".to_owned()]
        );
        assert_eq!(toggled(&[], "NQ"), vec!["NQ".to_owned()]);
        // Back to the defaults: nothing written.
        for key in ["data.store", "data.budget", "data.depth"] {
            let st = by_key(key).unwrap();
            st.apply(&mut p, &t, st.default.clone().unwrap()).unwrap();
        }
        assert_eq!(p, Prefs::default());
    }

    /// A changed Data value says it applies after restart; a flag says it rules this run.
    #[test]
    fn data_rows_say_restart_or_flag() {
        let t = acct();
        let mut f = Facts::default();
        let mut p = Prefs::default();
        let budget = by_key("data.budget").unwrap();
        assert_eq!(status(budget, &p, &t, &f), None);
        budget.apply(&mut p, &t, Value::Number(Some(8))).unwrap();
        assert_eq!(
            status(budget, &p, &t, &f).as_deref(),
            Some("Applies after restart")
        );
        // The run started with 8: nothing waits.
        f.store_start.budget_gb = Some(8.0);
        assert_eq!(status(budget, &p, &t, &f), None);
        f.store_flags.budget_gb = Some(2.0);
        assert_eq!(
            status(budget, &p, &t, &f).as_deref(),
            Some("2 GiB for this run (--store-budget-gb) · saved: 8 GiB")
        );
        f.store_flags.budget_gb = Some(2.5);
        assert_eq!(
            status(budget, &p, &t, &f).as_deref(),
            Some("2.5 GiB for this run (--store-budget-gb) · saved: 8 GiB")
        );
        f.store_flags.no_store = true;
        let store = by_key("data.store").unwrap();
        assert_eq!(
            status(store, &p, &t, &f).as_deref(),
            Some("Off for this run (--no-store) · saved: On")
        );
        // The control shows the value in force, not the saved one.
        assert_eq!(flag_value(store, &f), Some(Value::Bool(false)));
        assert_eq!(flag_text(budget, &f).as_deref(), Some("2.5 GiB"));
        assert_eq!(flag_value(by_key("data.depth").unwrap(), &f), None);
        let depth = by_key("data.depth").unwrap();
        depth
            .apply(&mut p, &t, Value::List(vec!["nq".into()]))
            .unwrap();
        f.store_start.depth = Some(vec!["NQ".into()]);
        assert_eq!(
            status(depth, &p, &t, &f),
            None,
            "case and order don't matter"
        );
    }
}
