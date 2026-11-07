//! Built-in product catalog: permanent ids and exact, provider-neutral specs (decisions 0002,
//! 0003, 0013).
//!
//! Each product's price unit and tick are defined here, once. Providers must convert their
//! wire values into these units and reject metadata that disagrees. Contracts are not listed:
//! a `ContractKey` is computed from a product and a delivery month.
//!
//! Provider *root* symbols live in this crate only ([`roots`]) and never cross the public
//! API above `lines-feed`; each provider derives its contract symbols from a key by rule.
//!
//! Ids are permanent: never renumber or reuse one (they match the former `InstrumentId`s).

use std::sync::OnceLock;

use lines_core::sessions::{ExchangeHours, Hours};
use lines_core::{
    ContractKey, Currency, DisplayStyle, ExerciseStyle, ExpiryRule, HolidayTable, LocalTime,
    MicroEquivalent, MonthCycle, OptionRules, OptionTick, PriceFormat, PriceScale, Product,
    ProductId, ProductKind, QtyFormat, Ratio, RollRule, SessionCalendar, SessionSpec,
};

pub const ES: ProductId = ProductId(1);
pub const NQ: ProductId = ProductId(2);
pub const ZN: ProductId = ProductId(3);
pub const MES: ProductId = ProductId(4);
pub const MNQ: ProductId = ProductId(5);
pub const BTC_USD: ProductId = ProductId(1001);
pub const ETH_USD: ProductId = ProductId(1002);
/// Kraken spot. A crypto spot market belongs to its exchange (its own book and prices), so
/// each exchange's pair is its own product; the `.KR` suffix keeps symbols unique.
pub const BTC_USD_KR: ProductId = ProductId(1003);
pub const ETH_USD_KR: ProductId = ProductId(1004);
/// Kraken Futures perpetual swaps (USD-margined "flexible" perps). A perpetual has one
/// contract that never expires or rolls, which is exactly `Delivery::Spot`'s meaning, so no
/// new delivery variant is needed (`ProductKind::Spot`, `Delivery::Spot`).
pub const BTC_PERP_KR: ProductId = ProductId(1101);
pub const ETH_PERP_KR: ProductId = ProductId(1102);
/// US-listed ETFs and their options (decision 0028). An option is a `ContractKey` on the ETF's
/// own product (`Delivery::Option`): no product per series.
pub const SPY: ProductId = ProductId(2001);
pub const QQQ: ProductId = ProductId(2002);

/// SPY and QQQ options: 100 shares, American, in the penny interval program (0.01 at every
/// premium). Providers report a chain's own increment, and theirs wins (decision 0028).
const ETF_OPTIONS: OptionRules = OptionRules {
    multiplier: 100,
    style: ExerciseStyle::American,
    tick: OptionTick::Flat(1),
};

/// A US-listed ETF: priced in cents, whole shares, with listed options.
fn etf(id: ProductId, symbol: &'static str, description: &'static str) -> Product {
    Product {
        id,
        symbol,
        description,
        price: decimal(2, 1),
        qty: QtyFormat::WHOLE,
        currency: Currency::USD,
        point_value: Ratio::new(1, 1),
        kind: ProductKind::Equity {
            options: Some(ETF_OPTIONS),
        },
        micro: None,
    }
}

fn decimal(digits: u32, tick_units: i64) -> PriceFormat {
    PriceFormat::new(
        PriceScale::decimal(digits),
        DisplayStyle::Decimal {
            digits: digits as u8,
        },
        tick_units,
    )
}

/// CME equity index futures: quarterly, expire the third Friday, roll 8 days before.
const EQUITY_INDEX: ProductKind = ProductKind::Future {
    cycle: MonthCycle::QUARTERLY,
    expiry: ExpiryRule::ThirdFriday,
    roll: RollRule::DaysBeforeExpiry(8),
};

