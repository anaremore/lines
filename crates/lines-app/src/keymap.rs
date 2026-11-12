//! The effective keymap: the defaults (`commands::BINDINGS`) plus the user's changes, saved in
//! `settings.json` (`key_bindings`) as only the differences: for each changed command, its
//! whole list of keys. Pure and GPUI-free, so every rule is tested without a window.
//!
//! - **One table for everything:** `commands::command_for` (the key path), menu hints, tooltips,
//!   the palette, F1 hints, keycaps and the DOM's labels all read the installed table
//!   ([`install`], [`with`]). It is built when the settings change, never on the key path:
//!   [`Keymap::lookup`] reads a lock and scans, allocating nothing.
//! - **Rules** ([`refusal`]): any command that trades (`Command::Trade`, every strategy key)
//!   keeps Ctrl+Shift or Ctrl+Alt; Flatten, Cancel all and Stop all strategies always keep a
//!   key; bare letters, digits and characters (pane typing), Esc, Enter, Space, Tab, bare arrows
//!   and Home, the Alt+arrows and Ctrl+1–9 patterns, the OS key and the panes' own keys (Ctrl+K,
//!   Ctrl+D, Alt+M, Alt+V/R/B/G) are refused, each with its reason. The replay's plain keys
//!   (Space, ← →, [ ]) are fixed: shown, never moved or removed.
//! - **Never twice:** a key is on one command; taking one another command has offers Replace
//!   ([`Keymap::check`] → [`Check::Conflict`]), which moves it.
//! - **Loading** skips, and reports, unknown commands, unreadable keys, refused keys and keys
//!   already taken; never fatal.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::commands::{BINDINGS, Command, StrategyCmd, TradeCmd};

/// Keys in messages as the platform shows them (macOS: ⌘ for Ctrl).
const MAC: bool = cfg!(target_os = "macos");

/// What `settings.json` keeps: each changed command (by [`id`]) with its whole list of keys
/// (saved form, [`Combo::saved`]); an empty list: no key. Optional; only the differences.
pub type Saved = BTreeMap<String, Vec<String>>;

/// One keystroke: Ctrl (⌘ on macOS), Shift, Alt and GPUI's key name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Combo {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
}

impl Combo {
    pub fn new(ctrl: bool, shift: bool, alt: bool, key: &str) -> Combo {
        Combo {
            ctrl,
            shift,
            alt,
            key: key.to_lowercase(),
        }
    }

    /// As saved: "ctrl+shift+f" (modifiers in that order, then the key).
    pub fn saved(&self) -> String {
        let mut s = String::new();
        for (on, m) in [
            (self.ctrl, "ctrl"),
            (self.shift, "shift"),
            (self.alt, "alt"),
        ] {
            if on {
                s.push_str(m);
                s.push('+');
            }
        }
        s.push_str(&self.key);
        s
    }

    /// A saved (or typed) key: "ctrl+shift+f", "Ctrl+Alt+Up", "f1". The key may be "+" itself
    /// ("ctrl++"). `None`: not a key.
    pub fn parse(s: &str) -> Option<Combo> {
        let s = s.trim().to_lowercase();
        if s.is_empty() {
            return None;
        }
        let (mods, key) = if let Some(m) = s.strip_suffix("++") {
            (m, "+")
        } else {
            match s.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", s.as_str()),
            }
        };
        if key.is_empty() {
            return None;
        }
        let mut c = Combo::new(false, false, false, key);
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            match m {
                "ctrl" | "cmd" | "⌘" => c.ctrl = true,
                "shift" | "⇧" => c.shift = true,
                "alt" | "option" | "⌥" => c.alt = true,
                _ => return None,
            }
        }
        Some(c)
    }

    /// One cap per key, in the platform's order (macOS ⌥ ⇧ ⌘; elsewhere Ctrl Alt Shift), then
    /// the key's name as drawn ("→", "Comma", "F").
    pub fn caps(&self, mac: bool) -> Vec<String> {
        let all: [(bool, &str); 3] = if mac {
            [(self.alt, "⌥"), (self.shift, "⇧"), (self.ctrl, "⌘")]
        } else {
            [
                (self.ctrl, "Ctrl"),
                (self.alt, "Alt"),
                (self.shift, "Shift"),
            ]
        };
        let mut v: Vec<String> = all
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, k)| (*k).to_owned())
            .collect();
        v.push(key_name(&self.key, mac));
        v
    }

    /// As text: "Ctrl+Shift+F" (macOS "⇧⌘F").
    pub fn text(&self, mac: bool) -> String {
        self.caps(mac).join(if mac { "" } else { "+" })
    }

    fn is(&self, ctrl: bool, shift: bool, alt: bool, key: &str) -> bool {
        self.ctrl == ctrl && self.shift == shift && self.alt == alt && self.key == key
    }
}

