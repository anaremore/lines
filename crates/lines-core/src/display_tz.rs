//! The display time zone: the zone every time shown to the trader is written in (the chart's
//! axis and crosshair, time & sales, the journal, strategy marks, ...).
//!
//! Display only. Storage, recordings, logs and session logic stay UTC, and sessions, the
//! 18:00 ET VWAP reset and expiries use their exchange's zone ([`crate::sessions`]) whatever
//! this is. Zones come from jiff's bundled tzdb (as sessions do); only [`ZoneChoice::System`]
//! asks the machine, and only to know which zone the trader picked.
//!
//! Writing never allocates: an offset lookup and an abbreviation are reads of the zone's
//! transitions. The app keeps one current zone ([`current`], [`set_current`]); functions that
//! write times take a `&DisplayTz`, so tests pass their own and never race on the global.

use std::fmt;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::time::UtcMillis;

/// The default display zone, US Eastern ("Exchange (ET)", the owner's choice): the 18:00 ET
/// VWAP reset and the equity sessions are on it (CME's Globex hours are kept in Central time
/// by `crate::sessions`, whatever this is).
pub const EXCHANGE: &str = "America/New_York";

/// What the trader chose (`settings.json`'s `time_zone`; absent is the exchange's).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ZoneChoice {
    /// US Eastern, labeled "ET" (the default).
    #[default]
    Exchange,
    /// The operating system's zone.
    System,
    /// An IANA zone ("Europe/London", "UTC").
    Named(String),
}

/// The saved word for [`ZoneChoice::System`] (no IANA name is spelled this way).
pub const SYSTEM: &str = "System";

impl ZoneChoice {
    /// From the saved text: absent or empty is the exchange's.
    pub fn from_saved(s: Option<&str>) -> ZoneChoice {
        match s.map(str::trim) {
            None | Some("") => ZoneChoice::Exchange,
            Some(SYSTEM) => ZoneChoice::System,
            Some(n) => ZoneChoice::Named(n.to_owned()),
        }
    }

    /// The saved text (`None`: the default, not written).
    pub fn to_saved(&self) -> Option<String> {
        match self {
            ZoneChoice::Exchange => None,
            ZoneChoice::System => Some(SYSTEM.to_owned()),
            ZoneChoice::Named(n) => Some(n.clone()),
        }
    }

