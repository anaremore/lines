//! The display time zone setting (Settings › General › Time zone, `--time-zone`): applying
//! it, and the searchable zone list the Settings window shows. GPUI-free.
//!
//! The zone is display only (`lines_core::display_tz`): sessions, the VWAP reset, expiries,
//! storage, recordings and logs never read it.

use lines_core::display_tz::{self, DisplayTz, ZoneChoice};

use crate::search_list;

/// The setting's key in the Settings table.
pub const KEY: &str = "appearance.time_zone";

/// Show times in `c` from now on. A zone that can't be used shows the exchange's instead,
/// and the returned text says so.
pub fn apply(c: &ZoneChoice) -> Option<String> {
    let (zone, problem) = match DisplayTz::resolve(c) {
        Ok(z) => (z, None),
        Err(e) => (
            DisplayTz::exchange(),
            Some(format!("{e}: times show in Exchange (ET)")),
        ),
    };
    display_tz::set_current(zone);
    // Panes that cache time text (the journal, the strategies pane) rebuild on the display
    // epoch.
    crate::pnl::display_changed();
    problem
}

/// `--time-zone`: `exchange` (or `et`), `system`, `utc`, or an IANA name.
pub fn parse_flag(s: &str) -> Result<ZoneChoice, String> {
    let c = match s.trim().to_ascii_lowercase().as_str() {
        "exchange" | "et" => ZoneChoice::Exchange,
        "system" => ZoneChoice::System,
        "utc" => ZoneChoice::Named("UTC".to_owned()),
        _ => ZoneChoice::Named(s.trim().to_owned()),
    };
    DisplayTz::resolve(&c).map(|_| c)
}

/// What the setting shows for the saved text (`settings.json`'s `time_zone`).
pub fn describe_saved(saved: Option<&str>) -> String {
    ZoneChoice::from_saved(saved).describe()
}

/// The saved text for what the setting shows (the inverse of [`describe_saved`]): a zone
/// this tzdb knows, by its IANA spelling. An unknown zone is refused.
pub fn saved_of(shown: &str) -> Result<Option<String>, &'static str> {
    let c = if shown == ZoneChoice::Exchange.describe() {
        ZoneChoice::Exchange
    } else if shown == "System" || shown.starts_with("System (") {
        ZoneChoice::System
    } else {
        let z = DisplayTz::resolve(&ZoneChoice::Named(shown.trim().to_owned()))
            .map_err(|_| "unknown time zone")?;
        ZoneChoice::Named(z.iana_name().unwrap_or("UTC").to_owned())
    };
    Ok(c.to_saved())
}

/// Every choice the list offers: the exchange's, the system's, then UTC and every city.
pub fn options() -> &'static [ZoneChoice] {
    static ALL: std::sync::OnceLock<Vec<ZoneChoice>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        [ZoneChoice::Exchange, ZoneChoice::System]
            .into_iter()
            .chain(
                display_tz::zone_names()
                    .iter()
                    .map(|n| ZoneChoice::Named(n.clone())),
            )
            .collect()
    })
}

/// A choice's words for the list ("Exchange (ET)", "System (Europe/Berlin)", "America/New
/// York"): an IANA name's underscores read as spaces.
pub fn option_text(c: &ZoneChoice) -> String {
    c.describe().replace('_', " ")
}

/// Indices into [`options`] matching `query` (`crate::search_list::matching`): every word in
/// the choice's words (case and `_`/space ignored), word starts first ("eu" is Europe/Paris
/// before Africa/Ceuta). Empty: all.
pub fn matching(query: &str) -> Vec<usize> {
    static HAYS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let hays = HAYS.get_or_init(|| {
        options()
            .iter()
            .map(|c| match c {
                ZoneChoice::Exchange => "exchange (et) eastern new york".to_owned(),
                _ => option_text(c).to_lowercase(),
            })
            .collect()
    });
    search_list::matching(query, hays.iter().map(String::as_str))
}