/// A key's name as drawn on its cap.
pub fn key_name(key: &str, mac: bool) -> String {
    match key {
        "right" => "→".to_owned(),
        "left" => "←".to_owned(),
        "down" => "↓".to_owned(),
        "up" => "↑".to_owned(),
        "tab" => "Tab".to_owned(),
        "pagedown" => "PgDn".to_owned(),
        "pageup" => "PgUp".to_owned(),
        "space" => "Space".to_owned(),
        "escape" => "Esc".to_owned(),
        "enter" => "Enter".to_owned(),
        "backspace" => "⌫".to_owned(),
        "delete" => "Del".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        // A lone comma reads as punctuation in a row's hint: named, as macOS shows "⌘,".
        "," if !mac => "Comma".to_owned(),
        k => k.to_uppercase(),
    }
}

/// A command's id in `settings.json`: its name as the code has it ("Trade(Flatten)"). A name
/// that no longer exists is skipped and reported when loading.
pub fn id(c: Command) -> String {
    format!("{c:?}")
}

/// Every command that can have keys: the palette's, then any other a default binds, in that
/// order (Settings › Keys lists them so). Patterns (Alt+arrows, Ctrl+1–9) and Settings at a
/// section (Settings' own key) are not among them.
pub fn bindable() -> Vec<Command> {
    let mut v: Vec<Command> = Vec::new();
    for c in crate::commands::palette()
        .into_iter()
        .chain(BINDINGS.iter().map(|b| b.cmd))
    {
        if matches!(
            c,
            Command::Focus(_) | Command::SelectTab(_) | Command::SettingsAt(_)
        ) || v.contains(&c)
        {
            continue;
        }
        v.push(c);
    }
    v
}

/// A command's group in Settings › Keys (its chip).
pub fn group(c: Command) -> &'static str {
    match c {
        Command::Trade(_) | Command::Order(_) | Command::CyclePnl | Command::SetPnl(_) => "Trading",
        Command::Strategy(_) => "Strategies",
        Command::Draw(_) => "Drawing",
        Command::Replay(_) => "Replay",
        Command::Timeframe(_)
        | Command::TimeframeMenu
        | Command::TickBarsContinuous
        | Command::Studies
        | Command::SeparateStudies
        | Command::ToggleStudyLabels
        | Command::ToggleStudies
        | Command::CleanChart
        | Command::VolumeOverlay(_)
        | Command::ToggleTrades
        | Command::TradesOnAllCharts(_)
        | Command::ToggleTradeLines
        | Command::ToggleTradeLabels
        | Command::TradeAccounts(_)
        | Command::TradePeriod(_)
        | Command::ToggleLevels
        | Command::ToggleLevelLabels
        | Command::LevelLabelPlace(_)
        | Command::ProfileLabel(_)
        | Command::ToggleCrosshairSync
        | Command::ToggleCrosshairSyncPrice
        | Command::ToggleMagnet
        | Command::ToggleOrderStrip
        | Command::ToggleDrawingTools => "Charts",
        _ => "Workspace",
    }
}

/// The groups, in the chips' order.
pub const GROUPS: [&str; 6] = [
    "Workspace",
    "Charts",
    "Drawing",
    "Trading",
    "Strategies",
    "Replay",
];

/// A command that trades (or sends, starts or stops a strategy): its keys keep Ctrl+Shift or
/// Ctrl+Alt (Alt alone types on macOS and moves focus; Shift alone types).
pub fn trades(c: Command) -> bool {
    matches!(c, Command::Trade(_) | Command::Strategy(_))
}

/// Flatten, Cancel all and Stop all strategies: never left without a key.
pub fn keeps_a_key(c: Command) -> bool {
    matches!(
        c,
        Command::Trade(TradeCmd::Flatten | TradeCmd::CancelAll)
            | Command::Strategy(StrategyCmd::StopAll)
    )
}

/// A command's default keys (`BINDINGS`).
fn default_keys(c: Command) -> Vec<Combo> {
    BINDINGS
        .iter()
        .filter(|b| b.cmd == c)
        .map(|b| Combo::new(b.ctrl, b.shift, b.alt, b.key))
        .collect()
}