    /// How a list or a setting names it: "Exchange (ET)", "System (Europe/Berlin)",
    /// "Asia/Tokyo".
    pub fn describe(&self) -> String {
        match self {
            ZoneChoice::Exchange => "Exchange (ET)".to_owned(),
            ZoneChoice::System => match system_name() {
                Some(n) => format!("System ({n})"),
                None => "System".to_owned(),
            },
            ZoneChoice::Named(n) => n.clone(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Label {
    /// The same word all year ("ET", "UTC").
    Fixed(&'static str),
    /// The zone's own abbreviation at that instant ("CET", "CEST", "JST", "+03").
    Abbrev,
}

/// A zone to write times in.
#[derive(Clone, Debug)]
pub struct DisplayTz {
    tz: TimeZone,
    label: Label,
}

impl PartialEq for DisplayTz {
    fn eq(&self, o: &Self) -> bool {
        self.label == o.label && self.tz.iana_name() == o.tz.iana_name()
    }
}

impl Default for DisplayTz {
    fn default() -> Self {
        DisplayTz::utc()
    }
}

impl DisplayTz {
    pub fn utc() -> DisplayTz {
        DisplayTz {
            tz: TimeZone::UTC,
            label: Label::Fixed("UTC"),
        }
    }

    /// US Eastern, labeled "ET" (EST and EDT alike: the label says whose clock, the offset
    /// follows DST).
    pub fn exchange() -> DisplayTz {
        DisplayTz {
            tz: TimeZone::get(EXCHANGE).unwrap_or(TimeZone::UTC),
            label: Label::Fixed("ET"),
        }
    }

    /// The zone for a choice. An IANA name this tzdb doesn't know, or a system zone that
    /// can't be read, is an error (the caller falls back to the exchange's and says so).
    pub fn resolve(c: &ZoneChoice) -> Result<DisplayTz, String> {
        match c {
            ZoneChoice::Exchange => Ok(DisplayTz::exchange()),
            ZoneChoice::System => TimeZone::try_system()
                .map(DisplayTz::of)
                .map_err(|e| format!("the system time zone can't be read ({e})")),
            ZoneChoice::Named(n) if is_utc(n) => Ok(DisplayTz::utc()),
            ZoneChoice::Named(n) if n == EXCHANGE => Ok(DisplayTz::exchange()),
            ZoneChoice::Named(n) => TimeZone::get(n)
                .map(DisplayTz::of)
                .map_err(|_| format!("unknown time zone {n:?}")),
        }
    }

    fn of(tz: TimeZone) -> DisplayTz {
        if tz.iana_name().is_some_and(is_utc) {
            return DisplayTz::utc();
        }
        // An OS on New York's clock reads "ET", as the exchange's choice does.
        if tz.iana_name() == Some(EXCHANGE) {
            return DisplayTz::exchange();
        }
        DisplayTz {
            tz,
            label: Label::Abbrev,
        }
    }

    /// The zone's IANA name, if it has one.
    pub fn iana_name(&self) -> Option<&str> {
        self.tz.iana_name()
    }

    /// The offset from UTC at `t`, in milliseconds (west of Greenwich is negative).
    pub fn offset_ms(&self, t: UtcMillis) -> i64 {
        match Timestamp::from_millisecond(t.0) {
            Ok(ts) => i64::from(self.tz.to_offset(ts).seconds()) * 1000,
            Err(_) => 0,
        }
    }

    /// The wall clock at `t`, as if it were a UTC instant: day and hour arithmetic on it
    /// (`div_euclid(DAY)`, [`UtcMillis::to_civil`]) gives this zone's days and hours.
    pub fn local(&self, t: UtcMillis) -> UtcMillis {
        UtcMillis(t.0.saturating_add(self.offset_ms(t)))
    }

    /// (year, month, day, hour, minute, second) on this zone's wall clock.
    pub fn civil(&self, t: UtcMillis) -> (i64, u32, u32, u32, u32, u32) {
        self.local(t).to_civil()
    }

    /// The zone's label at `t`: "ET", "UTC", or its abbreviation then ("CEST", "+03").
    pub fn write_label<W: fmt::Write>(&self, t: UtcMillis, out: &mut W) -> fmt::Result {
        match self.label {
            Label::Fixed(s) => out.write_str(s),
            Label::Abbrev => match Timestamp::from_millisecond(t.0) {
                Ok(ts) => out.write_str(self.tz.to_offset_info(ts).abbreviation()),
                Err(_) => out.write_str("UTC"),
            },
        }
    }

    /// `HH:MM:SS` on this zone's clock.
    pub fn write_clock<W: fmt::Write>(&self, t: UtcMillis, out: &mut W) -> fmt::Result {
        let (_, _, _, h, m, s) = self.civil(t);
        write!(out, "{h:02}:{m:02}:{s:02}")
    }

    /// `HH:MM` on this zone's clock.
    pub fn write_hhmm<W: fmt::Write>(&self, t: UtcMillis, out: &mut W) -> fmt::Result {
        let (_, _, _, h, m, _) = self.civil(t);
        write!(out, "{h:02}:{m:02}")
    }
}

/// The OS zone's IANA name, read once (the Settings window asks every frame).
fn system_name() -> Option<&'static str> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        TimeZone::try_system()
            .ok()
            .and_then(|z| z.iana_name().map(str::to_owned))
    })
    .as_deref()
}

fn is_utc(n: &str) -> bool {
    matches!(
        n,
        "UTC" | "Etc/UTC" | "Etc/UCT" | "UCT" | "Etc/Universal" | "Universal" | "Zulu" | "Etc/Zulu"
    )
}

/// Every zone a trader can pick, sorted: "UTC", then the tzdb's `Area/City` names (no
/// legacy aliases like "US/Eastern" or "EST5EDT"). Built once.
pub fn zone_names() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        const AREAS: [&str; 10] = [
            "Africa/",
            "America/",
            "Antarctica/",
            "Arctic/",
            "Asia/",
            "Atlantic/",
            "Australia/",
            "Europe/",
            "Indian/",
            "Pacific/",
        ];
        let mut v: Vec<String> = jiff::tz::db()
            .available()
            .map(|n| n.as_str().to_owned())
            .filter(|n| AREAS.iter().any(|a| n.starts_with(a)))
            .collect();
        v.sort();
        v.dedup();
        v.insert(0, "UTC".to_owned());
        v
    })
}

// ---- the app's current zone ---------------------------------------------------------------

/// `None` until the app sets one: UTC (tests and tools that never set it write UTC).
static CURRENT: RwLock<Option<DisplayTz>> = RwLock::new(None);
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// The zone times are shown in now (a reference-count bump, never an allocation).
pub fn current() -> DisplayTz {
    match CURRENT.read() {
        Ok(g) => g.clone().unwrap_or_else(DisplayTz::utc),
        Err(p) => p.into_inner().clone().unwrap_or_else(DisplayTz::utc),
    }
}

