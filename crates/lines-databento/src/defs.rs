//! Instrument ids → Lines contracts, from Databento `definition` records.
//!
//! Databento's `instrument_id` is unique only within a (UTC) day and may be reused, so the map
//! is keyed by `(day, instrument_id)` and built from the definitions of that day. A market
//! record from a day with no definition for its id is *unmapped* and counted, never guessed.
//!
//! Every definition is checked against the catalog (like Kraken's metadata check): a futures
//! contract whose tick, point value, currency, exchange or symbol disagrees is refused as
//! `BadMetadata`.

use std::collections::BTreeMap;

use dbn::{InstrumentClass, InstrumentDefMsg};
use lines_core::instrument::Month;
use lines_core::{CivilDate, ContractKey, Product, UtcMillis};
use lines_feed::catalog;

use crate::import::raw_symbol_for;
use crate::price::{NANO, PriceConv};

/// UTC civil day of a Databento nanosecond timestamp.
pub fn day_of(ts_ns: u64) -> CivilDate {
    CivilDate::from_utc(UtcMillis((ts_ns / 1_000_000) as i64))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefOutcome {
    Contract(ContractKey),
    /// Not something Lines charts (a spread, an option, a product not in the catalog).
    NotCharted(String),
    /// In the catalog, but the definition disagrees with it.
    BadMetadata(String),
}

#[derive(Default, Debug)]
pub struct InstrumentMap {
    by_day: BTreeMap<(CivilDate, u32), DefOutcome>,
    pub definitions: u64,
}

fn month_code(m: u8) -> Option<char> {
    Month::new(m).map(|m| m.code())
}

/// What one definition means for Lines.
pub fn classify(def: &InstrumentDefMsg) -> DefOutcome {
    let raw = def.raw_symbol().unwrap_or("").to_string();
    match def.instrument_class() {
        Ok(InstrumentClass::Future) => {}
        Ok(other) => return DefOutcome::NotCharted(format!("{raw}: {other:?}")),
        Err(_) => return DefOutcome::NotCharted(format!("{raw}: unknown class")),
    }
    let asset = def.asset().unwrap_or("").trim().to_string();
    let Some(product) = catalog::by_symbol(&asset).filter(|p| !p.symbol.contains('/')) else {
        return DefOutcome::NotCharted(format!("{raw}: product {asset:?} not in the catalog"));
    };
    if let Err(m) = check_spec(def, &product) {
        return DefOutcome::BadMetadata(format!("{raw}: {m}"));
    }
    let (year, month) = (def.maturity_year, def.maturity_month);
    let Some(code) = month_code(month) else {
        return DefOutcome::BadMetadata(format!("{raw}: maturity month {month}"));
    };
    // CME raw symbols carry a one-digit year (`ESZ6`), except roots that use two (`NGZ26`).
    let expected = raw_symbol_for(&asset, code, year);
    if raw != expected {
        return DefOutcome::BadMetadata(format!(
            "{raw}: symbol disagrees with maturity {year}-{month:02} ({expected})"
        ));
    }
    let label = format!("{asset}{code}{:02}", year % 100);
    let Some(key) = ContractKey::parse_label(&product, &label) else {
        // A serial month of a root known to list them (gold's November, FX monthlies): CME
        // lists it, Lines doesn't chart it. Any other root's month outside the catalog's
        // cycle means the cycle is wrong, which is refused rather than skipped.
        return if SERIAL_MONTH_ROOTS.contains(&product.symbol) {
            DefOutcome::NotCharted(format!("{raw}: not one of {}'s months", product.symbol))
        } else {
            DefOutcome::BadMetadata(format!("{raw}: {label} is not a listed month"))
        };
    };
    DefOutcome::Contract(key)
}

/// Roots whose `GLBX.MDP3` definitions list months outside the catalog's cycle (serial
/// months), from the gateway's definitions (2026-11-09): GC, SI and HG list every month, the
/// FX roots monthly contracts beside the quarterlies.
pub const SERIAL_MONTH_ROOTS: &[&str] = &["GC", "SI", "HG", "6E", "6J", "6B"];

/// The exchange's MIC for a catalog exchange name.
fn mic(exchange: &str) -> Option<&'static str> {
    Some(match exchange {
        "CME" => "XCME",
        "CBOT" => "XCBT",
        "NYMEX" => "XNYM",
        "COMEX" => "XCEC",
        _ => return None,
    })
}