/// The short name of a command that always keeps a key (for a row's refusal).
fn emergency_name(c: Command) -> String {
    match c {
        Command::Trade(TradeCmd::Flatten) => "Flatten".into(),
        Command::Trade(TradeCmd::CancelAll) => "Cancel all".into(),
        Command::Strategy(StrategyCmd::StopAll) => "Stop all strategies".into(),
        c => crate::commands::label(c).to_string(),
    }
}

/// A default key that stays as it is: the replay's plain keys (Space plays and pauses; ← →
/// step; [ ] change the speed; they act only while a replay plays and the focused pane doesn't
/// use the key).
pub fn fixed(c: Command, k: &Combo) -> bool {
    matches!(c, Command::Replay(_))
        && !k.ctrl
        && !k.alt
        && BINDINGS
            .iter()
            .any(|b| b.cmd == c && k.is(b.ctrl, b.shift, b.alt, b.key))
}

/// Why `k` can't be one of `c`'s keys, if it can't (whatever else has it).
pub fn refusal(c: Command, k: &Combo) -> Option<String> {
    let key = k.key.as_str();
    let arrow = matches!(key, "left" | "right" | "up" | "down");
    let plain = !k.ctrl && !k.alt;
    let single = key.chars().count() == 1;
    let name = || k.text(MAC);
    if plain && single && key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(format!(
            "{}: letters and digits type in panes (symbols, timeframes, sizes)",
            name()
        ));
    }
    if plain && single {
        return Some(format!("{}: characters type in panes and notes", name()));
    }
    if plain && matches!(key, "escape" | "enter" | "space" | "tab" | "backspace") {
        return Some(format!(
            "{}: stays with lists, fields and the replay",
            name()
        ));
    }
    if plain && (arrow || matches!(key, "home" | "end" | "pageup" | "pagedown")) {
        return Some(format!("{}: moves within a pane", name()));
    }
    if k.alt && !k.ctrl && !k.shift && arrow {
        return Some(format!("{}: Alt+arrows move between panes", name()));
    }
    if k.ctrl
        && !k.alt
        && !k.shift
        && single
        && ('1'..='9').contains(&key.chars().next().unwrap_or(' '))
    {
        return Some(format!("{}: Ctrl+1–9 go to tabs", name()));
    }
    let pane_local = (k.ctrl && !k.alt && !k.shift && matches!(key, "k" | "d"))
        || (k.alt && !k.ctrl && !k.shift && matches!(key, "m" | "v" | "r" | "b" | "g"));
    if pane_local {
        return Some(format!("{}: a pane uses it (picker, DOM, profile)", name()));
    }
    if trades(c) && !(k.ctrl && (k.shift || k.alt)) {
        let need = if MAC {
            "⇧⌘ or ⌥⌘"
        } else {
            "Ctrl+Shift or Ctrl+Alt"
        };
        return Some(format!("{}: order keys need {need}", name()));
    }
    None
}

/// What setting a key would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// It can be set.
    Free,
    /// Already this command's.
    Same,
    /// Another command has it: Replace moves it.
    Conflict(Command),
    /// Refused, with the reason.
    Refused(String),
}

/// The effective table: each command's keys, in the defaults' order (a command's first key is
/// its hint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    binds: Vec<(Command, Combo)>,
}

impl Keymap {
    /// The defaults: exactly `commands::BINDINGS`.
    pub fn defaults() -> Keymap {
        Keymap {
            binds: BINDINGS
                .iter()
                .map(|b| (b.cmd, Combo::new(b.ctrl, b.shift, b.alt, b.key)))
                .collect(),
        }
    }

    /// The defaults with `saved`'s changes, and what was skipped (each a sentence for the
    /// footer's report).
    pub fn with(saved: &Saved) -> (Keymap, Vec<String>) {
        let mut m = Keymap::defaults();
        let mut skipped = Vec::new();
        let all = bindable();
        // First every changed command's keys go, then each gets its saved ones: a key moved
        // from one command to another (both saved) lands, whatever the order.
        let mut changes: Vec<(Command, &Vec<String>)> = Vec::new();
        for (name, keys) in saved {
            match all.iter().find(|c| id(**c) == *name) {
                Some(c) => changes.push((*c, keys)),
                None => skipped.push(format!(
                    "Key bindings: no command \"{name}\" in this version (skipped)"
                )),
            }
        }
        for (c, _) in &changes {
            m.binds.retain(|(b, k)| b != c || fixed(*c, k));
        }
        for (c, keys) in changes {
            for text in keys {
                let Some(k) = Combo::parse(text) else {
                    skipped.push(format!(
                        "Key bindings: {}: can't read key \"{text}\" (skipped)",
                        crate::commands::label(c)
                    ));
                    continue;
                };
                // A fixed key is saved with its command's list and never left it.
                if fixed(c, &k) {
                    continue;
                }
                match m.check(c, &k) {
                    Check::Free => m.binds.push((c, k)),
                    Check::Same => {}
                    Check::Conflict(o) => skipped.push(format!(
                        "Key bindings: {} for {}: already {}'s (skipped)",
                        k.text(cfg!(target_os = "macos")),
                        crate::commands::label(c),
                        crate::commands::label(o)
                    )),
                    Check::Refused(why) => skipped.push(format!(
                        "Key bindings: {} for {} refused: {why}",
                        k.text(cfg!(target_os = "macos")),
                        crate::commands::label(c)
                    )),
                }
            }
        }
        // A command that must keep a key and lost every one: its defaults back (reported).
        skipped.extend(m.keep_emergency_keys());
        (m, skipped)
    }

