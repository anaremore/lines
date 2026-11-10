//! The fonts (decision 0034): IBM Plex Sans for UI text and IBM Plex Mono for numbers, embedded
//! (`lines_fonts`) and loaded before the first window. The UI family is a setting (Settings ›
//! Appearance › UI font, `--ui-font`): the built-in Plex Sans (the default), the OS's UI font
//! ("System", GPUI's `.SystemUIFont`) or any installed family. Numbers are always Plex Mono.
//!
//! Every window's root sets [`ui_family`] (with the UI zoom's rem size), so element text
//! follows it; text shaped by hand takes [`ui`] (words) or [`mono`] (numbers). A change bumps the UI
//! zoom's generation (`crate::zoom::text_changed`), so text shaped in the old family is shaped
//! again. The family is one lock read per window per frame (render, never paint), no
//! allocation.

use std::borrow::Cow;
use std::sync::{OnceLock, RwLock};

use gpui::{App, Font, SharedString};

pub use lines_fonts::{MONO, SANS};

/// The setting's key in the Settings table.
pub const KEY: &str = "appearance.ui_font";

/// What the setting shows for the built-in font.
pub const BUILT_IN: &str = "IBM Plex Sans (built in)";
/// What the setting shows (and saves) for the OS's UI font.
pub const SYSTEM: &str = "System";
/// The sample each row of the font list shows in its font.
pub const SAMPLE: &str = "Aa Bb 0123";
/// GPUI's name for the OS's UI font (Segoe UI on Windows, SF on macOS).
const SYSTEM_FAMILY: &str = ".SystemUIFont";

/// The UI font setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UiFont {
    /// IBM Plex Sans, embedded.
    #[default]
    BuiltIn,
    /// The OS's UI font.
    System,
    /// An installed family, by name.
    Named(String),
}

impl UiFont {
    /// From `settings.json`'s `ui_font`: absent is the built-in font, "System" the OS's, else
    /// a family name.
    pub fn from_saved(saved: Option<&str>) -> UiFont {
        match saved.map(str::trim) {
            None | Some("") => UiFont::BuiltIn,
            Some(s) if s == SANS || s == BUILT_IN => UiFont::BuiltIn,
            Some(SYSTEM) => UiFont::System,
            Some(s) => UiFont::Named(s.to_owned()),
        }
    }

    /// The saved form: absent for the built-in font.
    pub fn to_saved(&self) -> Option<String> {
        match self {
            UiFont::BuiltIn => None,
            UiFont::System => Some(SYSTEM.to_owned()),
            UiFont::Named(n) => Some(n.clone()),
        }
    }

    /// The family it names (the list draws its row's sample in it); [`apply`] checks it is
    /// installed.
    pub fn family(&self) -> &str {
        match self {
            UiFont::BuiltIn => SANS,
            UiFont::System => SYSTEM_FAMILY,
            UiFont::Named(n) => n,
        }
    }

    /// The words the setting and its list show.
    pub fn describe(&self) -> &str {
        match self {
            UiFont::BuiltIn => BUILT_IN,
            UiFont::System => SYSTEM,
            UiFont::Named(n) => n,
        }
    }
}

/// The UI family in force: `None` is the built-in [`SANS`].
static FAMILY: RwLock<Option<SharedString>> = RwLock::new(None);

/// The installed families (from the text system at [`load`]), sorted, without the embedded
/// ones or the OS's hidden and vertical-writing names.
static INSTALLED: OnceLock<Vec<String>> = OnceLock::new();

/// Load the embedded fonts into GPUI's text system and note the installed families. Call once,
/// before the first window opens.
pub fn load(cx: &App) {
    let ts = cx.text_system();
    if let Err(e) = ts.add_fonts(lines_fonts::all().map(Cow::Borrowed).collect()) {
        // GPUI falls back to the OS fonts for an unknown family: text still shows.
        tracing::error!("the built-in fonts didn't load: {e:#}");
    }
    let _ = INSTALLED.set(installed_of(ts.all_font_names()));
}

/// The families a font list offers: sorted, deduplicated, without the embedded families
/// (the list has its own row for Plex Sans) and without names the OS keeps for itself
/// (`.`-prefixed on macOS, `@`-prefixed vertical-writing names on Windows).
fn installed_of(mut names: Vec<String>) -> Vec<String> {
    names.retain(|n| {
        !n.is_empty() && !n.starts_with('.') && !n.starts_with('@') && !lines_fonts::is_bundled(n)
    });
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

/// The installed families (empty before [`load`]).
pub fn installed() -> &'static [String] {
    INSTALLED.get().map_or(&[], Vec::as_slice)
}

