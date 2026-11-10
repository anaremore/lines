//! "Download days…" for Databento (decision 0030 §8): the public `PastDays` seam on the
//! historical API.
//!
//! - **Contents:** "Trades + top of book" (`trades` + `mbp-1`, included in the Standard plan
//!   for the last 12 months), or "Full depth (top 10)" (`mbp-10`, whose trades come from its
//!   own records; included for the last month). `mbo` is later work. Every request also pulls
//!   the contract's `definition` records (free) for the definition check.
//! - **Estimate:** `metadata.get_cost` and `metadata.get_billable_size` per schema for the
//!   trade date's session (free calls); $0 is "included in your plan". A refusal is
//!   Databento's own message.
//! - **Download:** each schema streams (DBN + zstd) into the day's scratch directory inside
//!   the local store, then [`crate::import::convert`] turns them into Lines events with the
//!   importer's exact conversion and definition checks (tick, point value, currency,
//!   exchange, symbol: a disagreeing definition stops the day; nothing is stored).
//! - **Key:** [`ApiKey::load`] (`DATABENTO_API_KEY`, else the OS keychain entry
//!   `lines-databento`, else a key file), per call, on the download job's thread (never the
//!   UI's); never logged or shown.
//!
//! The data stays in the local store (decision 0010): the scratch files are deleted with the
//! day, and the stored segment is stamped with the source `databento`.

use std::path::PathBuf;

use lines_core::{CivilDate, Product, UtcMillis};
use lines_feed::catalog;
use lines_feed::download::{
    API_VERSION, Charge, Contents, DayError, DayEstimate, DayRequest, DaySink, PastDays, Refusal,
};

use crate::api::{self, ApiError, DATASET, Query};
use crate::import::raw_symbol;
use crate::key::ApiKey;

pub const TOB: Contents = Contents {
    id: "tob",
    label: "Trades + top of book",
};
pub const DEPTH: Contents = Contents {
    id: "mbp-10",
    label: "Full depth (top 10)",
};

/// The market schemas a choice fetches (the definition is added to every request).
pub fn schemas(contents: &str) -> Option<&'static [&'static str]> {
    match contents {
        "tob" => Some(&["trades", "mbp-1"]),
        "mbp-10" => Some(&["mbp-10"]),
        _ => None,
    }
}

/// `2026-11-04T23:00:00` (UTC, as Databento reads a time without a zone).
pub fn iso(t: UtcMillis) -> String {
    let (y, mo, d, h, mi, s) = t.to_civil();
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
}

/// The queries for one day: the definition over whole UTC days (Databento ids are mapped
/// per UTC day, and a session starts the evening before its trade date), then the market
/// schemas over the session.
pub fn queries(req: &DayRequest) -> Result<Vec<Query>, Refusal> {
    let raw = raw_symbol(req.contract)
        .ok_or_else(|| Refusal("Only futures contracts can be downloaded.".into()))?;
    in_life(&req.product, req.contract, req.date)?;
    let schemas = schemas(req.contents)
        .ok_or_else(|| Refusal(format!("Unknown contents {:?}.", req.contents)))?;
    let q = |schema: &str, start: String, end: String| Query {
        dataset: DATASET.into(),
        schema: schema.into(),
        symbols: vec![raw.clone()],
        stype_in: "raw_symbol".into(),
        start,
        end,
    };
    let day_start = CivilDate::from_utc(req.open);
    let day_end = CivilDate::from_utc(UtcMillis(req.close.0 - 1)).add_days(1);
    let mut v = vec![q("definition", day_start.to_string(), day_end.to_string())];
    for s in schemas {
        v.push(q(s, iso(req.open), iso(req.close)));
    }
    Ok(v)
}

/// Years before expiry a contract can be listed: 5 for a quarterly cycle (ES lists about
/// two years out), else 9 (one-digit years repeat every 10, so a raw symbol names one
/// contract only inside a decade); 12 for roots with two-digit years (NG, CL from 2030).
fn horizon_years(product: &Product, contract: lines_core::ContractKey) -> i64 {
    let two_digits = contract
        .year_month()
        .is_some_and(|(y, _)| crate::import::year_digits(product.symbol, y) == 2);
    match product.kind {
        _ if two_digits => 12,
        lines_core::ProductKind::Future { cycle, .. }
            if cycle == lines_core::MonthCycle::QUARTERLY =>
        {
            5
        }
        _ => 9,
    }
}