    /// Flatten, Cancel all and Stop all strategies each with a key: one left without any
    /// takes its defaults back from whoever has them; if another of the three holds one as
    /// its only key, all three go back to their defaults. What was done, as sentences.
    fn keep_emergency_keys(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        let emergency: Vec<Command> = bindable().into_iter().filter(|c| keeps_a_key(*c)).collect();
        for &c in &emergency {
            if self.keys(c).next().is_some() {
                continue;
            }
            for k in default_keys(c) {
                match self.lookup(k.ctrl, k.shift, k.alt, &k.key) {
                    None => self.binds.push((c, k)),
                    Some(o) if keeps_a_key(o) && self.keys(o).count() == 1 => {}
                    Some(o) => {
                        said.push(format!(
                            "Key bindings: {} moved back from {} to {}",
                            k.text(MAC),
                            crate::commands::label(o),
                            emergency_name(c)
                        ));
                        self.binds.retain(|(_, x)| *x != k);
                        self.binds.push((c, k));
                    }
                }
            }
            if self.keys(c).next().is_some() {
                said.push(format!(
                    "Key bindings: {} always keeps a key: its default is back",
                    emergency_name(c)
                ));
            }
        }
        if emergency.iter().any(|c| self.keys(*c).next().is_none()) {
            for &c in &emergency {
                self.binds.retain(|(b, _)| *b != c);
            }
            for &c in &emergency {
                for k in default_keys(c) {
                    self.binds.retain(|(_, x)| *x != k);
                    self.binds.push((c, k));
                }
            }
            said.push(
                "Key bindings: Flatten, Cancel all and Stop all strategies are back to their                  default keys"
                    .to_owned(),
            );
        }
        self.in_default_order();
        said
    }

    /// The command a key runs. Allocates nothing (the key path).
    pub fn lookup(&self, ctrl: bool, shift: bool, alt: bool, key: &str) -> Option<Command> {
        self.binds
            .iter()
            .find(|(_, k)| k.is(ctrl, shift, alt, key))
            .map(|(c, _)| *c)
    }

    /// A command's keys, the first being its hint.
    pub fn keys(&self, c: Command) -> impl Iterator<Item = &Combo> {
        self.binds
            .iter()
            .filter(move |(b, _)| *b == c)
            .map(|(_, k)| k)
    }

    /// Whether a command's keys differ from its defaults.
    pub fn changed(&self, c: Command) -> bool {
        let now: Vec<&Combo> = self.keys(c).collect();
        let def: Vec<Combo> = BINDINGS
            .iter()
            .filter(|b| b.cmd == c)
            .map(|b| Combo::new(b.ctrl, b.shift, b.alt, b.key))
            .collect();
        now.len() != def.len() || now.iter().zip(&def).any(|(a, b)| **a != *b)
    }

    /// How many commands' keys differ from their defaults.
    pub fn changed_count(&self) -> usize {
        bindable().into_iter().filter(|c| self.changed(*c)).count()
    }

    /// What `settings.json` keeps: each changed command's whole list.
    pub fn saved(&self) -> Saved {
        bindable()
            .into_iter()
            .filter(|c| self.changed(*c))
            .map(|c| (id(c), self.keys(c).map(Combo::saved).collect()))
            .collect()
    }