/// The zone list, opened on the zone in force (`current`), selected.
pub fn pick(current: &ZoneChoice) -> search_list::Pick {
    search_list::Pick::new(matching, options().iter().position(|c| c == current))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_starts_with_exchange_and_system_and_finds_a_city() {
        assert_eq!(options()[0], ZoneChoice::Exchange);
        assert_eq!(options()[1], ZoneChoice::System);
        let hits = matching("new york");
        assert!(hits.contains(&0), "the exchange's is New York's");
        assert!(
            hits.iter()
                .any(|&i| options()[i] == ZoneChoice::Named("America/New_York".into()))
        );
        // ("System (Asia/Tokyo)" matches too on a machine in Tokyo.)
        let tokyo: Vec<usize> = matching("TOKYO")
            .into_iter()
            .filter(|&i| options()[i] != ZoneChoice::System)
            .collect();
        assert_eq!(tokyo.len(), 1);
        assert_eq!(options()[tokyo[0]], ZoneChoice::Named("Asia/Tokyo".into()));
        assert!(
            matching("utc")
                .iter()
                .any(|&i| options()[i] == ZoneChoice::Named("UTC".into()))
        );
        assert!(matching("no such place").is_empty());
        // A word's start ranks first: "eu" is Europe before Africa/Ceuta.
        let eu = matching("eu");
        let at = |i: usize| eu.iter().position(|&k| k == i).unwrap();
        let europe = eu
            .iter()
            .position(|&k| option_text(&options()[k]).starts_with("Europe/"))
            .unwrap();
        assert!(europe < at(matching("ceuta")[0]), "{eu:?}");
    }

    /// The chosen index's zone.
    fn chosen(k: search_list::PickKey) -> Option<ZoneChoice> {
        match k {
            search_list::PickKey::Choose(i) => Some(options()[i].clone()),
            _ => None,
        }
    }

    #[test]
    fn typing_filters_and_enter_chooses() {
        let mut p = pick(&ZoneChoice::Exchange);
        assert_eq!(p.selected(), Some(0));
        p.type_text("chicago");
        // "System (America/Chicago)" also matches on a machine in Chicago.
        if p.selected() == Some(1) {
            p.key("down", None);
        }
        assert_eq!(
            chosen(p.key("enter", None)),
            Some(ZoneChoice::Named("America/Chicago".into()))
        );
        p.key("backspace", None);
        assert_eq!(p.query, "chicag");
        assert_eq!(p.key("escape", None), search_list::PickKey::Cancel);
        // Nothing matches: Enter does nothing.
        let mut p = pick(&ZoneChoice::Exchange);
        p.type_text("zzzz");
        assert_eq!(p.key("enter", None), search_list::PickKey::Ignored);
    }

    #[test]
    fn it_opens_on_the_zone_in_force_and_keeps_it_in_view() {
        use search_list::ROWS;
        let berlin = ZoneChoice::Named("Europe/Berlin".into());
        let p = pick(&berlin);
        assert_eq!(p.selected().map(|i| &options()[i]), Some(&berlin));
        assert!(p.top() <= p.sel && p.sel < p.top() + ROWS);
        let mut p = pick(&ZoneChoice::Exchange);
        p.key("up", None);
        assert_eq!(p.sel, 0);
        for _ in 0..3 {
            p.key("pagedown", None);
        }
        assert!(p.top() <= p.sel && p.sel < p.top() + ROWS);
    }

    #[test]
    fn the_shown_text_maps_back_to_the_saved_text() {
        for saved in [None, Some("System"), Some("Asia/Tokyo"), Some("UTC")] {
            assert_eq!(
                saved_of(&describe_saved(saved)),
                Ok(saved.map(str::to_owned))
            );
        }
        // Saved by its IANA spelling; an unknown zone is refused.
        assert_eq!(saved_of("asia/tokyo"), Ok(Some("Asia/Tokyo".to_owned())));
        assert!(saved_of("Mars/Olympus").is_err());
    }

    #[test]
    fn the_flag_takes_words_and_iana_names() {
        assert_eq!(parse_flag("exchange"), Ok(ZoneChoice::Exchange));
        assert_eq!(parse_flag("ET"), Ok(ZoneChoice::Exchange));
        assert_eq!(parse_flag("utc"), Ok(ZoneChoice::Named("UTC".into())));
        assert_eq!(
            parse_flag("Europe/London"),
            Ok(ZoneChoice::Named("Europe/London".into()))
        );
        assert!(parse_flag("Nowhere/Special").is_err());
    }
}