fn index_future(
    id: ProductId,
    symbol: &'static str,
    description: &'static str,
    point_value: i64,
    micro: ProductId,
    per_contract: u32,
) -> Product {
    Product {
        id,
        symbol,
        description,
        // Unit 0.01 (finer than the 0.25 tick) so off-tick values such as some settlements
        // remain representable.
        price: decimal(2, 25),
        qty: QtyFormat::WHOLE,
        currency: Currency::USD,
        point_value: Ratio::new(point_value, 1),
        kind: EQUITY_INDEX,
        micro: Some(MicroEquivalent {
            micro,
            per_contract,
        }),
    }
}

fn spot(id: ProductId, symbol: &'static str, description: &'static str) -> Product {
    crypto(id, symbol, description, decimal(2, 1), 8)
}

/// A crypto product quoted in USD with a decimal price grid and fractional quantity.
fn crypto(
    id: ProductId,
    symbol: &'static str,
    description: &'static str,
    price: PriceFormat,
    qty_decimals: u8,
) -> Product {
    Product {
        id,
        symbol,
        description,
        price,
        qty: QtyFormat {
            decimals: qty_decimals,
        },
        currency: Currency::USD,
        point_value: Ratio::new(1, 1),
        kind: ProductKind::Spot,
        micro: None,
    }
}

pub fn all() -> [Product; 13] {
    [
        index_future(ES, "ES", "E-mini S&P 500", 50, MES, 10),
        index_future(NQ, "NQ", "E-mini Nasdaq-100", 20, MNQ, 10),
        index_future(MES, "MES", "Micro E-mini S&P 500", 5, MES, 1),
        index_future(MNQ, "MNQ", "Micro E-mini Nasdaq-100", 2, MNQ, 1),
        Product {
            id: ZN,
            symbol: "ZN",
            description: "10-Year T-Note",
            // Tick is 1/64 (half a 32nd); unit 1/128 leaves room for finer quotes.
            price: PriceFormat::new(
                PriceScale::new(1, 128),
                DisplayStyle::SubFraction {
                    denominator: 32,
                    sub: 4,
                },
                2,
            ),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
            point_value: Ratio::new(1000, 1),
            kind: ProductKind::Future {
                cycle: MonthCycle::QUARTERLY,
                expiry: ExpiryRule::SeventhBusinessDayBeforeLastBusinessDay,
                // Physically delivered: traders roll before first notice day.
                roll: RollRule::DaysBeforeFirstNotice(2),
            },
            micro: None,
        },
        spot(BTC_USD, "BTC/USD", "Bitcoin / US Dollar (spot)"),
        spot(ETH_USD, "ETH/USD", "Ether / US Dollar (spot)"),
        // Kraken: tick and lot size match Kraken's AssetPairs / instruments metadata, which
        // the sessions check on every connect (mismatch = fatal BadMetadata).
        crypto(
            BTC_USD_KR,
            "BTC/USD.KR",
            "Bitcoin / US Dollar (spot)",
            decimal(1, 1),
            8,
        ),
        crypto(
            ETH_USD_KR,
            "ETH/USD.KR",
            "Ether / US Dollar (spot)",
            decimal(2, 1),
            8,
        ),
        crypto(
            BTC_PERP_KR,
            "BTC-PERP.KR",
            "Bitcoin perpetual (USD)",
            decimal(0, 1),
            4,
        ),
        crypto(
            ETH_PERP_KR,
            "ETH-PERP.KR",
            "Ether perpetual (USD)",
            decimal(1, 1),
            3,
        ),
        etf(SPY, "SPY", "SPDR S&P 500 ETF"),
        etf(QQQ, "QQQ", "Invesco QQQ (Nasdaq-100 ETF)"),
    ]
}

pub fn get(id: ProductId) -> Option<Product> {
    all().into_iter().find(|p| p.id == id)
}