    /// Whether `k` can be one of `c`'s keys now.
    pub fn check(&self, c: Command, k: &Combo) -> Check {
        if let Some(why) = refusal(c, k) {
            return Check::Refused(why);
        }
        match self.lookup(k.ctrl, k.shift, k.alt, &k.key) {
            None => Check::Free,
            Some(o) if o == c => Check::Same,
            Some(o) if fixed(o, k) => Check::Refused(format!(
                "{}: stays with {}",
                k.text(MAC),
                crate::commands::label(o)
            )),
            Some(o) if keeps_a_key(o) && self.keys(o).count() == 1 => Check::Refused(format!(
                "{}: {}'s only key (it always keeps one)",
                k.text(MAC),
                emergency_name(o)
            )),
            Some(o) => Check::Conflict(o),
        }
    }

    /// Give `c` key `k`, in place of `old` (or added): `Ok` with what changed, `Err` with why
    /// not. `take`: a conflict's Replace (the key moves from its command).
    pub fn set(
        &mut self,
        c: Command,
        old: Option<&Combo>,
        k: Combo,
        take: bool,
    ) -> Result<(), String> {
        if let Some(o) = old
            && fixed(c, o)
        {
            return Err(format!("{}: fixed", o.text(MAC)));
        }
        match self.check(c, &k) {
            Check::Refused(why) => return Err(why),
            Check::Same => return Ok(()),
            Check::Conflict(other) if !take => {
                return Err(format!(
                    "{} is already {}'s",
                    k.text(MAC),
                    crate::commands::label(other)
                ));
            }
            Check::Conflict(_) => self.binds.retain(|(_, b)| *b != k),
            Check::Free => {}
        }
        match old.and_then(|o| self.binds.iter().position(|(b, x)| *b == c && x == o)) {
            Some(i) => self.binds[i].1 = k,
            None => self.binds.push((c, k)),
        }
        Ok(())
    }

    /// Remove one of `c`'s keys (⌫ while it is captured).
    pub fn remove(&mut self, c: Command, k: &Combo) -> Result<(), String> {
        if fixed(c, k) {
            return Err(format!("{}: fixed", k.text(MAC)));
        }
        if keeps_a_key(c) && self.keys(c).count() <= 1 {
            return Err(format!(
                "{}: {} always keeps a key",
                k.text(MAC),
                emergency_name(c)
            ));
        }
        self.binds.retain(|(b, x)| !(*b == c && x == k));
        Ok(())
    }

    /// `c`'s keys back to its defaults (↺); a default another command took since comes back
    /// from it (unless that would leave a command that keeps a key without one).
    /// A command that keeps a key and would be left with none keeps the keys it had.
    pub fn reset(&mut self, c: Command) {
        let before = self.binds.clone();
        let defaults = default_keys(c);
        self.binds.retain(|(b, _)| *b != c);
        for k in defaults {
            if let Some(o) = self.lookup(k.ctrl, k.shift, k.alt, &k.key) {
                if keeps_a_key(o) && self.keys(o).count() == 1 {
                    continue;
                }
                self.binds.retain(|(_, x)| *x != k);
            }
            self.binds.push((c, k));
        }
        if keeps_a_key(c) && self.keys(c).next().is_none() {
            self.binds = before;
            return;
        }
        self.in_default_order();
    }

    /// Defaults in `BINDINGS`' order, then the keys added (as they were added): a command's
    /// hint (its first key) is its default's once reset.
    fn in_default_order(&mut self) {
        let at = |c: &Command, k: &Combo| {
            BINDINGS
                .iter()
                .position(|b| b.cmd == *c && k.is(b.ctrl, b.shift, b.alt, b.key))
                .unwrap_or(usize::MAX)
        };
        self.binds.sort_by_key(|(c, k)| at(c, k));
    }
}

fn current() -> &'static RwLock<Option<Arc<Keymap>>> {
    static C: RwLock<Option<Arc<Keymap>>> = RwLock::new(None);
    &C
}

fn defaults() -> &'static Keymap {
    static D: OnceLock<Keymap> = OnceLock::new();
    D.get_or_init(Keymap::defaults)
}