/// `name` as the font list spells it, if it is installed (case ignored).
fn installed_name(name: &str) -> Option<&'static str> {
    installed()
        .iter()
        .find(|n| n.eq_ignore_ascii_case(name.trim()))
        .map(String::as_str)
}

/// Draw UI text in `choice`'s family from now on. A family that isn't installed shows the
/// built-in one, and the returned text says so.
pub fn apply(choice: &UiFont) -> Option<String> {
    let (family, problem) = resolve(choice, installed());
    let family = (family != SANS).then(|| SharedString::from(family.to_owned()));
    let mut now = FAMILY.write().unwrap_or_else(|e| e.into_inner());
    if *now != family {
        *now = family;
        crate::zoom::text_changed();
    }
    problem
}

/// The family `choice` draws in, given the installed families, and a problem to tell.
fn resolve<'a>(choice: &'a UiFont, installed: &'a [String]) -> (&'a str, Option<String>) {
    match choice {
        UiFont::BuiltIn => (SANS, None),
        UiFont::System => (SYSTEM_FAMILY, None),
        UiFont::Named(n) => match installed.iter().find(|i| i.eq_ignore_ascii_case(n.trim())) {
            Some(i) => (i.as_str(), None),
            None => (
                SANS,
                Some(format!("UI font {n} isn't installed: text shows in {SANS}")),
            ),
        },
    }
}

/// The UI family in force, for each window's root (`div().font_family(..)`).
pub fn ui_family() -> SharedString {
    FAMILY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or(SharedString::new_static(SANS))
}

/// The UI font in force, for text shaped by hand (painted labels, and widths measured in
/// `render` to decide what fits). Never `window.text_style()` for this: in `render` the
/// window's style stack is empty, so it is GPUI's default (the OS font), not the root's.
pub fn ui() -> Font {
    gpui::font(ui_family())
}

/// The number font: Plex Mono at the regular weight (every digit the same width).
pub fn mono() -> Font {
    gpui::font(MONO)
}

/// `--ui-font`: `plex` (or `builtin`), `system`, or an installed family's name. Checked
/// against the installed families, so call it after [`load`].
pub fn parse_flag(s: &str) -> Result<UiFont, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" => Err("expected plex, system or a font's name".to_owned()),
        "plex" | "builtin" | "built-in" => Ok(UiFont::BuiltIn),
        "system" => Ok(UiFont::System),
        _ => match UiFont::from_saved(Some(s)) {
            UiFont::Named(n) => installed_name(&n)
                .map(|n| UiFont::Named(n.to_owned()))
                .ok_or_else(|| format!("{n}: not an installed font")),
            c => Ok(c),
        },
    }
}

/// What the setting shows for the saved text (`settings.json`'s `ui_font`).
pub fn describe_saved(saved: Option<&str>) -> String {
    UiFont::from_saved(saved).describe().to_owned()
}

/// The saved text for what the setting shows (the inverse of [`describe_saved`]): the
/// built-in font, the OS's, or a family by its listed spelling when installed. A name that
/// isn't installed is kept as given (an undo back to a font since uninstalled; [`apply`]
/// shows Plex Sans and says so, and the font comes back if it is installed again).
pub fn saved_of(shown: &str) -> Result<Option<String>, &'static str> {
    match UiFont::from_saved(Some(shown)) {
        UiFont::Named(n) => Ok(Some(installed_name(&n).map_or(n.clone(), str::to_owned))),
        c => Ok(c.to_saved()),
    }
}

/// Every choice the list offers: the built-in font, the OS's, then each installed family.
pub fn options() -> &'static [UiFont] {
    static ALL: OnceLock<Vec<UiFont>> = OnceLock::new();
    // Before `load` (tests) the list has the first two only, and is not kept.
    if INSTALLED.get().is_none() {
        static FIRST: [UiFont; 2] = [UiFont::BuiltIn, UiFont::System];
        return &FIRST;
    }
    ALL.get_or_init(|| {
        [UiFont::BuiltIn, UiFont::System]
            .into_iter()
            .chain(installed().iter().map(|n| UiFont::Named(n.clone())))
            .collect()
    })
}

/// Indices into [`options`] matching `query` (`crate::search_list::matching`).
pub fn matching(query: &str) -> Vec<usize> {
    let hays: Vec<String> = options()
        .iter()
        .map(|c| match c {
            UiFont::BuiltIn => format!("{} default plex", BUILT_IN.to_lowercase()),
            UiFont::System => "system os default".to_owned(),
            UiFont::Named(n) => n.to_lowercase(),
        })
        .collect();
    crate::search_list::matching(query, hays.iter().map(String::as_str))
}