/// Case-insensitive lookup by product symbol; `BTC-USD` is also accepted for `BTC/USD`.
pub fn by_symbol(symbol: &str) -> Option<Product> {
    let symbol = symbol.trim();
    let find = |want: &str| {
        all()
            .into_iter()
            .find(|p| p.symbol.eq_ignore_ascii_case(want))
    };
    find(symbol).or_else(|| find(&symbol.replace('-', "/")))
}

/// Where a product trades, for display and search (e.g. the symbol picker). An exchange name,
/// not a vendor symbol.
pub fn exchange(id: ProductId) -> &'static str {
    match id {
        ES | NQ | MES | MNQ => "CME",
        ZN => "CBOT",
        BTC_USD | ETH_USD => "Coinbase",
        BTC_USD_KR | ETH_USD_KR => "Kraken",
        BTC_PERP_KR | ETH_PERP_KR => "Kraken Futures",
        SPY => "NYSE Arca",
        QQQ => "Nasdaq",
        _ => "",
    }
}

/// Exchange holidays and early closes: versioned data, not code (decision 0017). Sources and
/// the verification status of every date are in the file.
pub const HOLIDAYS: &str = include_str!("../data/exchange-holidays.txt");

/// CME Globex hours shared by the CME equity index and CBOT Treasury futures: Sunday–Friday
/// 17:00–16:00 Central, with the daily 16:00–17:00 halt. The session opening at 17:00 belongs
/// to the next trade date.
const fn globex(rth_open: LocalTime, rth_close: LocalTime) -> Hours {
    Hours::Exchange(ExchangeHours {
        eth_open: LocalTime::hm(17, 0),
        eth_close: LocalTime::hm(16, 0),
        rth: Some((rth_open, rth_close)),
    })
}

/// A product's trading hours (decision 0017). Every catalog product has one.
pub fn session_spec(id: ProductId) -> Option<SessionSpec> {
    match id {
        // RTH = the cash equity session, 08:30–15:00 CT.
        ES | NQ | MES | MNQ => Some(SessionSpec {
            time_zone: "America/Chicago",
            hours: globex(LocalTime::hm(8, 30), LocalTime::hm(15, 0)),
            holidays: Some("cme-equity-index"),
        }),
        // RTH = the former open-outcry hours, 07:20–14:00 CT (14:00 is the settlement time).
        ZN => Some(SessionSpec {
            time_zone: "America/Chicago",
            hours: globex(LocalTime::hm(7, 20), LocalTime::hm(14, 0)),
            holidays: Some("cbot-treasury"),
        }),
        // Coinbase and Kraken spot and Kraken perpetuals trade 24/7; the session day is the
        // UTC day.
        BTC_USD | ETH_USD | BTC_USD_KR | ETH_USD_KR | BTC_PERP_KR | ETH_PERP_KR => {
            Some(SessionSpec::continuous_utc(LocalTime::MIDNIGHT))
        }
        // US equities: extended hours 04:00–20:00 ET, the regular session 09:30–16:00, on the
        // trade date itself (same-day hours). Their options trade in RTH only (decision 0028).
        SPY | QQQ => Some(SessionSpec {
            time_zone: "America/New_York",
            hours: Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(4, 0),
                eth_close: LocalTime::hm(20, 0),
                rth: Some((LocalTime::hm(9, 30), LocalTime::hm(16, 0))),
            }),
            holidays: Some("us-equity"),
        }),
        _ => None,
    }
}

/// The shipped holiday table, parsed and validated once.
pub fn holiday_table() -> &'static HolidayTable {
    static TABLE: OnceLock<HolidayTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        // The embedded file is validated by `catalog::tests`; failing here is a build defect.
        HolidayTable::parse(HOLIDAYS).unwrap_or_else(|e| panic!("embedded {e}"))
    })
}

