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

/// The calendar refresh's official source (roadmap 8b): FRED, the Federal Reserve Bank of St.
/// Louis's API, which publishes the agencies' own release calendars (BLS, BEA and the Board's
/// FOMC statements) as dates. No other site is read and nothing is scraped. A FRED API key
/// (free, the user's own) is needed; Lines keeps it in the OS keychain.
pub mod fred {
    use lines_core::CivilDate;
    use lines_core::econ::EconSchedule;
    use std::time::Duration;

    /// Where the API lives.
    pub const API: &str = "https://api.stlouisfed.org/fred/release/dates";

    /// Each bundled release's FRED release id.
    pub const RELEASES: [(&str, u32); 6] = [
        ("cpi", 10),
        ("nfp", 50),
        ("ppi", 46),
        ("gdp", 53),
        ("pce", 54),
        ("fomc", 101),
    ];

    /// The release's id on FRED, if it has one.
    pub fn release_id(key: &str) -> Option<u32> {
        RELEASES.iter().find(|(k, _)| *k == key).map(|&(_, id)| id)
    }

    /// The page a refreshed release names as its source.
    pub fn source_page(key: &str) -> Option<String> {
        release_id(key).map(|id| format!("https://fred.stlouisfed.org/release?rid={id}"))
    }

    /// The request for one release's dates from `from` on, future scheduled dates included.
    pub fn url(release: u32, api_key: &str, from: CivilDate) -> String {
        format!(
            "{API}?release_id={release}&api_key={api_key}&file_type=json             &include_release_dates_with_no_data=true&realtime_start={from}&sort_order=asc&limit=1000"
        )
    }

    /// The dates in an answer, or FRED's own error message.
    pub fn parse(body: &str) -> Result<Vec<CivilDate>, String> {
        #[derive(serde::Deserialize)]
        struct Answer {
            release_dates: Option<Vec<Row>>,
            error_message: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Row {
            date: String,
        }
        let a: Answer =
            serde_json::from_str(body).map_err(|e| format!("unreadable answer ({e})"))?;
        if let Some(m) = a.error_message {
            return Err(m.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        let rows = a.release_dates.ok_or("no release dates in the answer")?;
        rows.iter()
            .map(|r| date(&r.date).ok_or_else(|| format!("bad date {:?}", r.date)))
            .collect()
    }

    fn date(s: &str) -> Option<CivilDate> {
        let mut p = s.split('-');
        let (y, m, d) = (p.next()?, p.next()?, p.next()?);
        if p.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return None;
        }
        CivilDate::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
    }

    /// What a refresh got: each release's dates, and the first problem (if any release
    /// wasn't answered).
    #[derive(Debug, Default)]
    pub struct Fetched {
        pub dates: Vec<(String, Vec<CivilDate>)>,
        pub problem: Option<String>,
    }

    impl Fetched {
        /// Every release answered.
        pub fn complete(&self, schedule: &EconSchedule) -> bool {
            self.problem.is_none()
                && schedule
                    .releases
                    .iter()
                    .all(|r| self.dates.iter().any(|(k, _)| *k == r.key))
        }
    }

    /// Ask FRED for every release of `schedule` it has (blocking; never on a UI thread). A
    /// release that fails is left out and the first failure kept; nothing in it names the key.
    pub fn fetch(schedule: &EconSchedule, api_key: &str, from: CivilDate) -> Fetched {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .user_agent(concat!("lines/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let scrub = |m: String| {
            if api_key.is_empty() {
                m
            } else {
                m.replace(api_key, "…")
            }
        };
        let mut out = Fetched::default();
        for r in &schedule.releases {
            let Some(id) = release_id(&r.key) else {
                out.problem
                    .get_or_insert_with(|| format!("{}: not on FRED", r.name));
                continue;
            };
            let got = agent
                .get(&url(id, api_key, from))
                .call()
                .map_err(|e| format!("can't reach FRED ({e})"))
                .and_then(|mut resp| {
                    let status = resp.status().as_u16();
                    let body = resp
                        .body_mut()
                        .read_to_string()
                        .map_err(|e| format!("can't read FRED's answer ({e})"))?;
                    match parse(&body) {
                        Ok(d) if status == 200 => Ok(d),
                        Ok(_) => Err(format!("FRED answered {status}")),
                        Err(m) if status == 200 => Err(m),
                        Err(m) => Err(format!("FRED answered {status}: {m}")),
                    }
                });
            match got {
                Ok(d) => out.dates.push((r.key.clone(), d)),
                Err(m) => {
                    let m = scrub(m);
                    out.problem
                        .get_or_insert_with(|| format!("{}: {m}", r.name));
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_bundled_release_has_a_fred_id() {
            for r in &crate::econ::bundled().releases {
                assert!(release_id(&r.key).is_some(), "{}", r.key);
            }
        }

        #[test]
        fn answers_and_errors_parse() {
            let ok = r#"{"realtime_start":"2026-11-19","count":2,"release_dates":[
                {"release_id":10,"date":"2026-12-10"},{"release_id":10,"date":"2027-01-13"}]}"#;
            assert_eq!(
                parse(ok).unwrap(),
                [
                    CivilDate::new(2026, 12, 10).unwrap(),
                    CivilDate::new(2027, 1, 13).unwrap()
                ]
            );
            let bad = r#"{"error_code":400,"error_message":"Bad Request.  The value for variable api_key is not registered."}"#;
            assert_eq!(
                parse(bad).unwrap_err(),
                "Bad Request. The value for variable api_key is not registered."
            );
            assert!(parse("<html>").is_err());
            assert!(parse(r#"{"release_dates":[{"date":"soon"}]}"#).is_err());
        }

        #[test]
        fn the_request_asks_for_future_dates() {
            let u = url(10, "k", CivilDate::new(2026, 11, 19).unwrap());
            assert!(u.starts_with("https://api.stlouisfed.org/fred/release/dates?release_id=10&"));
            assert!(
                u.contains("&include_release_dates_with_no_data=true"),
                "{u}"
            );
            assert!(u.contains("&realtime_start=2026-11-19"), "{u}");
            assert!(u.contains("&file_type=json"), "{u}");
        }
    }
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