/// Point value (USD per 1.0 of price per contract) as an exact ratio, from the definition's
/// unit of measure: `unit_of_measure_qty` is in 1e-9 units. Treasuries (`USD`: face value,
/// priced per 100) and grains (`BU`: priced in cents) divide it by 100; index points, barrels,
/// MMBtu, troy ounces, pounds and the FX units use it as-is.
pub fn point_value(def: &InstrumentDefMsg) -> Option<(i128, i128)> {
    let per = match def.unit_of_measure().ok()?.trim() {
        "USD" | "BU" => 100,
        "IPNT" | "BBL" | "MMBTU" | "TRYOZ" | "LBS" | "EUR" | "JPY" | "GBP" => 1,
        _ => return None,
    };
    (def.unit_of_measure_qty > 0).then_some((i128::from(def.unit_of_measure_qty), NANO * per))
}

/// Every catalog spec the definition carries: tick, point value, currency and exchange.
fn check_spec(def: &InstrumentDefMsg, product: &Product) -> Result<(), String> {
    // The tick is the scale check: prices are always in 1e-9 units, so the catalog tick in
    // nanos must equal `min_price_increment` exactly, or every price would land off the grid.
    // `display_factor` is not checked: it is a display hint that varies by product (YM 1,
    // GC 0.1, SI 0.001, 6J 0.000001 on the gateway, 2026-11-09) and never scales a price.
    let conv = PriceConv::new(product);
    if conv.tick_nanos() != Some(def.min_price_increment) {
        return Err(format!(
            "tick {} nanos, catalog {:?}",
            def.min_price_increment,
            conv.tick_nanos()
        ));
    }
    let uom = def.unit_of_measure().unwrap_or("").trim().to_string();
    let pv = product.point_value;
    match point_value(def) {
        Some((n, d)) if n * i128::from(pv.den) == i128::from(pv.num) * d => {}
        _ => {
            return Err(format!(
                "multiplier {} nanos {uom:?}, catalog {}/{} {} per point",
                def.unit_of_measure_qty,
                pv.num,
                pv.den,
                product.currency.as_str()
            ));
        }
    }
    let ccy = def.currency().unwrap_or("").trim();
    if ccy != product.currency.as_str() {
        return Err(format!(
            "currency {ccy:?}, catalog {}",
            product.currency.as_str()
        ));
    }
    let exch = def.exchange().unwrap_or("").trim();
    let want = mic(catalog::exchange(product.id));
    if want != Some(exch) {
        return Err(format!(
            "exchange {exch:?}, catalog {} ({want:?})",
            catalog::exchange(product.id)
        ));
    }
    Ok(())
}

impl InstrumentMap {
    pub fn add(&mut self, def: &InstrumentDefMsg) {
        self.definitions += 1;
        let day = day_of(def.ts_recv);
        let outcome = classify(def);
        // A later definition of the same id on the same day (an update) replaces the earlier
        // one; definitions are applied in file order, which is deterministic.
        self.by_day.insert((day, def.hd.instrument_id), outcome);
    }

    /// The definition for `instrument_id` on the day of `ts_recv`, if any.
    pub fn lookup(&self, ts_recv: u64, instrument_id: u32) -> Option<&DefOutcome> {
        self.by_day.get(&(day_of(ts_recv), instrument_id))
    }

    /// Every contract the definitions name, with the days they cover.
    pub fn contracts(&self) -> BTreeMap<ContractKey, Vec<CivilDate>> {
        let mut out: BTreeMap<ContractKey, Vec<CivilDate>> = BTreeMap::new();
        for ((day, _), o) in &self.by_day {
            if let DefOutcome::Contract(k) = o {
                let days = out.entry(*k).or_default();
                if days.last() != Some(day) {
                    days.push(*day);
                }
            }
        }
        out
    }