/// A date inside the contract's possible life, else why not, before anything is priced:
/// Databento's raw symbols carry one year digit (ESZ6), so a date a decade off would name
/// (and bill for) another contract.
pub fn in_life(
    product: &Product,
    contract: lines_core::ContractKey,
    date: CivilDate,
) -> Result<(), Refusal> {
    let label = contract.label(product);
    let expiry = lines_core::calendar::calendar_expiry(product, contract).ok_or_else(|| {
        Refusal(format!(
            "{}: no expiry is known, so it can't be downloaded.",
            label.as_str()
        ))
    })?;
    if date > expiry {
        return Err(Refusal(format!(
            "{} expired on {expiry}: pick dates up to then.",
            label.as_str()
        )));
    }
    let years = horizon_years(product, contract);
    if expiry.days() - date.days() > years * 366 {
        return Err(Refusal(format!(
            "{} wasn't listed yet on {date} (it lists at most {years} years before it expires \
             on {expiry}).",
            label.as_str()
        )));
    }
    Ok(())
}

/// The contract's definition in `path` agrees with the catalog (tick, point value, currency,
/// exchange, symbol), else why not.
pub fn check_definition(
    path: &std::path::Path,
    contract: lines_core::ContractKey,
) -> Result<(), String> {
    let raw = raw_symbol(contract).ok_or("not a futures contract")?;
    let mut map = crate::defs::InstrumentMap::default();
    crate::import::read_definitions(path, &mut map).map_err(|e| format!("{e:#}"))?;
    let bad = map.bad_metadata_for(&raw);
    if !bad.is_empty() {
        return Err(format!(
            "Databento's definition disagrees with Lines' catalog, nothing was downloaded: {}",
            bad.join("; ")
        ));
    }
    if !map.contracts().contains_key(&contract) {
        return Err(format!(
            "Databento sent no definition for {raw}; nothing was downloaded."
        ));
    }
    Ok(())
}

/// A download error: Databento's refusal in its own words, or a failure on this side.
/// Only Databento's own refusals (4xx: entitlement, symbol, key) are `Refused`; a busy or
/// failing gateway (429, 5xx), the network and anything local are `Failed` (with Databento's
/// text when it gave one).
pub fn fetch_error(e: &anyhow::Error) -> DayError {
    match e.downcast_ref::<ApiError>() {
        // 408 (timeout) and 429 (busy) are the gateway's state, not an answer about the data.
        Some(a) if (400..500).contains(&a.status) && a.status != 408 && a.status != 429 => {
            DayError::Refused(refusal(e))
        }
        Some(_) => DayError::Failed(refusal(e).0),
        None => DayError::Failed(format!("{e:#}")),
    }
}

/// Dollars on the wire to millionths, rounded to the nearest (the float's own noise is below
/// that); the dialog shows cents rounded up.
fn micros(usd: f64) -> Option<u64> {
    (usd.is_finite() && usd >= 0.0 && usd * 1e6 < u64::MAX as f64)
        // A nonzero quote is never "included": at least one millionth.
        .then(|| ((usd * 1e6).round() as u64).max(u64::from(usd > 0.0)))
}

/// A download's fetcher: one query into a file, reporting bytes (`false` from the callback
/// stops it). The real one is [`api::fetch`]; tests pass a fake.
pub type Fetch<'a> =
    dyn FnMut(&Query, &std::path::Path, &mut dyn FnMut(u64) -> bool) -> anyhow::Result<u64> + 'a;