/// Install the effective table (at startup, and whenever the user changes a key).
/// Whether Settings › Keys is capturing a key (`command_for` then runs no trading command).
static CAPTURING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn capturing() -> bool {
    CAPTURING.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_capturing(on: bool) {
    CAPTURING.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn install(m: Keymap) {
    *current().write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(m));
}

/// Read the effective table (the defaults until one is installed).
pub fn with<R>(f: impl FnOnce(&Keymap) -> R) -> R {
    let g = current().read().unwrap_or_else(|e| e.into_inner());
    match g.as_deref() {
        Some(m) => f(m),
        None => f(defaults()),
    }
}

/// A copy of the effective table (for editing).
pub fn snapshot() -> Keymap {
    with(Keymap::clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{DrawCmd, ReplayCmd};

    fn k(s: &str) -> Combo {
        Combo::parse(s).unwrap()
    }

    /// With nothing saved, the effective table is exactly `BINDINGS`, in order.
    #[test]
    fn the_default_table_is_bindings() {
        let m = Keymap::defaults();
        assert_eq!(m.binds.len(), BINDINGS.len());
        for (b, (c, x)) in BINDINGS.iter().zip(&m.binds) {
            assert_eq!(*c, b.cmd);
            assert_eq!(*x, Combo::new(b.ctrl, b.shift, b.alt, b.key));
        }
        let (loaded, skipped) = Keymap::with(&Saved::new());
        assert_eq!(loaded, m);
        assert!(skipped.is_empty());
        assert!(m.saved().is_empty());
        assert_eq!(m.changed_count(), 0);
    }

    /// Every default passes the rules (a fixed replay key aside), and every command that
    /// trades has Ctrl+Shift or Ctrl+Alt on every key.
    #[test]
    fn the_defaults_keep_the_rules() {
        for b in BINDINGS {
            let c = Combo::new(b.ctrl, b.shift, b.alt, b.key);
            if fixed(b.cmd, &c) {
                continue;
            }
            assert_eq!(refusal(b.cmd, &c), None, "{:?} {}", b.cmd, c.saved());
            if trades(b.cmd) {
                assert!(c.ctrl && (c.shift || c.alt), "{:?}", b.cmd);
            }
        }
    }

    /// For every command that trades, a key without Ctrl+Shift or Ctrl+Alt is refused; with
    /// either it isn't (on that rule).
    #[test]
    fn order_keys_keep_ctrl_shift_or_ctrl_alt() {
        for c in bindable().into_iter().filter(|c| trades(*c)) {
            for bad in [
                "ctrl+f9",
                "alt+f9",
                "shift+f9",
                "f9",
                "alt+shift+f9",
                "ctrl+,",
            ] {
                let r = refusal(c, &k(bad));
                assert!(r.is_some(), "{c:?} {bad}");
            }
            for good in ["ctrl+shift+f9", "ctrl+alt+f9", "ctrl+shift+alt+f9"] {
                assert_eq!(refusal(c, &k(good)), None, "{c:?} {good}");
            }
        }
        let why = refusal(Command::Trade(TradeCmd::BuyMarket), &k("b")).unwrap();
        assert!(why.contains("letters"), "{why}");
        let why = refusal(Command::Trade(TradeCmd::BuyMarket), &k("ctrl+f9")).unwrap();
        if !MAC {
            assert_eq!(why, "Ctrl+F9: order keys need Ctrl+Shift or Ctrl+Alt");
        }
    }

    #[test]
    fn the_refused_keys() {
        let c = Command::NewTab;
        for bad in [
            "a", "shift+a", "7", "escape", "enter", "space", "tab", "home", "left", "-",
            "alt+left", "ctrl+1", "ctrl+9", "ctrl+k", "ctrl+d", "alt+m", "alt+v",
        ] {
            assert!(refusal(c, &k(bad)).is_some(), "{bad}");
        }
        for ok in [
            "ctrl+0",
            "f2",
            "shift+f10",
            "ctrl+shift+1",
            "ctrl+alt+left",
            "ctrl+j",
        ] {
            assert_eq!(refusal(c, &k(ok)), None, "{ok}");
        }
    }

    /// A key is on one command: taking another's offers Replace, which moves it.
    #[test]
    fn a_key_moves_on_replace_only() {
        let mut m = Keymap::defaults();
        let dom = m.lookup(true, true, false, "d").unwrap();
        let c = Command::ToggleCrosshairSync;
        assert_eq!(m.check(c, &k("ctrl+shift+d")), Check::Conflict(dom));
        assert!(m.set(c, None, k("ctrl+shift+d"), false).is_err());
        assert_eq!(m.lookup(true, true, false, "d"), Some(dom), "unchanged");
        m.set(c, None, k("ctrl+shift+d"), true).unwrap();
        assert_eq!(m.lookup(true, true, false, "d"), Some(c));
        assert_eq!(m.keys(dom).count(), 0, "moved, never twice");
        assert!(m.changed(c) && m.changed(dom));
        // Saved: both changes, and they load back to the same table.
        let saved = m.saved();
        assert_eq!(saved.get(&id(dom)), Some(&Vec::<String>::new()));
        assert_eq!(
            saved.get(&id(c)),
            Some(&vec!["ctrl+shift+x".to_owned(), "ctrl+shift+d".to_owned()])
        );
        let (back, skipped) = Keymap::with(&saved);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(back.lookup(true, true, false, "d"), Some(c));
        assert_eq!(back.keys(dom).count(), 0);
    }

    /// Flatten, Cancel all and Stop all keep a key: their last one can't be removed or taken.
    #[test]
    fn the_emergency_commands_always_keep_a_key() {
        let mut m = Keymap::defaults();
        for c in [
            Command::Trade(TradeCmd::Flatten),
            Command::Trade(TradeCmd::CancelAll),
            Command::Strategy(StrategyCmd::StopAll),
        ] {
            let last = m.keys(c).next().unwrap().clone();
            assert!(m.remove(c, &last).is_err(), "{c:?}");
            assert!(
                matches!(
                    m.check(Command::Trade(TradeCmd::ToggleArm), &last),
                    Check::Refused(_)
                ),
                "{c:?}: its only key can't be taken"
            );
            // A second key first: then either can go.
            m.set(c, None, k("ctrl+shift+f12"), false).unwrap();
            m.remove(c, &last).unwrap();
            assert_eq!(m.keys(c).count(), 1);
            m.reset(c);
        }
        // Saved without one (by hand): the default comes back, reported.
        let mut s = Saved::new();
        s.insert(id(Command::Trade(TradeCmd::Flatten)), Vec::new());
        let (m, skipped) = Keymap::with(&s);
        assert!(m.keys(Command::Trade(TradeCmd::Flatten)).next().is_some());
        assert_eq!(skipped.len(), 1, "{skipped:?}");
    }

    /// ↺ never leaves Flatten without a key (its default held by Cancel all as its only one):
    /// it keeps the keys it had.
    #[test]
    fn reset_never_leaves_an_emergency_command_keyless() {
        let mut m = Keymap::defaults();
        let flat = Command::Trade(TradeCmd::Flatten);
        let cancel = Command::Trade(TradeCmd::CancelAll);
        let f = m.keys(flat).next().unwrap().clone();
        let a = m.keys(cancel).next().unwrap().clone();
        m.set(flat, None, k("ctrl+shift+f12"), false).unwrap();
        m.set(cancel, None, f.clone(), true).unwrap();
        m.remove(cancel, &a).unwrap();
        m.reset(flat);
        assert_eq!(m.keys(flat).count(), 1, "kept its key");
        assert_eq!(m.keys(cancel).cloned().collect::<Vec<_>>(), vec![f]);
        // Such a file loads with every one of the three keyed.
        let mut s = m.saved();
        s.insert(id(flat), Vec::new());
        let (back, skipped) = Keymap::with(&s);
        for c in [flat, cancel, Command::Strategy(StrategyCmd::StopAll)] {
            assert!(back.keys(c).next().is_some(), "{c:?} {skipped:?}");
        }
        assert!(!skipped.is_empty());
    }

    /// Loading: an emergency command's default taken by another command comes back from it,
    /// and the report says so.
    #[test]
    fn loading_gives_an_emergency_command_its_key_back() {
        let flat = Command::Trade(TradeCmd::Flatten);
        let mut s = Saved::new();
        s.insert(id(flat), Vec::new());
        s.insert(id(Command::ToggleMagnet), vec!["ctrl+shift+f".into()]);
        let (m, skipped) = Keymap::with(&s);
        assert_eq!(m.lookup(true, true, false, "f"), Some(flat), "{skipped:?}");
        assert!(
            skipped.iter().any(|x| x.contains("moved back")),
            "{skipped:?}"
        );
    }

    /// Many random edits: no order key without Ctrl+Shift or Ctrl+Alt, every emergency
    /// command keyed, no key on two commands, and the saved form loads back the same.
    #[test]
    fn random_edits_keep_the_rules() {
        let cmds = bindable();
        let keys: Vec<Combo> = [
            "ctrl+shift+f",
            "ctrl+shift+a",
            "ctrl+shift+f12",
            "ctrl+alt+f11",
            "ctrl+f9",
            "f2",
            "ctrl+shift+d",
            "ctrl+alt+s",
            "alt+f",
            "space",
            "ctrl+shift+space",
            "ctrl+j",
        ]
        .iter()
        .map(|x| k(x))
        .collect();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut m = Keymap::defaults();
        for _ in 0..20_000 {
            let c = cmds[next(cmds.len())];
            let key = keys[next(keys.len())].clone();
            let old = m.keys(c).nth(next(3)).cloned();
            match next(4) {
                0 => {
                    let _ = m.set(c, old.as_ref(), key, next(2) == 0);
                }
                1 => {
                    if let Some(o) = old {
                        let _ = m.remove(c, &o);
                    }
                }
                2 => m.reset(c),
                _ => {
                    let _ = m.set(c, None, key, true);
                }
            }
            for (b, x) in &m.binds {
                assert!(!trades(*b) || x.ctrl && (x.shift || x.alt), "{b:?} {x:?}");
                assert_eq!(
                    m.lookup(x.ctrl, x.shift, x.alt, &x.key),
                    Some(*b),
                    "{x:?} twice"
                );
            }
            for e in cmds.iter().filter(|c| keeps_a_key(**c)) {
                assert!(m.keys(*e).next().is_some(), "{e:?} keyless");
            }
            assert_eq!(
                m.binds
                    .iter()
                    .filter(|(_, x)| m.binds.iter().filter(|(_, y)| y == x).count() > 1)
                    .count(),
                0
            );
        }
        let (back, skipped) = Keymap::with(&m.saved());
        assert!(skipped.is_empty(), "{skipped:?}");
        for c in &cmds {
            let mut a: Vec<_> = m.keys(*c).collect();
            let mut b: Vec<_> = back.keys(*c).collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{c:?}");
        }
    }

    /// The replay's plain keys stay: never replaced, removed or taken.
    #[test]
    fn the_replays_plain_keys_are_fixed() {
        let mut m = Keymap::defaults();
        let play = Command::Replay(ReplayCmd::PlayPause);
        let space = k("space");
        assert!(fixed(play, &space));
        assert!(m.remove(play, &space).is_err());
        assert!(m.set(play, Some(&space), k("ctrl+shift+p"), false).is_err());
        // Another key besides it is fine.
        m.set(play, None, k("ctrl+alt+f11"), false).unwrap();
        assert_eq!(m.keys(play).count(), 2);
        // Saved with its fixed key, it loads back without a report.
        let (back, skipped) = Keymap::with(&m.saved());
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(back, m);
    }

    /// A file from an older or newer version: unknown commands and unreadable, refused or
    /// taken keys are skipped and reported, never fatal; the rest applies.
    #[test]
    fn loading_skips_and_reports_what_it_cant_use() {
        let mut s = Saved::new();
        s.insert("NoSuchCommand".into(), vec!["ctrl+shift+q".into()]);
        s.insert(
            id(Command::NewTab),
            vec!["ctrl+bogus+t".into(), "ctrl+alt+f11".into()],
        );
        s.insert(id(Command::Draw(DrawCmd::Undo)), vec!["q".into()]);
        s.insert(id(Command::ToggleMagnet), vec!["ctrl+shift+d".into()]);
        let (m, skipped) = Keymap::with(&s);
        assert_eq!(skipped.len(), 4, "{skipped:?}");
        assert!(skipped[0].contains("NoSuchCommand"));
        assert_eq!(m.lookup(true, false, true, "f11"), Some(Command::NewTab));
        assert_eq!(
            m.lookup(true, true, false, "d"),
            Some(Command::Toggle(crate::layout::PaneKind::Dom)),
            "a taken key stays where it was"
        );
    }

    #[test]
    fn keys_read_and_print_back() {
        for s in [
            "ctrl+shift+f",
            "f1",
            "shift+f10",
            "ctrl++",
            "ctrl+alt+up",
            "ctrl+,",
        ] {
            assert_eq!(k(s).saved(), s);
        }
        assert_eq!(k("Ctrl+Shift+F").saved(), "ctrl+shift+f");
        assert_eq!(k("ctrl+shift+f").text(false), "Ctrl+Shift+F");
        assert_eq!(k("ctrl+shift+f").text(true), "⇧⌘F");
        assert!(Combo::parse("").is_none());
        assert!(Combo::parse("hyper+x").is_none());
    }

    /// ↺ puts a command's defaults back, taking a default key back from the command that
    /// took it.
    #[test]
    fn reset_brings_the_defaults_back() {
        let mut m = Keymap::defaults();
        let dom = Command::Toggle(crate::layout::PaneKind::Dom);
        m.set(Command::ToggleMagnet, None, k("ctrl+shift+d"), true)
            .unwrap();
        m.reset(dom);
        assert_eq!(m.lookup(true, true, false, "d"), Some(dom));
        assert!(
            m.keys(Command::ToggleMagnet)
                .all(|x| *x != k("ctrl+shift+d"))
        );
        m.reset(Command::ToggleMagnet);
        assert_eq!(m, Keymap::defaults());
    }
}