    /// Definitions for the target that disagree with the catalog.
    pub fn bad_metadata_for(&self, raw_prefix: &str) -> Vec<String> {
        self.by_day
            .values()
            .filter_map(|o| match o {
                DefOutcome::BadMetadata(m) if m.starts_with(raw_prefix) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_char;

    use dbn::record::str_to_c_chars;
    use dbn::{RecordHeader, rtype};
    use lines_feed::catalog::{self, Target};

    use super::*;
    use crate::import::raw_symbol;

    /// One root's definition as the gateway sent it (2026-11-09, the evidence file): tick and
    /// unit-of-measure quantity in 1e-9 units, display factor, unit, exchange, a contract.
    struct Evidence {
        asset: &'static str,
        raw: &'static str,
        year: u16,
        month: u8,
        tick: i64,
        display: i64,
        uom_qty: i64,
        uom: &'static str,
        exch: &'static str,
        label: &'static str,
    }

    #[allow(clippy::too_many_arguments)] // one evidence row, as the file reads
    const fn ev(
        asset: &'static str,
        raw: &'static str,
        (year, month): (u16, u8),
        tick: i64,
        display: i64,
        uom_qty: i64,
        uom: &'static str,
        exch: &'static str,
        label: &'static str,
    ) -> Evidence {
        Evidence {
            asset,
            raw,
            year,
            month,
            tick,
            display,
            uom_qty,
            uom,
            exch,
            label,
        }
    }

    /// Every catalog CME future, from the evidence file's values (its front contract, or a
    /// catalog-cycle month for roots whose front is a serial month).
    const EVIDENCE: &[Evidence] = &[
        ev(
            "ES",
            "ESZ6",
            (2026, 12),
            250_000_000,
            10_000_000,
            50_000_000_000,
            "IPNT",
            "XCME",
            "ESZ26",
        ),
        ev(
            "NQ",
            "NQZ6",
            (2026, 12),
            250_000_000,
            10_000_000,
            20_000_000_000,
            "IPNT",
            "XCME",
            "NQZ26",
        ),
        ev(
            "YM",
            "YMZ6",
            (2026, 12),
            1_000_000_000,
            1_000_000_000,
            5_000_000_000,
            "IPNT",
            "XCBT",
            "YMZ26",
        ),
        ev(
            "RTY",
            "RTYZ6",
            (2026, 12),
            100_000_000,
            10_000_000,
            50_000_000_000,
            "IPNT",
            "XCME",
            "RTYZ26",
        ),
        ev(
            "MES",
            "MESZ6",
            (2026, 12),
            250_000_000,
            10_000_000,
            5_000_000_000,
            "IPNT",
            "XCME",
            "MESZ26",
        ),
        ev(
            "MNQ",
            "MNQZ6",
            (2026, 12),
            250_000_000,
            10_000_000,
            2_000_000_000,
            "IPNT",
            "XCME",
            "MNQZ26",
        ),
        ev(
            "MYM",
            "MYMZ6",
            (2026, 12),
            1_000_000_000,
            1_000_000_000,
            500_000_000,
            "IPNT",
            "XCBT",
            "MYMZ26",
        ),
        ev(
            "M2K",
            "M2KZ6",
            (2026, 12),
            100_000_000,
            10_000_000,
            5_000_000_000,
            "IPNT",
            "XCME",
            "M2KZ26",
        ),
        ev(
            "ZN",
            "ZNZ6",
            (2026, 12),
            15_625_000,
            1_000_000_000,
            100_000_000_000_000,
            "USD",
            "XCBT",
            "ZNZ26",
        ),
        ev(
            "ZB",
            "ZBZ6",
            (2026, 12),
            31_250_000,
            1_000_000_000,
            100_000_000_000_000,
            "USD",
            "XCBT",
            "ZBZ26",
        ),
        ev(
            "ZF",
            "ZFZ6",
            (2026, 12),
            7_812_500,
            1_000_000_000,
            100_000_000_000_000,
            "USD",
            "XCBT",
            "ZFZ26",
        ),
        ev(
            "ZT",
            "ZTZ6",
            (2026, 12),
            3_906_250,
            1_000_000_000,
            200_000_000_000_000,
            "USD",
            "XCBT",
            "ZTZ26",
        ),
        ev(
            "CL",
            "CLZ6",
            (2026, 12),
            10_000_000,
            10_000_000,
            1_000_000_000_000,
            "BBL",
            "XNYM",
            "CLZ26",
        ),
        ev(
            "MCL",
            "MCLZ6",
            (2026, 12),
            10_000_000,
            10_000_000,
            100_000_000_000,
            "BBL",
            "XNYM",
            "MCLZ26",
        ),
        ev(
            "NG",
            "NGZ26",
            (2026, 12),
            1_000_000,
            1_000_000,
            10_000_000_000_000,
            "MMBTU",
            "XNYM",
            "NGZ26",
        ),
        ev(
            "GC",
            "GCZ6",
            (2026, 12),
            100_000_000,
            100_000_000,
            100_000_000_000,
            "TRYOZ",
            "XCEC",
            "GCZ26",
        ),
        ev(
            "MGC",
            "MGCZ6",
            (2026, 12),
            100_000_000,
            100_000_000,
            10_000_000_000,
            "TRYOZ",
            "XCEC",
            "MGCZ26",
        ),
        ev(
            "SI",
            "SIZ6",
            (2026, 12),
            5_000_000,
            1_000_000,
            5_000_000_000_000,
            "TRYOZ",
            "XCEC",
            "SIZ26",
        ),
        ev(
            "HG",
            "HGZ6",
            (2026, 12),
            500_000,
            100_000,
            25_000_000_000_000,
            "LBS",
            "XCEC",
            "HGZ26",
        ),
        ev(
            "6E",
            "6EZ6",
            (2026, 12),
            50_000,
            100_000,
            125_000_000_000_000,
            "EUR",
            "XCME",
            "6EZ26",
        ),
        ev(
            "6J",
            "6JZ6",
            (2026, 12),
            500,
            1_000,
            12_500_000_000_000_000,
            "JPY",
            "XCME",
            "6JZ26",
        ),
        ev(
            "6B",
            "6BZ6",
            (2026, 12),
            100_000,
            100_000,
            62_500_000_000_000,
            "GBP",
            "XCME",
            "6BZ26",
        ),
        ev(
            "ZC",
            "ZCZ6",
            (2026, 12),
            250_000_000,
            1_000_000_000,
            5_000_000_000_000,
            "BU",
            "XCBT",
            "ZCZ26",
        ),
        ev(
            "ZS",
            "ZSX6",
            (2026, 11),
            250_000_000,
            1_000_000_000,
            5_000_000_000_000,
            "BU",
            "XCBT",
            "ZSX26",
        ),
        ev(
            "ZW",
            "ZWZ6",
            (2026, 12),
            250_000_000,
            1_000_000_000,
            5_000_000_000_000,
            "BU",
            "XCBT",
            "ZWZ26",
        ),
    ];

    fn def_of(e: &Evidence) -> InstrumentDefMsg {
        InstrumentDefMsg {
            hd: RecordHeader::new::<InstrumentDefMsg>(rtype::INSTRUMENT_DEF, 1, 7, 0),
            min_price_increment: e.tick,
            display_factor: e.display,
            unit_of_measure_qty: e.uom_qty,
            unit_of_measure: str_to_c_chars(e.uom).unwrap(),
            currency: str_to_c_chars("USD").unwrap(),
            exchange: str_to_c_chars(e.exch).unwrap(),
            maturity_year: e.year,
            maturity_month: e.month,
            raw_symbol: str_to_c_chars(e.raw).unwrap(),
            asset: str_to_c_chars(e.asset).unwrap(),
            instrument_class: b'F' as c_char,
            ..Default::default()
        }
    }

    fn evidence(asset: &str) -> &'static Evidence {
        EVIDENCE.iter().find(|e| e.asset == asset).unwrap()
    }

    /// The root's evidence with another contract (raw symbol, maturity).
    fn def(asset: &str, raw: &str, year: u16, month: u8) -> InstrumentDefMsg {
        let mut d = def_of(evidence(asset));
        d.raw_symbol = str_to_c_chars(raw).unwrap();
        d.maturity_year = year;
        d.maturity_month = month;
        d
    }

    fn contract(label: &str) -> ContractKey {
        match catalog::parse_target(label) {
            Some(Target::Contract(k)) => k,
            other => panic!("{label}: {other:?}"),
        }
    }

    fn refused(o: &DefOutcome, what: &str) -> bool {
        matches!(o, DefOutcome::BadMetadata(m) if m.contains(what))
    }

    /// Every catalog CME future's definition, as the gateway sent it, maps to its contract,
    /// and its contract maps back to the same raw symbol.
    #[test]
    fn every_products_real_definition_maps_to_its_contract() {
        for e in EVIDENCE {
            let k = contract(e.label);
            assert_eq!(classify(&def_of(e)), DefOutcome::Contract(k), "{}", e.raw);
            assert_eq!(raw_symbol(k).as_deref(), Some(e.raw), "{}", e.label);
        }
        // Every catalog future on a CME Group exchange is in the evidence.
        for p in catalog::all() {
            if mic(catalog::exchange(p.id)).is_some() {
                assert!(EVIDENCE.iter().any(|e| e.asset == p.symbol), "{}", p.symbol);
            }
        }
    }

    /// Point values from the unit of measure, exactly: Treasuries and grains per 100.
    #[test]
    fn point_values_follow_the_unit_of_measure() {
        let pv = |a| {
            let (n, d) = point_value(&def_of(evidence(a))).unwrap();
            let g = (1..=n.min(d))
                .rev()
                .find(|g| n % g == 0 && d % g == 0)
                .unwrap_or(1);
            (n / g, d / g)
        };
        assert_eq!(pv("ZN"), (1000, 1));
        assert_eq!(pv("ZT"), (2000, 1));
        assert_eq!(pv("ZC"), (50, 1), "$50 per cent");
        assert_eq!(pv("MYM"), (1, 2));
        assert_eq!(pv("6J"), (12_500_000, 1));
        let mut d = def_of(evidence("CL"));
        d.unit_of_measure = str_to_c_chars("XYZ").unwrap();
        assert_eq!(point_value(&d), None, "an unknown unit has no point value");
        assert!(refused(&classify(&d), "multiplier"));
    }

    /// A wrong multiplier, currency or exchange is refused, each on its own.
    #[test]
    fn a_disagreeing_spec_is_refused() {
        let mut d = def_of(evidence("GC"));
        d.unit_of_measure_qty = 10_000_000_000; // MGC's 10 oz
        assert!(refused(&classify(&d), "multiplier"), "{:?}", classify(&d));
        let mut d = def_of(evidence("ZB"));
        d.unit_of_measure_qty = 200_000_000_000_000;
        assert!(refused(&classify(&d), "multiplier"));
        let mut d = def_of(evidence("6E"));
        d.currency = str_to_c_chars("EUR").unwrap();
        assert!(refused(&classify(&d), "currency"), "{:?}", classify(&d));
        let mut d = def_of(evidence("CL"));
        d.exchange = str_to_c_chars("XCME").unwrap();
        assert!(refused(&classify(&d), "exchange"), "{:?}", classify(&d));
        let mut d = def_of(evidence("YM"));
        d.exchange = str_to_c_chars("XCME").unwrap();
        assert!(refused(&classify(&d), "exchange"), "YM is CBOT's");
    }

    /// NG's raw symbols carry two year digits; a one-digit NG symbol, or a two-digit CL one,
    /// disagrees with the gateway's convention and is refused.
    #[test]
    fn year_digits_follow_each_roots_convention() {
        let ng = |raw| classify(&def("NG", raw, 2027, 1));
        assert_eq!(ng("NGF27"), DefOutcome::Contract(contract("NGF27")));
        assert!(refused(&ng("NGF7"), "symbol"), "{:?}", ng("NGF7"));
        let cl = classify(&def("CL", "CLF27", 2027, 1));
        assert!(refused(&cl, "symbol"), "{cl:?}");
        // A symbol for another maturity is refused too.
        let off = classify(&def("CL", "CLG7", 2027, 1));
        assert!(refused(&off, "symbol"), "{off:?}");
        // CL from 2030 on: two digits (the gateway lists CLZ9, then CLF30); MCL stays at one.
        let cl30 = classify(&def("CL", "CLF30", 2030, 1));
        assert_eq!(cl30, DefOutcome::Contract(contract("CLF30")));
        assert_eq!(raw_symbol(contract("CLF30")).as_deref(), Some("CLF30"));
        assert_eq!(raw_symbol(contract("CLZ29")).as_deref(), Some("CLZ9"));
        assert!(refused(&classify(&def("CL", "CLF0", 2030, 1)), "symbol"));
        let mcl30 = classify(&def("MCL", "MCLF0", 2030, 1));
        assert_eq!(mcl30, DefOutcome::Contract(contract("MCLF30")));
    }

    /// A definition whose tick disagrees with the catalog's is refused, whatever its display
    /// factor; the display factor alone never refuses one.
    #[test]
    fn a_disagreeing_tick_is_refused_and_the_display_factor_ignored() {
        for (asset, tick) in [("GC", 10_000_000), ("6J", 1_000), ("ZB", 15_625_000)] {
            let mut d = def_of(evidence(asset));
            d.min_price_increment = tick;
            let o = classify(&d);
            assert!(refused(&o, "tick"), "{asset}: {o:?}");
        }
        for factor in [1, 1_000, 1_000_000_000, 7] {
            let mut d = def_of(evidence("ES"));
            d.display_factor = factor;
            assert_eq!(classify(&d), DefOutcome::Contract(contract("ESZ26")));
        }
    }

    /// Serial months of roots known to list them (gold's November, FX monthlies) are not
    /// charted; a month outside any other root's cycle means the cycle is wrong: refused.
    #[test]
    fn serial_months_only_where_the_gateway_lists_them() {
        for (asset, raw) in [
            ("GC", "GCX6"),
            ("SI", "SIX6"),
            ("HG", "HGX6"),
            ("6E", "6EX6"),
            ("6J", "6JX6"),
            ("6B", "6BX6"),
        ] {
            let o = classify(&def(asset, raw, 2026, 11));
            assert!(matches!(o, DefOutcome::NotCharted(_)), "{raw}: {o:?}");
        }
        for (asset, raw) in [
            ("ES", "ESX6"),
            ("ZN", "ZNX6"),
            ("MGC", "MGCX6"),
            ("ZC", "ZCX6"),
        ] {
            let o = classify(&def(asset, raw, 2026, 11));
            assert!(refused(&o, "not a listed month"), "{raw}: {o:?}");
        }
    }
}