/// One day into `sink`, fetching through `fetch`: the definition first (free) and checked
/// before any paid schema is asked for; a cancel is honored before each fetch. The sink hears
/// `requesting` right before the first paid request (the day counts as charged from there)
/// and `fetched` once every file has arrived: the money is spent, so the conversion then runs
/// to the end and the day is stored even if Stop now was pressed meanwhile.
pub fn download_with(
    req: &DayRequest,
    sink: &mut dyn DaySink,
    fetch: &mut Fetch<'_>,
) -> Result<(), DayError> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut before = 0u64;
    for q in queries(req).map_err(|r| DayError::Failed(r.0))? {
        if sink.canceled() {
            return Err(DayError::Failed("stopped".into()));
        }
        if q.schema != "definition" {
            sink.requesting();
        }
        let path = sink.scratch_dir().join(format!("{}.dbn.zst", q.schema));
        let n = fetch(&q, &path, &mut |n| {
            sink.received(before + n);
            !sink.canceled()
        })
        .map_err(|e| fetch_error(&e))?;
        before += n;
        if q.schema == "definition" {
            check_definition(&path, req.contract).map_err(DayError::Failed)?;
        }
        files.push(path);
    }
    sink.fetched();
    convert_day(req, &files, sink)
}

/// Databento's own words for an error (the HTTP status and method only when it gave none).
pub fn refusal(e: &anyhow::Error) -> Refusal {
    match e.downcast_ref::<ApiError>() {
        Some(a) if !a.message.trim().is_empty() => Refusal(a.message.clone()),
        Some(a) => Refusal(format!("Databento answered HTTP {}.", a.status)),
        None => Refusal(format!("{e:#}")),
    }
}

/// Databento's historical API as a past-days source.
#[derive(Clone, Debug, Default)]
pub struct DatabentoDays;

impl DatabentoDays {
    fn key() -> Result<ApiKey, DayError> {
        ApiKey::load().map_err(|e| DayError::Failed(format!("{e:#}")))
    }
}

/// The products Databento's CME dataset carries: the catalog's CME Group futures.
pub fn serves(product: &Product) -> bool {
    !product.is_spot()
        && !product.is_equity()
        && matches!(
            catalog::exchange(product.id),
            "CME" | "CBOT" | "NYMEX" | "COMEX"
        )
}

impl PastDays for DatabentoDays {
    fn api_version(&self) -> u32 {
        API_VERSION
    }

    fn contents(&self, product: &Product) -> Vec<Contents> {
        if serves(product) {
            vec![TOB, DEPTH]
        } else {
            Vec::new()
        }
    }

    fn estimate(&self, req: &DayRequest) -> Result<DayEstimate, DayError> {
        // This side's checks first (the contract's life, the key): never Databento's words.
        let queries = queries(req).map_err(|r| DayError::Failed(r.0))?;
        let key = Self::key()?;
        let (mut usd_micros, mut bytes) = (0u64, 0u64);
        for q in queries {
            let e = api::estimate(&key, &q).map_err(|e| fetch_error(&e))?;
            usd_micros += micros(e.usd).ok_or_else(|| {
                DayError::Failed(format!("Databento quoted an unusable price ({}).", e.usd))
            })?;
            bytes += e.bytes;
        }
        let charge = if usd_micros == 0 {
            Charge::Included
        } else {
            Charge::Estimated { usd_micros }
        };
        Ok(DayEstimate { charge, bytes })
    }

    fn download(&self, req: &DayRequest, sink: &mut dyn DaySink) -> Result<(), DayError> {
        let key = Self::key()?;
        download_with(req, sink, &mut |q, path, progress| {
            api::fetch(&key, q, path, progress)
        })
    }
}