/// A product's session calendar, built once. Queries on it are pure and allocation-free.
pub fn sessions(id: ProductId) -> Option<&'static SessionCalendar> {
    static CALENDARS: OnceLock<Vec<(ProductId, SessionCalendar)>> = OnceLock::new();
    CALENDARS
        .get_or_init(|| {
            all()
                .iter()
                .filter_map(|p| {
                    let spec = session_spec(p.id)?;
                    let cal = SessionCalendar::new(spec, Some(holiday_table()))
                        .unwrap_or_else(|e| panic!("{}: {e}", p.symbol));
                    Some((p.id, cal))
                })
                .collect()
        })
        .iter()
        .find(|(p, _)| *p == id)
        .map(|(_, c)| c)
}

/// What a user asked for: a product (resolved to its front month) or one contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Product(ProductId),
    Contract(ContractKey),
}

/// Parse a product symbol (`ES`, `BTC/USD`) or a canonical contract label (`ESZ26`,
/// `ESZ2026`). Longest product symbol wins, so `MESZ26` is MES, not ES.
pub fn parse_target(s: &str) -> Option<Target> {
    let s = s.trim().to_ascii_uppercase();
    if let Some(p) = by_symbol(&s) {
        return Some(Target::Product(p.id));
    }
    let mut products = all();
    products.sort_by_key(|p| std::cmp::Reverse(p.symbol.len()));
    products
        .iter()
        .find_map(|p| ContractKey::parse_label(p, &s).map(Target::Contract))
}

/// Provider root symbols (decision 0013). Crate-private: vendor symbols never leave
/// `lines-feed` (decision 0003). Rithmic roots belong to the private connector (0014).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roots {
    pub cqg: Option<&'static str>,
    pub coinbase: Option<&'static str>,
}

