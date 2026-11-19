//! The bundled economic calendar (roadmap 8b): official US release schedules (BLS, BEA, the
//! Federal Reserve) as versioned data, so the news blackout works offline, in replays and in
//! tests. Sources and the verification status of every line are in the file; the format and
//! the missing / stale rules are in [`lines_core::econ`].

use std::sync::{Arc, OnceLock};

use lines_core::econ::EconSchedule;

/// The shipped schedule's text.
pub const BUNDLED: &str = include_str!("../data/econ-calendar.txt");

/// A fingerprint of the shipped schedule's text (FNV-1a): a cached refresh records the one it
/// was built on, so a build with a changed calendar never reads an older build's cache.
pub fn bundled_hash() -> u64 {
    BUNDLED.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

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

    /// A bundled release FRED gives the dates of, and the cadence a list of its dates must
    /// keep to be taken.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Mapped {
        pub key: &'static str,
        /// FRED's release id.
        pub id: u32,
        /// Fewest days between two releases.
        pub min_gap_days: i64,
        /// Most releases in any year from today.
        pub max_per_year: usize,
    }

    /// Releases taken from FRED. Monthly ones: a week apart at least (an agency catching up
    /// after a shutdown can bunch them) and 14 a year at most; GDP has an estimate most
    /// months. **FOMC is not here:** FRED's "FOMC Press Release" (101) also carries daily
    /// series, so its release dates are every day, not the statements (owner report,
    /// 2026-11-19). The Fed publishes its meetings a year ahead; the bundled list is used.
    pub const RELEASES: [Mapped; 5] = [
        Mapped {
            key: "cpi",
            id: 10,
            min_gap_days: 7,
            max_per_year: 14,
        },
        Mapped {
            key: "nfp",
            id: 50,
            min_gap_days: 7,
            max_per_year: 14,
        },
        Mapped {
            key: "ppi",
            id: 46,
            min_gap_days: 7,
            max_per_year: 14,
        },
        Mapped {
            key: "gdp",
            id: 53,
            min_gap_days: 7,
            max_per_year: 15,
        },
        Mapped {
            key: "pce",
            id: 54,
            min_gap_days: 7,
            max_per_year: 14,
        },
    ];

    /// Bumped whenever [`RELEASES`] or [`check`] change what a refresh can add: a cache built
    /// under another mapping is discarded at start (`econ_refresh::usable_cache`), so dates a
    /// wrong mapping added never outlive it. 2: FOMC dropped, cadence checked.
    pub const MAPPING_VERSION: u32 = 2;

    pub fn mapped(key: &str) -> Option<&'static Mapped> {
        RELEASES.iter().find(|m| m.key == key)
    }

    /// The release's id on FRED, if Lines takes its dates from there.
    pub fn release_id(key: &str) -> Option<u32> {
        mapped(key).map(|m| m.id)
    }

    /// Whether a release's dates from `today` on keep its cadence: weekdays only, at least
    /// `min_gap_days` apart, at most `max_per_year` in the year ahead. The error says what
    /// broke it, in words.
    pub fn check(m: &Mapped, dates: &[CivilDate], today: CivilDate) -> Result<(), String> {
        let mut ahead: Vec<CivilDate> = dates.iter().copied().filter(|d| *d >= today).collect();
        ahead.sort();
        ahead.dedup();
        for d in &ahead {
            // 1970-01-01 was a Thursday: 0 = Monday … 5 = Saturday, 6 = Sunday.
            if (d.days() + 3).rem_euclid(7) >= 5 {
                return Err(format!("a weekend date ({d})"));
            }
        }
        if let Some(w) = ahead
            .windows(2)
            .find(|w| w[1].days() - w[0].days() < m.min_gap_days)
        {
            return Err(format!(
                "dates {} days apart ({} and {})",
                w[1].days() - w[0].days(),
                w[0],
                w[1]
            ));
        }
        let year = ahead
            .iter()
            .filter(|d| d.days() - today.days() < 365)
            .count();
        if year > m.max_per_year {
            return Err(format!(
                "{year} dates in a year (at most {})",
                m.max_per_year
            ));
        }
        Ok(())
    }

    /// The answers that may be used: releases Lines takes from FRED whose dates keep their
    /// cadence. Any other is left out (the calendar keeps its own dates for it) and the
    /// first such problem kept.
    pub fn validated(
        schedule: &EconSchedule,
        answers: Vec<(String, Vec<CivilDate>)>,
        today: CivilDate,
    ) -> Fetched {
        let mut out = Fetched::default();
        for (key, dates) in answers {
            let Some(m) = mapped(&key) else {
                continue;
            };
            match check(m, &dates, today) {
                Ok(()) => out.dates.push((key, dates)),
                Err(why) => {
                    let name = schedule
                        .releases
                        .iter()
                        .find(|r| r.key == key)
                        .map_or(key.as_str(), |r| r.name.as_str());
                    out.problem
                        .get_or_insert_with(|| format!("{name} dates from FRED refused: {why}"));
                }
            }
        }
        out
    }

    /// The page a refreshed release names as its source.
    pub fn source_page(key: &str) -> Option<String> {
        release_id(key).map(|id| format!("https://fred.stlouisfed.org/release?rid={id}"))
    }

    /// The request for one release's dates, future scheduled dates included, newest first (a
    /// release with more than the limit's dates since it began still gives its newest).
    pub fn url(release: u32, api_key: &str, from: CivilDate) -> String {
        format!(
            "{API}?release_id={release}&api_key={api_key}&file_type=json\
             &include_release_dates_with_no_data=true&realtime_start={from}&sort_order=desc&limit=1000"
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
        /// Every release Lines takes from FRED answered with a date from `today` on (a
        /// release with only past dates said nothing about what is ahead). Releases it doesn't
        /// take (FOMC) keep the calendar's own published list.
        pub fn complete(&self, schedule: &EconSchedule, today: CivilDate) -> bool {
            self.problem.is_none()
                && schedule
                    .releases
                    .iter()
                    .filter(|r| mapped(&r.key).is_some())
                    .all(|r| {
                        self.dates
                            .iter()
                            .any(|(k, d)| *k == r.key && d.iter().any(|d| *d >= today))
                    })
        }
    }

    /// Ask FRED for every release of `schedule` Lines takes from it (blocking; never on a UI
    /// thread), then keep only answers that keep their cadence ([`validated`]). A release that
    /// fails is left out and the first failure kept; nothing in it names the key.
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
        let mut answers = Vec::new();
        let mut problem: Option<String> = None;
        for r in &schedule.releases {
            // Releases not taken from FRED (FOMC) keep the calendar's list.
            let Some(id) = release_id(&r.key) else {
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
                Ok(d) => answers.push((r.key.clone(), d)),
                Err(m) => {
                    let m = scrub(m);
                    problem.get_or_insert_with(|| format!("{}: {m}", r.name));
                }
            }
        }
        let mut out = validated(schedule, answers, from);
        if out.problem.is_none() {
            out.problem = problem;
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Every bundled release but FOMC is taken from FRED; FOMC never is (FRED's release
        /// 101 is dated every day).
        #[test]
        fn every_bundled_release_but_fomc_has_a_fred_id() {
            for r in &crate::econ::bundled().releases {
                assert_eq!(release_id(&r.key).is_some(), r.key != "fomc", "{}", r.key);
            }
            assert!(RELEASES.iter().all(|m| m.id != 101));
        }

        fn d(y: u16, m: u8, day: u8) -> CivilDate {
            CivilDate::new(y, m, day).unwrap()
        }

        /// The owner's report: a release dated every day (weekends too) is refused whole,
        /// says why, and leaves the refresh incomplete; a monthly list passes; dates before
        /// today aren't judged.
        #[test]
        fn a_list_off_its_cadence_is_refused() {
            let s = crate::econ::bundled();
            let today = d(2026, 11, 19);
            let cpi = mapped("cpi").unwrap();
            let monthly = [d(2026, 12, 10), d(2027, 1, 13), d(2027, 2, 11)];
            assert_eq!(check(cpi, &monthly, today), Ok(()));
            let daily: Vec<CivilDate> = (19..=30).map(|n| d(2026, 11, n)).collect();
            let e = check(cpi, &daily, today).unwrap_err();
            assert!(e.contains("weekend"), "{e}");
            // Weekdays only, but a day apart.
            let e = check(cpi, &[d(2026, 11, 19), d(2026, 11, 20)], today).unwrap_err();
            assert_eq!(e, "dates 1 days apart (2026-11-19 and 2026-11-20)");
            // Too many in a year, each a week apart.
            let weekly: Vec<CivilDate> = (0..20)
                .map(|w| CivilDate::from_days(d(2026, 11, 19).days() + 7 * w))
                .collect();
            assert!(
                check(cpi, &weekly, today)
                    .unwrap_err()
                    .contains("in a year")
            );
            // Old dates (before today) are not judged.
            assert_eq!(
                check(
                    cpi,
                    &[d(2026, 11, 14), d(2026, 11, 15), d(2026, 12, 10)],
                    today
                ),
                Ok(())
            );
            // Through `validated`: the daily list is left out with a problem in words, FOMC
            // answers are ignored, CPI kept; the refresh is not complete.
            let f = validated(
                &s,
                vec![
                    ("cpi".to_owned(), monthly.to_vec()),
                    ("ppi".to_owned(), daily.clone()),
                    ("fomc".to_owned(), daily),
                ],
                today,
            );
            assert_eq!(f.dates.len(), 1);
            assert_eq!(f.dates[0].0, "cpi");
            assert!(
                f.problem
                    .as_deref()
                    .is_some_and(|p| p.starts_with("PPI dates from FRED refused: a weekend")),
                "{:?}",
                f.problem
            );
            assert!(!f.complete(&s, today));
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

        /// A release whose answer has only past dates leaves the refresh incomplete, so the
        /// calendar's version (and so its staleness) doesn't move.
        #[test]
        fn only_future_dates_count_as_an_answer() {
            let s = crate::econ::bundled();
            let today = CivilDate::new(2026, 11, 19).unwrap();
            let past = CivilDate::new(2026, 1, 1).unwrap();
            let next = CivilDate::new(2026, 12, 10).unwrap();
            let mut f = Fetched {
                dates: RELEASES
                    .iter()
                    .map(|m| (m.key.to_owned(), vec![next]))
                    .collect(),
                problem: None,
            };
            assert!(f.complete(&s, today));
            f.dates[0].1 = vec![past];
            assert!(!f.complete(&s, today));
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
            assert!(u.contains("&sort_order=desc"), "newest first: {u}");
            assert!(!u.contains(char::is_whitespace), "{u}");
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