/// The fetched files into `sink`, with the importer's conversion and checks.
fn convert_day(
    req: &DayRequest,
    files: &[PathBuf],
    sink: &mut dyn DaySink,
) -> Result<(), DayError> {
    let mut stopped = false;
    let report = crate::import::convert(files, req.contract, |recv_us, events| {
        sink.frame(recv_us, events).map_err(|_| {
            stopped = true;
            anyhow::anyhow!("stopped")
        })
    });
    match report {
        Ok(r) => {
            tracing::info!(
                "databento day {} {}: {} trades, {} quote changes, {} book batches, \
                     rejected {} trades / {} quotes / {} levels",
                req.date,
                req.contents,
                r.trades_written,
                r.quotes_written,
                r.book.batches,
                r.trades_rejected,
                r.quotes_rejected,
                r.book.off_grid_levels
            );
            // The files hold only this contract's records (asked for by its symbol): any
            // without a definition means the day is incomplete.
            if r.unmapped > 0 {
                tracing::warn!(
                    "databento day {}: {} unmapped records",
                    req.date,
                    r.unmapped
                );
                return Err(DayError::Failed(format!(
                    "{} records had no definition for their day; nothing stored",
                    r.unmapped
                )));
            }
            Ok(())
        }
        Err(_) if stopped => Err(DayError::Failed("stopped".into())),
        Err(e) => Err(DayError::Failed(format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{ContractKey, Month};

    fn req(contents: &'static str) -> DayRequest {
        let es = catalog::get(catalog::ES).unwrap();
        let date = CivilDate::new(2026, 11, 5).unwrap();
        let (open, close) = lines_feed::disk::day::session_span(&es, date).unwrap();
        DayRequest {
            product: es,
            contract: ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap()),
            date,
            open,
            close,
            contents,
        }
    }

    #[test]
    fn a_day_asks_for_its_session_and_whole_utc_days_of_definitions() {
        let qs = queries(&req("tob")).unwrap();
        let v: Vec<(&str, &str, &str)> = qs
            .iter()
            .map(|q| (q.schema.as_str(), q.start.as_str(), q.end.as_str()))
            .collect();
        // ES trade date Thu 2026-11-05: Wed 17:00 CT (23:00 UTC) to Thu 16:00 CT.
        assert_eq!(
            v,
            vec![
                ("definition", "2026-11-04", "2026-11-06"),
                ("trades", "2026-11-04T23:00:00", "2026-11-05T22:00:00"),
                ("mbp-1", "2026-11-04T23:00:00", "2026-11-05T22:00:00"),
            ]
        );
        assert!(
            qs.iter()
                .all(|q| q.symbols == ["ESZ6"] && q.validate().is_ok())
        );
        let qs = queries(&req("mbp-10")).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[1].schema, "mbp-10");
        assert!(queries(&req("mbo")).is_err());
    }

    /// ESZ26 with 2016 dates would be ESZ6 = December 2016 at Databento: refused before
    /// anything is priced.
    #[test]
    fn dates_outside_the_contracts_life_are_refused_first() {
        let es = catalog::get(catalog::ES).unwrap();
        let z26 = ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap());
        let day = |y, m, d| CivilDate::new(y, m, d).unwrap();
        assert!(in_life(&es, z26, day(2026, 11, 5)).is_ok());
        assert!(in_life(&es, z26, day(2022, 1, 3)).is_ok(), "within 5 years");
        let e = in_life(&es, z26, day(2016, 11, 4)).unwrap_err();
        assert_eq!(
            e.0,
            "ESZ26 wasn't listed yet on 2016-11-04 (it lists at most 5 years before it expires \
             on 2026-12-18)."
        );
        let e = in_life(&es, z26, day(2026, 12, 21)).unwrap_err();
        assert!(e.0.contains("expired on 2026-12-18"), "{e}");
        let mut r = req("tob");
        r.date = day(2016, 11, 4);
        assert!(queries(&r).is_err(), "no query is formed");
        // CL (monthly, one digit until 2030): 9 years.
        let cl = catalog::by_symbol("CL").unwrap();
        let clz6 = ContractKey::monthly(cl.id, 2026, Month::new(12).unwrap());
        assert!(in_life(&cl, clz6, day(2018, 1, 2)).is_ok());
        assert!(in_life(&cl, clz6, day(2016, 1, 4)).is_err());
    }

    #[test]
    fn contents_only_for_cme_futures() {
        let d = DatabentoDays;
        assert_eq!(d.api_version(), API_VERSION);
        assert_eq!(
            d.contents(&catalog::by_symbol("ES").unwrap()),
            vec![TOB, DEPTH]
        );
        assert!(
            d.contents(&catalog::by_symbol("BTC/USD").unwrap())
                .is_empty()
        );
    }

    #[test]
    fn refusals_are_databentos_words() {
        let e: anyhow::Error = ApiError {
            method: "metadata.get_cost".into(),
            status: 403,
            message: "Your license doesn't include this data.".into(),
        }
        .into();
        assert_eq!(refusal(&e).0, "Your license doesn't include this data.");
        let e: anyhow::Error = ApiError {
            method: "x".into(),
            status: 500,
            message: " ".into(),
        }
        .into();
        assert_eq!(refusal(&e).0, "Databento answered HTTP 500.");
    }

    // ---- the download's order and checks, with a fake fetcher (synthetic DBN only) ----

    use dbn::encode::{DbnEncoder, EncodeRecord};
    use dbn::record::str_to_c_chars;
    use dbn::{InstrumentDefMsg, Metadata, RecordHeader, SType, Schema, TradeMsg, rtype};
    use lines_feed::native::NativeEvent;

    const NOV4: u64 = 1_793_750_400_000_000_000; // 2026-11-04T00:00:00Z
    const DAY: u64 = 86_400_000_000_000;

    fn es_def(ts: u64, currency: &str) -> InstrumentDefMsg {
        InstrumentDefMsg {
            hd: RecordHeader::new::<InstrumentDefMsg>(rtype::INSTRUMENT_DEF, 1, 42, ts),
            ts_recv: ts,
            min_price_increment: 250_000_000,
            display_factor: 1_000_000_000,
            unit_of_measure_qty: 50 * 1_000_000_000,
            unit_of_measure: str_to_c_chars("IPNT").unwrap(),
            currency: str_to_c_chars(currency).unwrap(),
            exchange: str_to_c_chars("XCME").unwrap(),
            maturity_year: 2026,
            maturity_month: 12,
            raw_symbol: str_to_c_chars("ESZ6").unwrap(),
            asset: str_to_c_chars("ES").unwrap(),
            instrument_class: b'F' as std::ffi::c_char,
            ..Default::default()
        }
    }

    fn write<R: dbn::encode::DbnEncodable>(path: &std::path::Path, schema: Schema, recs: &[R]) {
        let meta = Metadata::builder()
            .dataset("GLBX.MDP3")
            .schema(Some(schema))
            .start(NOV4)
            .stype_in(Some(SType::RawSymbol))
            .stype_out(SType::InstrumentId)
            .build();
        let f = std::fs::File::create(path).unwrap();
        let mut e = DbnEncoder::with_zstd(f, &meta).unwrap();
        for r in recs {
            e.encode_record(r).unwrap();
        }
    }

    struct TestSink {
        scratch: PathBuf,
        frames: Vec<Vec<NativeEvent>>,
        canceled: bool,
        requested: bool,
        fetched: bool,
    }

    impl DaySink for TestSink {
        fn frame(&mut self, _: i64, ev: &[NativeEvent]) -> Result<(), lines_feed::download::Stop> {
            self.frames.push(ev.to_vec());
            Ok(())
        }
        fn received(&mut self, _: u64) {}
        fn canceled(&self) -> bool {
            self.canceled
        }
        fn scratch_dir(&self) -> &std::path::Path {
            &self.scratch
        }
        fn requesting(&mut self) {
            self.requested = true;
        }
        fn fetched(&mut self) {
            self.fetched = true;
        }
    }

    fn sink(name: &str) -> TestSink {
        let d = std::env::temp_dir().join(format!("lines-days-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TestSink {
            scratch: d,
            frames: Vec::new(),
            canceled: false,
            requested: false,
            fetched: false,
        }
    }

    /// A fake gateway: writes what each schema would return and records what was asked.
    fn fake<'a>(
        asked: &'a mut Vec<String>,
        currency: &'a str,
    ) -> impl FnMut(&Query, &std::path::Path, &mut dyn FnMut(u64) -> bool) -> anyhow::Result<u64> + 'a
    {
        move |q, path, progress| {
            asked.push(q.schema.clone());
            match q.schema.as_str() {
                "definition" => write(
                    path,
                    Schema::Definition,
                    &[es_def(NOV4 + 1, currency), es_def(NOV4 + DAY + 1, currency)],
                ),
                "trades" => {
                    let ts = NOV4 + DAY + 14 * 3_600_000_000_000;
                    write(
                        path,
                        Schema::Trades,
                        &[TradeMsg {
                            hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, 1, 42, ts - 1_000),
                            price: 6_000_000_000_000,
                            size: 2,
                            action: b'T' as std::ffi::c_char,
                            side: b'B' as std::ffi::c_char,
                            flags: dbn::FlagSet::empty().set_last(),
                            depth: 0,
                            ts_recv: ts,
                            ts_in_delta: 0,
                            sequence: 1,
                        }],
                    )
                }
                _ => write::<TradeMsg>(path, Schema::Mbp1, &[]),
            }
            progress(1);
            Ok(std::fs::metadata(path)?.len())
        }
    }

    #[test]
    fn the_definition_is_checked_before_anything_paid_is_asked_for() {
        let mut asked = Vec::new();
        let mut s = sink("order");
        let e = download_with(&req("tob"), &mut s, &mut fake(&mut asked, "EUR")).unwrap_err();
        assert!(
            matches!(&e, DayError::Failed(m) if m.contains("disagrees")),
            "{e:?}"
        );
        assert_eq!(asked, ["definition"], "no paid schema was requested");
        assert!(s.frames.is_empty());
        assert!(!s.requested, "never counted as charged");

        let mut asked = Vec::new();
        let mut s = sink("good");
        download_with(&req("tob"), &mut s, &mut fake(&mut asked, "USD")).unwrap();
        assert_eq!(asked, ["definition", "trades", "mbp-1"]);
        assert_eq!(s.frames.len(), 1, "the trade");
        assert!(s.requested && s.fetched);

        // Canceled: nothing is asked for at all.
        let mut asked = Vec::new();
        let mut s = sink("cancel");
        s.canceled = true;
        assert!(download_with(&req("tob"), &mut s, &mut fake(&mut asked, "USD")).is_err());
        assert!(asked.is_empty());
    }

    #[test]
    fn check_definition_refuses_a_bad_or_missing_contract() {
        let s = sink("defs");
        let p = s.scratch.join("d.dbn.zst");
        let z6 = ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap());
        write(&p, Schema::Definition, &[es_def(NOV4 + 1, "USD")]);
        assert!(check_definition(&p, z6).is_ok());
        write(&p, Schema::Definition, &[es_def(NOV4 + 1, "EUR")]);
        assert!(check_definition(&p, z6).unwrap_err().contains("disagrees"));
        let h7 = ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap());
        write(&p, Schema::Definition, &[es_def(NOV4 + 1, "USD")]);
        assert!(
            check_definition(&p, h7)
                .unwrap_err()
                .contains("no definition for ESH7")
        );
    }

    #[test]
    fn only_databentos_own_refusals_are_refusals() {
        let err = |status: u16| -> anyhow::Error {
            ApiError {
                method: "timeseries.get_range".into(),
                status,
                message: "words".into(),
            }
            .into()
        };
        assert_eq!(
            fetch_error(&err(403)),
            DayError::Refused(Refusal("words".into()))
        );
        assert_eq!(fetch_error(&err(429)), DayError::Failed("words".into()));
        assert_eq!(fetch_error(&err(408)), DayError::Failed("words".into()));
        assert_eq!(fetch_error(&err(503)), DayError::Failed("words".into()));
        assert!(matches!(
            fetch_error(&anyhow::anyhow!("connection reset")),
            DayError::Failed(m) if m == "connection reset"
        ));
        assert_eq!(micros(1.636), Some(1_636_000));
        assert_eq!(micros(0.0000004), Some(1), "a nonzero quote is never free");
        assert_eq!(micros(0.0), Some(0));
        assert_eq!(micros(2.587353944778), Some(2_587_354));
        assert_eq!(micros(-1.0), None);
    }
}