pub(crate) fn roots(id: ProductId) -> Roots {
    match id {
        // CQG's own roots; only EP has been exercised (via the mock). MES/MNQ unmapped until
        // checked against CQG symbol resolution.
        ES => Roots {
            cqg: Some("EP"),
            ..Default::default()
        },
        NQ => Roots {
            cqg: Some("ENQ"),
            ..Default::default()
        },
        ZN => Roots {
            cqg: Some("TYA"),
            ..Default::default()
        },
        BTC_USD => Roots {
            coinbase: Some("BTC-USD"),
            ..Default::default()
        },
        ETH_USD => Roots {
            coinbase: Some("ETH-USD"),
            ..Default::default()
        },
        _ => Roots::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{CivilDate, HolidayData, Month, Qty, SessionKind, UtcMillis};

    #[test]
    fn every_product_has_an_exchange() {
        for p in all() {
            assert!(!exchange(p.id).is_empty(), "{}", p.symbol);
        }
    }

    #[test]
    fn ids_and_symbols_are_unique() {
        let all = all();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.id, b.id);
                assert_ne!(a.symbol, b.symbol);
            }
        }
    }

    #[test]
    fn lookup_and_targets() {
        assert_eq!(by_symbol("es").unwrap().id, ES);
        assert_eq!(by_symbol("BTC-USD").unwrap().id, BTC_USD);
        assert_eq!(
            by_symbol("btc-perp.kr").unwrap().id,
            BTC_PERP_KR,
            "exact match first"
        );
        assert_eq!(by_symbol("ETH/USD.KR").unwrap().id, ETH_USD_KR);
        assert_eq!(
            parse_target("eth-perp.kr"),
            Some(Target::Product(ETH_PERP_KR))
        );
        assert_eq!(get(ZN).unwrap().symbol, "ZN");
        assert!(
            by_symbol("EP").is_none(),
            "vendor symbols are not catalog symbols"
        );
        assert_eq!(parse_target("es"), Some(Target::Product(ES)));
        let z = Month::new(12).unwrap();
        assert_eq!(
            parse_target("ESZ26"),
            Some(Target::Contract(ContractKey::monthly(ES, 2026, z)))
        );
        assert_eq!(
            parse_target("MESZ26"),
            Some(Target::Contract(ContractKey::monthly(MES, 2026, z)))
        );
        assert_eq!(
            parse_target("MNQH2027"),
            Some(Target::Contract(ContractKey::monthly(
                MNQ,
                2027,
                Month::new(3).unwrap()
            )))
        );
        assert_eq!(parse_target("ESZ6"), None, "1-digit years are ambiguous");
        assert_eq!(parse_target("CLZ26"), None);
    }

    #[test]
    fn micro_mini_equivalence() {
        // Prop-firm caps are evaluated in micro units (decision 0013).
        assert_eq!(
            get(NQ).unwrap().micro_equivalent(Qty(2)),
            Some((MNQ, Qty(20)))
        );
        assert_eq!(
            get(MNQ).unwrap().micro_equivalent(Qty(7)),
            Some((MNQ, Qty(7)))
        );
        assert_eq!(
            get(ES).unwrap().micro_equivalent(Qty(1)),
            Some((MES, Qty(10)))
        );
        assert_eq!(get(ZN).unwrap().micro_equivalent(Qty(1)), None);
        // Point values: NQ = 10 × MNQ, ES = 10 × MES.
        let pv = |id| get(id).unwrap().point_value.num;
        assert_eq!((pv(NQ), pv(MNQ), pv(ES), pv(MES)), (20, 2, 50, 5));
    }

    fn t(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    fn d(y: u16, m: u8, day: u8) -> CivilDate {
        CivilDate::new(y, m, day).unwrap()
    }

    #[test]
    fn shipped_holiday_table_is_valid() {
        let table = HolidayTable::parse(HOLIDAYS).unwrap();
        assert_eq!(&table, holiday_table());
        for cal in &table.calendars {
            // Current and next year (decision 0017).
            assert!(
                cal.coverage.0 <= 2026 && cal.coverage.1 >= 2027,
                "{}",
                cal.name
            );
            assert!(!cal.sources.is_empty());
            assert!(!cal.days.is_empty());
        }
        // Every calendar is used by a product, and every product's calendar exists.
        for cal in &table.calendars {
            assert!(
                all()
                    .iter()
                    .any(|p| session_spec(p.id).unwrap().holidays == Some(&cal.name)),
                "unused calendar {}",
                cal.name
            );
        }
    }

    #[test]
    fn every_product_has_a_session_calendar() {
        for p in all() {
            let cal = sessions(p.id).unwrap_or_else(|| panic!("{}", p.symbol));
            assert_eq!(cal.has_rth(), !p.is_spot(), "{}", p.symbol);
        }
        assert!(sessions(ProductId(999)).is_none());
        // Micro and mini share hours and holidays.
        for (a, b) in [(ES, MES), (NQ, MNQ), (ES, NQ)] {
            assert_eq!(session_spec(a), session_spec(b));
        }
    }

    #[test]
    fn index_futures_sessions() {
        let es = sessions(ES).unwrap();
        // Sunday 17:00 CST opens Monday's session.
        let s = es
            .session_at(t("2026-11-08T23:00:00Z"), SessionKind::Eth)
            .unwrap();
        assert_eq!(s.trade_date, d(2026, 11, 9));
        assert_eq!(s.close, t("2026-11-09T22:00:00Z"));
        assert_eq!(
            s.holidays,
            HolidayData::Unverified,
            "table not yet verified"
        );
        let r = es.session(d(2026, 11, 9), SessionKind::Rth).unwrap();
        assert_eq!(
            (r.open, r.close),
            (t("2026-11-09T14:30:00Z"), t("2026-11-09T21:00:00Z"))
        );
        // Thanksgiving 2026: Thursday halts at 12:00 CT and belongs to Friday, which closes 12:15.
        let f = es.session(d(2026, 11, 27), SessionKind::Eth).unwrap();
        assert_eq!(f.open, t("2026-11-25T23:00:00Z"));
        assert_eq!(
            f.halt,
            Some((t("2026-11-26T18:00:00Z"), t("2026-11-26T23:00:00Z")))
        );
        assert_eq!(f.close, t("2026-11-27T18:15:00Z"));
        // Christmas 2026: early close Thursday, closed Friday, reopen Sunday 17:00.
        assert_eq!(
            es.next_open(t("2026-12-24T18:15:00Z")),
            Some(t("2026-12-27T23:00:00Z"))
        );
        // Good Friday 2026: an abbreviated trade date with no RTH for equities; ZN keeps a short RTH.
        assert!(es.session(d(2026, 4, 3), SessionKind::Rth).is_none());
        let zn = sessions(ZN).unwrap();
        let g = zn.session(d(2026, 4, 3), SessionKind::Rth).unwrap();
        assert_eq!(
            (g.open, g.close),
            (t("2026-04-03T12:20:00Z"), t("2026-04-03T15:15:00Z"))
        );
        // Outside coverage: regular, and flagged.
        assert_eq!(
            es.session(d(2025, 12, 25), SessionKind::Eth)
                .unwrap()
                .holidays,
            HolidayData::NotCovered
        );
    }

    /// Decision 0028: SPY and QQQ, same-day NYSE hours, the us-equity holidays, options.
    #[test]
    fn us_equities() {
        for id in [SPY, QQQ] {
            let p = get(id).unwrap();
            assert!(p.is_equity() && !p.is_spot());
            let o = p.options().unwrap();
            assert_eq!(
                (o.multiplier, o.style, o.tick),
                (100, ExerciseStyle::American, OptionTick::Flat(1))
            );
            assert_eq!(p.price.tick, 1, "0.01");
            assert_eq!(parse_target(p.symbol), Some(Target::Product(id)));
            assert_eq!(
                p.contracts_from(d(2026, 11, 9)).next(),
                Some(ContractKey::equity(id))
            );
        }
        assert_eq!(session_spec(SPY), session_spec(QQQ));
        let spy = sessions(SPY).unwrap();
        // Monday 2026-11-09, EST: 04:00-20:00, RTH 09:30-16:00.
        let s = spy.session(d(2026, 11, 9), SessionKind::Eth).unwrap();
        assert_eq!(
            (s.open, s.close),
            (t("2026-11-09T09:00:00Z"), t("2026-11-10T01:00:00Z"))
        );
        let r = spy.session(d(2026, 11, 9), SessionKind::Rth).unwrap();
        assert_eq!(
            (r.open, r.close),
            (t("2026-11-09T14:30:00Z"), t("2026-11-09T21:00:00Z"))
        );
        assert_eq!(r.holidays, HolidayData::Unverified);
        // EDT in the summer: RTH 13:30-20:00 UTC.
        let r = spy.session(d(2026, 7, 6), SessionKind::Rth).unwrap();
        assert_eq!(r.open, t("2026-07-06T13:30:00Z"));
        // Holidays: closed, and the early close at 13:00 ET.
        for closed in [d(2026, 11, 26), d(2026, 12, 25), d(2027, 3, 26)] {
            assert!(!spy.is_trade_date(closed), "{closed}");
        }
        let f = spy.session(d(2026, 11, 27), SessionKind::Rth).unwrap();
        assert!(f.early_close && f.close == t("2026-11-27T18:00:00Z"));
        assert!(!spy.is_open(t("2026-11-10T02:00:00Z")), "overnight");
        assert_eq!(exchange(SPY), "NYSE Arca");
        assert_eq!(exchange(QQQ), "Nasdaq");
    }

    #[test]
    fn crypto_sessions_are_utc_days() {
        let btc = sessions(BTC_USD).unwrap();
        let s = btc
            .session_at(t("2026-12-25T13:00:00Z"), SessionKind::Eth)
            .unwrap();
        assert_eq!(s.trade_date, d(2026, 12, 25));
        assert_eq!(
            (s.open, s.close),
            (t("2026-12-25T00:00:00Z"), t("2026-12-26T00:00:00Z"))
        );
        assert!(btc.is_open(t("2026-12-25T13:00:00Z")));
        assert_eq!(btc.next_close(t("2026-12-25T13:00:00Z")), None);
    }
}