/// Show times in `z` from now on; [`epoch`] moves so cached text is rebuilt.
pub fn set_current(z: DisplayTz) {
    match CURRENT.write() {
        Ok(mut g) => *g = Some(z),
        Err(p) => *p.into_inner() = Some(z),
    }
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Bumped whenever the current zone changes (lock-free: panes key cached text on it).
pub fn epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-08 (US DST starts 02:00 EST) and 2026-11-01 (ends 02:00 EDT).
    fn at(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    fn clock(z: &DisplayTz, t: UtcMillis) -> String {
        let mut s = String::new();
        z.write_clock(t, &mut s).unwrap();
        s.push(' ');
        z.write_label(t, &mut s).unwrap();
        s
    }

    #[test]
    fn exchange_follows_both_us_transitions() {
        let et = DisplayTz::exchange();
        // Spring forward: 06:59:59Z is 01:59:59 EST; 07:00Z is 03:00 EDT.
        assert_eq!(clock(&et, at("2026-03-08T06:59:59Z")), "01:59:59 ET");
        assert_eq!(clock(&et, at("2026-03-08T07:00:00Z")), "03:00:00 ET");
        // Fall back: 05:59:59Z is 01:59:59 EDT; 06:00Z is 01:00 EST (01:xx shows twice).
        assert_eq!(clock(&et, at("2026-11-01T05:59:59Z")), "01:59:59 ET");
        assert_eq!(clock(&et, at("2026-11-01T06:00:00Z")), "01:00:00 ET");
        assert_eq!(et.offset_ms(at("2026-11-01T05:59:59Z")), -4 * 3_600_000);
        assert_eq!(et.offset_ms(at("2026-11-01T06:00:00Z")), -5 * 3_600_000);
    }

    #[test]
    fn a_named_zone_shows_its_own_abbreviation_through_its_transitions() {
        let z = DisplayTz::resolve(&ZoneChoice::Named("Europe/Berlin".into())).unwrap();
        // EU: CET to CEST at 01:00Z on 2026-03-29, back at 01:00Z on 2026-10-25.
        assert_eq!(clock(&z, at("2026-03-29T00:59:59Z")), "01:59:59 CET");
        assert_eq!(clock(&z, at("2026-03-29T01:00:00Z")), "03:00:00 CEST");
        assert_eq!(clock(&z, at("2026-10-25T00:59:59Z")), "02:59:59 CEST");
        assert_eq!(clock(&z, at("2026-10-25T01:00:00Z")), "02:00:00 CET");
    }

    #[test]
    fn utc_by_any_name_is_labeled_utc() {
        for n in ["UTC", "Etc/UTC"] {
            let z = DisplayTz::resolve(&ZoneChoice::Named(n.into())).unwrap();
            assert_eq!(clock(&z, at("2026-11-05T21:59:00Z")), "21:59:00 UTC");
        }
        assert_eq!(
            clock(&DisplayTz::utc(), at("2026-11-05T21:59:00Z")),
            "21:59:00 UTC"
        );
    }

    #[test]
    fn an_unknown_zone_is_refused() {
        assert!(DisplayTz::resolve(&ZoneChoice::Named("Mars/Olympus".into())).is_err());
    }

    #[test]
    fn local_days_split_at_the_zones_midnight() {
        let et = DisplayTz::exchange();
        // 03:30Z on Nov 6 is 22:30 ET on Nov 5.
        let (_, mo, d, h, mi, _) = et.civil(at("2026-11-06T03:30:00Z"));
        assert_eq!((mo, d, h, mi), (11, 5, 22, 30));
    }

    #[test]
    fn the_saved_text_round_trips() {
        for c in [
            ZoneChoice::Exchange,
            ZoneChoice::System,
            ZoneChoice::Named("Asia/Tokyo".into()),
        ] {
            assert_eq!(ZoneChoice::from_saved(c.to_saved().as_deref()), c);
        }
        assert_eq!(ZoneChoice::from_saved(Some("")), ZoneChoice::Exchange);
    }

    #[test]
    fn the_zone_list_has_cities_and_utc_but_no_legacy_aliases() {
        let z = zone_names();
        assert_eq!(z[0], "UTC");
        for n in [
            "America/New_York",
            "America/Chicago",
            "Europe/London",
            "Asia/Tokyo",
        ] {
            assert!(z.iter().any(|x| x == n), "{n}");
        }
        assert!(!z.iter().any(|x| x == "US/Eastern" || x == "EST5EDT"));
        assert!(z.len() > 300, "{}", z.len());
        assert!(z[1..].windows(2).all(|w| w[0] < w[1]));
    }
}