/// The font list, opened on the font in force (`current`), selected.
pub fn pick(current: &UiFont) -> crate::search_list::Pick {
    crate::search_list::Pick::new(matching, options().iter().position(|c| c == current))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_saved_text_round_trips() {
        assert_eq!(UiFont::from_saved(None), UiFont::BuiltIn);
        assert_eq!(UiFont::from_saved(Some("")), UiFont::BuiltIn);
        assert_eq!(UiFont::from_saved(Some(SANS)), UiFont::BuiltIn);
        assert_eq!(UiFont::from_saved(Some(BUILT_IN)), UiFont::BuiltIn);
        assert_eq!(UiFont::from_saved(Some("System")), UiFont::System);
        assert_eq!(
            UiFont::from_saved(Some("Calibri")),
            UiFont::Named("Calibri".into())
        );
        for c in [
            UiFont::BuiltIn,
            UiFont::System,
            UiFont::Named("Calibri".into()),
        ] {
            assert_eq!(UiFont::from_saved(c.to_saved().as_deref()), c);
            assert_eq!(UiFont::from_saved(Some(c.describe())), c);
        }
        assert_eq!(
            UiFont::BuiltIn.to_saved(),
            None,
            "the default isn't written"
        );
        // The built-in and system rows map back without a font list.
        assert_eq!(saved_of(BUILT_IN), Ok(None));
        assert_eq!(saved_of(SYSTEM), Ok(Some("System".to_owned())));
        // A font no longer installed is kept (undo can go back to it; it shows Plex Sans).
        assert_eq!(saved_of("Comic Neue"), Ok(Some("Comic Neue".to_owned())));
    }

    /// A missing family falls back to Plex Sans and says so; an installed one is used by its
    /// listed spelling.
    #[test]
    fn a_missing_family_falls_back_to_plex_sans() {
        let installed = names(&["Arial", "Calibri"]);
        assert_eq!(resolve(&UiFont::BuiltIn, &installed), (SANS, None));
        assert_eq!(
            resolve(&UiFont::System, &installed),
            (".SystemUIFont", None)
        );
        assert_eq!(
            resolve(&UiFont::Named("calibri".into()), &installed),
            ("Calibri", None)
        );
        let gone = UiFont::Named("Comic Neue".into());
        let (family, problem) = resolve(&gone, &installed);
        assert_eq!(family, SANS);
        assert_eq!(
            problem.as_deref(),
            Some("UI font Comic Neue isn't installed: text shows in IBM Plex Sans")
        );
    }

    /// Applying another family reshapes text once (the UI zoom's generation, which every
    /// cache of shaped text watches); the same family again changes nothing. The only test
    /// that applies a font (the family is app-wide), and it puts the default back.
    #[test]
    fn applying_another_family_reshapes_text_once() {
        let before = crate::zoom::generation();
        assert_eq!(apply(&UiFont::System), None);
        assert_eq!(ui_family().as_ref(), ".SystemUIFont");
        assert_eq!(ui().family.as_ref(), ".SystemUIFont");
        let changed = crate::zoom::generation();
        assert_ne!(changed, before, "a new family bumps the generation");
        assert_eq!(apply(&UiFont::System), None);
        assert_eq!(
            crate::zoom::generation(),
            changed,
            "the same family again doesn't"
        );
        // Not installed (no font list in unit tests): Plex Sans, and it says so.
        assert!(apply(&UiFont::Named("Not A Font".into())).is_some());
        assert_eq!(ui_family().as_ref(), SANS);
        assert_ne!(crate::zoom::generation(), changed);
        assert_eq!(mono().family.as_ref(), MONO, "numbers never follow it");
        assert_eq!(apply(&UiFont::BuiltIn), None);
    }

    #[test]
    fn the_list_leaves_out_the_embedded_and_hidden_names() {
        let listed = installed_of(names(&[
            "Segoe UI",
            "IBM Plex Sans",
            "IBM Plex Mono SmBld",
            ".AppleSystemUIFont",
            "@MS Gothic",
            "arial",
            "Calibri",
            "Calibri",
        ]));
        assert_eq!(listed, names(&["arial", "Calibri", "Segoe UI"]));
    }

    #[test]
    fn the_list_starts_with_the_built_in_and_system_fonts() {
        // Before `load` (as in unit tests) only these two.
        assert_eq!(options()[0], UiFont::BuiltIn);
        assert_eq!(options()[1], UiFont::System);
        assert_eq!(matching("plex"), vec![0]);
        assert_eq!(matching("system"), vec![1]);
        assert_eq!(pick(&UiFont::System).selected(), Some(1));
    }
}
