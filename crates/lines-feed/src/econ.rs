//! The bundled economic calendar (roadmap 8b): official US release schedules (BLS, BEA, the
//! Federal Reserve) as versioned data, so the news blackout works offline, in replays and in
//! tests. Sources and the verification status of every line are in the file; the format and
//! the missing / stale rules are in [`lines_core::econ`].

use std::sync::{Arc, OnceLock};

use lines_core::econ::EconSchedule;

/// The shipped schedule's text.
pub const BUNDLED: &str = include_str!("../data/econ-calendar.txt");

/// The shipped schedule, parsed once. Validated by this module's tests (every symbol it names
/// is a catalog product); failing here is a build defect.
pub fn bundled() -> Arc<EconSchedule> {
    static SCHEDULE: OnceLock<Arc<EconSchedule>> = OnceLock::new();
    SCHEDULE
        .get_or_init(|| {
            Arc::new(EconSchedule::parse(BUNDLED).unwrap_or_else(|e| panic!("embedded {e}")))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use lines_core::UtcMillis;
    use lines_core::econ::{Coverage, Importance};

    use super::*;
    use crate::catalog;

    #[test]
    fn the_bundled_schedule_parses_and_names_only_catalog_products() {
        let s = bundled();
        assert_eq!(s.zone, "America/New_York");
        for r in &s.releases {
            for sym in &r.affects {
                assert!(catalog::by_symbol(sym).is_some(), "{}: {sym}", r.key);
            }
            // Official sources only.
            for src in &r.sources {
                assert!(
                    [
                        "https://www.bls.gov/",
                        "https://www.bea.gov/",
                        "https://www.federalreserve.gov/"
                    ]
                    .iter()
                    .any(|p| src.starts_with(p)),
                    "{src}"
                );
            }
        }
        let keys: Vec<_> = s.releases.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["cpi", "nfp", "ppi", "gdp", "pce", "fomc"]);
    }

    #[test]
    fn known_releases_land_on_their_eastern_times() {
        let s = bundled();
        let at = |iso: &str| UtcMillis::parse_iso(iso).unwrap();
        // CPI, Tuesday 2026-11-10 08:30 EST.
        let cpi = s
            .next_event(Some("ES"), at("2026-11-07T00:00:00Z"), Importance::High)
            .unwrap();
        assert_eq!(s.release(cpi).key, "cpi");
        assert_eq!(cpi.at, at("2026-11-10T13:30:00Z"));
        // FOMC, Wednesday 2027-06-09 14:00 EDT.
        let fomc = s
            .next_event(Some("NQ"), at("2027-06-05T00:00:00Z"), Importance::High)
            .unwrap();
        assert_eq!(s.release(fomc).key, "fomc");
        assert_eq!(fomc.at, at("2027-06-09T18:00:00Z"));
        // BEA has no 2027 list yet: missing, never assumed clear.
        assert!(matches!(
            s.coverage("ES", at("2027-01-04T15:00:00Z"), 0, 0, Importance::Medium),
            Coverage::Missing { .. }
        ));
        // ...which a high-only rule doesn't need: CPI, jobs and FOMC are covered.
        assert_eq!(
            s.coverage("ES", at("2027-01-04T15:00:00Z"), 0, 0, Importance::High),
            Coverage::Current
        );
        assert_eq!(
            s.coverage("ES", at("2026-11-09T15:00:00Z"), 0, 0, Importance::Low),
            Coverage::Current
        );
    }
}
