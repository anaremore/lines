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
// CME Group futures added 2026-11-09. Specs and their sources: docs/catalog-sources.md.
pub const YM: ProductId = ProductId(6);
pub const RTY: ProductId = ProductId(7);
pub const MYM: ProductId = ProductId(8);
pub const M2K: ProductId = ProductId(9);
pub const ZB: ProductId = ProductId(10);
pub const ZF: ProductId = ProductId(11);
pub const ZT: ProductId = ProductId(12);
pub const CL: ProductId = ProductId(13);
pub const MCL: ProductId = ProductId(14);
pub const NG: ProductId = ProductId(15);
pub const GC: ProductId = ProductId(16);
pub const MGC: ProductId = ProductId(17);
pub const SI: ProductId = ProductId(18);
pub const HG: ProductId = ProductId(19);
/// 6E.
pub const EURO_FX: ProductId = ProductId(20);
/// 6J.
pub const JAPANESE_YEN: ProductId = ProductId(21);
/// 6B.
pub const BRITISH_POUND: ProductId = ProductId(22);
pub const ZC: ProductId = ProductId(23);
pub const ZS: ProductId = ProductId(24);
pub const ZW: ProductId = ProductId(25);
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

/// A CME Group future. `point_value` is USD per 1.0 of price per contract (the multiplier):
/// tick value = tick × scale × point value, exactly. Sources: docs/catalog-sources.md.
fn future(
    id: ProductId,
    symbol: &'static str,
    description: &'static str,
    price: PriceFormat,
    point_value: Ratio,
    kind: ProductKind,
    micro: Option<(ProductId, u32)>,
) -> Product {
    Product {
        id,
        symbol,
        description,
        price,
        qty: QtyFormat::WHOLE,
        currency: Currency::USD,
        point_value,
        kind,
        micro: micro.map(|(micro, per_contract)| MicroEquivalent {
            micro,
            per_contract,
        }),
    }
}

fn index_future(
    id: ProductId,
    symbol: &'static str,
    description: &'static str,
    point_value: i64,
    micro: ProductId,
    per_contract: u32,
) -> Product {
    // Unit 0.01 (finer than the 0.25 tick) so off-tick values such as some settlements
    // remain representable.
    future(
        id,
        symbol,
        description,
        decimal(2, 25),
        Ratio::new(point_value, 1),
        EQUITY_INDEX,
        Some((micro, per_contract)),
    )
}

/// CBOT Treasury futures: quarterly and physically delivered, so traders roll before first
/// notice day (the last business day of the month before the contract month).
const fn treasury(expiry: ExpiryRule) -> ProductKind {
    ProductKind::Future {
        cycle: MonthCycle::QUARTERLY,
        expiry,
        roll: RollRule::DaysBeforeFirstNotice(2),
    }
}

/// Treasury prices: points and 32nds, in units of `1/den` of a point.
fn treasury_price(den: i64, style: DisplayStyle, tick: i64) -> PriceFormat {
    PriceFormat::new(PriceScale::new(1, den), style, tick)
}

/// NYMEX energy: monthly, and trading ends in the month before the contract month, so the
/// calendar roll is a few days before the last trade date.
const fn energy(expiry: ExpiryRule) -> ProductKind {
    ProductKind::Future {
        cycle: MonthCycle::MONTHLY,
        expiry,
        roll: RollRule::DaysBeforeExpiry(5),
    }
}

/// COMEX metals: the active (most traded) months only. Trading ends the third last business
/// day of the contract month and delivery runs through it, so traders roll before first
/// notice day.
const fn metal(cycle: MonthCycle) -> ProductKind {
    ProductKind::Future {
        cycle,
        expiry: ExpiryRule::NthLastBusinessDay(3),
        roll: RollRule::DaysBeforeFirstNotice(2),
    }
}

/// CME FX futures: the quarterly months (serial months aren't modelled). Trading ends two
/// business days before the third Wednesday.
const FX: ProductKind = ProductKind::Future {
    cycle: MonthCycle::QUARTERLY,
    expiry: ExpiryRule::BusinessDaysBeforeThirdWednesday(2),
    roll: RollRule::DaysBeforeExpiry(7),
};

/// CBOT grains: trading ends the business day before the 15th of the contract month; physical
/// delivery, so traders roll before first notice day.
const fn grain(cycle: MonthCycle) -> ProductKind {
    ProductKind::Future {
        cycle,
        expiry: ExpiryRule::BusinessDayBefore15th,
        roll: RollRule::DaysBeforeFirstNotice(2),
    }
}

/// Grain prices: cents per bushel in eighths (`432'2` = 432 2/8 cents), tick 1/4 cent. One
/// point (1 cent) is $50 on 5,000 bushels.
fn grain_price() -> PriceFormat {
    PriceFormat::new(
        PriceScale::new(1, 8),
        DisplayStyle::Fraction { denominator: 8 },
        2,
    )
}

const GOLD_MONTHS: MonthCycle = MonthCycle::from_codes("GJMQVZ");
const SILVER_COPPER_MONTHS: MonthCycle = MonthCycle::from_codes("HKNUZ");
const CORN_WHEAT_MONTHS: MonthCycle = MonthCycle::from_codes("HKNUZ");
const SOYBEAN_MONTHS: MonthCycle = MonthCycle::from_codes("FHKNQUX");

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

/// Every product, in [`Group`] order (the picker lists them so).
pub fn all() -> [Product; 33] {
    let usd = |n| Ratio::new(n, 1);
    [
        // Equity index.
        index_future(ES, "ES", "E-mini S&P 500", 50, MES, 10),
        index_future(NQ, "NQ", "E-mini Nasdaq-100", 20, MNQ, 10),
        future(
            YM,
            "YM",
            "E-mini Dow ($5)",
            decimal(0, 1),
            usd(5),
            EQUITY_INDEX,
            Some((MYM, 10)),
        ),
        future(
            RTY,
            "RTY",
            "E-mini Russell 2000",
            decimal(2, 10),
            usd(50),
            EQUITY_INDEX,
            Some((M2K, 10)),
        ),
        index_future(MES, "MES", "Micro E-mini S&P 500", 5, MES, 1),
        index_future(MNQ, "MNQ", "Micro E-mini Nasdaq-100", 2, MNQ, 1),
        future(
            MYM,
            "MYM",
            "Micro E-mini Dow",
            decimal(0, 1),
            Ratio::new(1, 2),
            EQUITY_INDEX,
            Some((MYM, 1)),
        ),
        future(
            M2K,
            "M2K",
            "Micro E-mini Russell 2000",
            decimal(2, 10),
            usd(5),
            EQUITY_INDEX,
            Some((M2K, 1)),
        ),
        // Rates.
        future(
            ZT,
            "ZT",
            "2-Year T-Note",
            // Tick 1/8 of a 32nd = unit 1/256.
            treasury_price(
                256,
                DisplayStyle::SubFraction {
                    denominator: 32,
                    sub: 8,
                },
                1,
            ),
            usd(2000),
            treasury(ExpiryRule::NthLastBusinessDay(1)),
            None,
        ),
        future(
            ZF,
            "ZF",
            "5-Year T-Note",
            // Tick 1/4 of a 32nd; unit 1/256 keeps eighth-32nd spread prices exact.
            treasury_price(
                256,
                DisplayStyle::SubFraction {
                    denominator: 32,
                    sub: 4,
                },
                2,
            ),
            usd(1000),
            treasury(ExpiryRule::NthLastBusinessDay(1)),
            None,
        ),
        future(
            ZN,
            "ZN",
            "10-Year T-Note",
            // Tick is 1/64 (half a 32nd); unit 1/128 leaves room for finer quotes.
            treasury_price(
                128,
                DisplayStyle::SubFraction {
                    denominator: 32,
                    sub: 4,
                },
                2,
            ),
            usd(1000),
            treasury(ExpiryRule::SeventhBusinessDayBeforeLastBusinessDay),
            None,
        ),
        future(
            ZB,
            "ZB",
            "30-Year T-Bond",
            // Tick 1/32; unit 1/128 as ZN's (a finer value shows truncated to its 32nd).
            treasury_price(128, DisplayStyle::Fraction { denominator: 32 }, 4),
            usd(1000),
            treasury(ExpiryRule::SeventhBusinessDayBeforeLastBusinessDay),
            None,
        ),
        // Energy.
        future(
            CL,
            "CL",
            "Crude Oil (WTI)",
            decimal(2, 1),
            usd(1000),
            energy(ExpiryRule::BusinessDaysBefore25thOfPriorMonth(3)),
            Some((MCL, 10)),
        ),
        future(
            MCL,
            "MCL",
            "Micro WTI Crude Oil",
            decimal(2, 1),
            usd(100),
            energy(ExpiryRule::BusinessDaysBefore25thOfPriorMonth(4)),
            Some((MCL, 1)),
        ),
        future(
            NG,
            "NG",
            "Henry Hub Natural Gas",
            decimal(3, 1),
            usd(10_000),
            energy(ExpiryRule::NthLastBusinessDayOfPriorMonth(3)),
            None,
        ),
        // Metals.
        future(
            GC,
            "GC",
            "Gold",
            decimal(1, 1),
            usd(100),
            metal(GOLD_MONTHS),
            Some((MGC, 10)),
        ),
        future(
            MGC,
            "MGC",
            "Micro Gold",
            decimal(1, 1),
            usd(10),
            metal(GOLD_MONTHS),
            Some((MGC, 1)),
        ),
        future(
            SI,
            "SI",
            "Silver",
            // Tick 0.005; unit 0.001 (settlement and spread prices).
            decimal(3, 5),
            usd(5000),
            metal(SILVER_COPPER_MONTHS),
            None,
        ),
        future(
            HG,
            "HG",
            "Copper",
            decimal(4, 5),
            usd(25_000),
            metal(SILVER_COPPER_MONTHS),
            None,
        ),
        // FX: USD per unit of the foreign currency.
        future(
            EURO_FX,
            "6E",
            "Euro FX",
            decimal(5, 5),
            usd(125_000),
            FX,
            None,
        ),
        future(
            JAPANESE_YEN,
            "6J",
            "Japanese Yen",
            decimal(7, 5),
            usd(12_500_000),
            FX,
            None,
        ),
        future(
            BRITISH_POUND,
            "6B",
            "British Pound",
            decimal(4, 1),
            usd(62_500),
            FX,
            None,
        ),
        // Agriculture: cents per bushel.
        future(
            ZC,
            "ZC",
            "Corn",
            grain_price(),
            usd(50),
            grain(CORN_WHEAT_MONTHS),
            None,
        ),
        future(
            ZS,
            "ZS",
            "Soybeans",
            grain_price(),
            usd(50),
            grain(SOYBEAN_MONTHS),
            None,
        ),
        future(
            ZW,
            "ZW",
            "Chicago SRW Wheat",
            grain_price(),
            usd(50),
            grain(CORN_WHEAT_MONTHS),
            None,
        ),
        // Crypto.
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
        // ETFs.
        etf(SPY, "SPY", "SPDR S&P 500 ETF"),
        etf(QQQ, "QQQ", "Invesco QQQ (Nasdaq-100 ETF)"),
    ]
}

/// How the picker groups products: by market, not by source.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    EquityIndex,
    Rates,
    Energy,
    Metals,
    Fx,
    Agriculture,
    Crypto,
    Etfs,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::EquityIndex => "Equity index",
            Group::Rates => "Rates",
            Group::Energy => "Energy",
            Group::Metals => "Metals",
            Group::Fx => "FX",
            Group::Agriculture => "Agriculture",
            Group::Crypto => "Crypto",
            Group::Etfs => "ETFs",
        }
    }
}

/// A product's picker group. `None` for an id not in the catalog.
pub fn group(id: ProductId) -> Option<Group> {
    Some(match id {
        ES | NQ | YM | RTY | MES | MNQ | MYM | M2K => Group::EquityIndex,
        ZT | ZF | ZN | ZB => Group::Rates,
        CL | MCL | NG => Group::Energy,
        GC | MGC | SI | HG => Group::Metals,
        EURO_FX | JAPANESE_YEN | BRITISH_POUND => Group::Fx,
        ZC | ZS | ZW => Group::Agriculture,
        BTC_USD | ETH_USD | BTC_USD_KR | ETH_USD_KR | BTC_PERP_KR | ETH_PERP_KR => Group::Crypto,
        SPY | QQQ => Group::Etfs,
        _ => return None,
    })
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
        ES | NQ | MES | MNQ | RTY | M2K | EURO_FX | JAPANESE_YEN | BRITISH_POUND => "CME",
        YM | MYM | ZN | ZB | ZF | ZT | ZC | ZS | ZW => "CBOT",
        CL | MCL | NG => "NYMEX",
        GC | MGC | SI | HG => "COMEX",
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

/// CME Globex hours shared by equity index, Treasury, energy, metals and FX futures:
/// Sunday–Friday 17:00–16:00 Central, with the daily 16:00–17:00 halt. The session opening at
/// 17:00 belongs to the next trade date.
const fn globex(rth_open: LocalTime, rth_close: LocalTime) -> Hours {
    Hours::Exchange(ExchangeHours {
        eth_open: LocalTime::hm(17, 0),
        eth_close: LocalTime::hm(16, 0),
        rth: Some((rth_open, rth_close)),
    })
}

const fn chicago(hours: Hours, holidays: &'static str) -> SessionSpec {
    SessionSpec {
        time_zone: "America/Chicago",
        hours,
        holidays: Some(holidays),
    }
}

/// A product's trading hours (decision 0017). Every catalog product has one.
pub fn session_spec(id: ProductId) -> Option<SessionSpec> {
    match id {
        // RTH = the cash equity session, 08:30–15:00 CT.
        ES | NQ | MES | MNQ | YM | RTY | MYM | M2K => Some(chicago(
            globex(LocalTime::hm(8, 30), LocalTime::hm(15, 0)),
            "cme-equity-index",
        )),
        // RTH = the former open-outcry hours, 07:20–14:00 CT (14:00 is the settlement time).
        ZT | ZF | ZN | ZB => Some(chicago(
            globex(LocalTime::hm(7, 20), LocalTime::hm(14, 0)),
            "cbot-treasury",
        )),
        // RTH = the former NYMEX floor hours, 08:00–13:30 CT (settlement 13:28–13:30).
        CL | MCL | NG => Some(chicago(
            globex(LocalTime::hm(8, 0), LocalTime::hm(13, 30)),
            "cme-energy-metals",
        )),
        // RTH = the former COMEX gold floor hours, 07:20–12:30 CT; one window for every metal
        // (docs/known-gaps.md).
        GC | MGC | SI | HG => Some(chicago(
            globex(LocalTime::hm(7, 20), LocalTime::hm(12, 30)),
            "cme-energy-metals",
        )),
        // RTH = the former IMM floor hours, 07:20–14:00 CT.
        EURO_FX | JAPANESE_YEN | BRITISH_POUND => Some(chicago(
            globex(LocalTime::hm(7, 20), LocalTime::hm(14, 0)),
            "cme-fx",
        )),
        // CBOT grains: Sunday–Friday 19:00–07:45 and Monday–Friday 08:30–13:20 CT. The session
        // of trade date D opens at 19:00 the evening before; RTH is the day session. The
        // 07:45–08:30 pause isn't modelled (docs/known-gaps.md).
        ZC | ZS | ZW => Some(chicago(
            Hours::Exchange(ExchangeHours {
                eth_open: LocalTime::hm(19, 0),
                eth_close: LocalTime::hm(13, 20),
                rth: Some((LocalTime::hm(8, 30), LocalTime::hm(13, 20))),
            }),
            "cbot-grains",
        )),
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
    use lines_core::{CivilDate, HolidayData, Month, Price, Qty, SessionKind, UtcMillis};

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
        assert_eq!(parse_target("XXZ26"), None, "not in the catalog");
        assert_eq!(parse_target("CLZ6"), None, "1-digit years are ambiguous");
        assert_eq!(
            parse_target("GCX26"),
            None,
            "gold's serial months aren't listed"
        );
    }

    /// The CME Group futures added 2026-11-09 (docs/catalog-sources.md): tick value = tick ×
    /// multiplier exactly, as `(num, den)` dollars.
    #[test]
    fn cme_tick_values_are_exact() {
        let cases: &[(ProductId, &str, (i128, i128))] = &[
            (ES, "CME", (25, 2)),
            (NQ, "CME", (5, 1)),
            (YM, "CBOT", (5, 1)),
            (RTY, "CME", (5, 1)),
            (MES, "CME", (5, 4)),
            (MNQ, "CME", (1, 2)),
            (MYM, "CBOT", (1, 2)),
            (M2K, "CME", (1, 2)),
            (ZT, "CBOT", (125, 16)), // $7.8125
            (ZF, "CBOT", (125, 16)), // $7.8125
            (ZN, "CBOT", (125, 8)),  // $15.625
            (ZB, "CBOT", (125, 4)),  // $31.25
            (CL, "NYMEX", (10, 1)),
            (MCL, "NYMEX", (1, 1)),
            (NG, "NYMEX", (10, 1)),
            (GC, "COMEX", (10, 1)),
            (MGC, "COMEX", (1, 1)),
            (SI, "COMEX", (25, 1)),
            (HG, "COMEX", (25, 2)),
            (EURO_FX, "CME", (25, 4)),       // $6.25
            (JAPANESE_YEN, "CME", (25, 4)),  // $6.25
            (BRITISH_POUND, "CME", (25, 4)), // $6.25
            (ZC, "CBOT", (25, 2)),
            (ZS, "CBOT", (25, 2)),
            (ZW, "CBOT", (25, 2)),
        ];
        for &(id, exch, (num, den)) in cases {
            let p = get(id).unwrap();
            let s = p.price.scale;
            let (n, d) = (
                p.price.tick as i128 * s.num() as i128 * p.point_value.num as i128,
                s.den() as i128 * p.point_value.den as i128,
            );
            assert_eq!(n * den, num * d, "{}: tick value {n}/{d}", p.symbol);
            assert_eq!(exchange(id), exch, "{}", p.symbol);
            assert!(!p.is_spot() && !p.is_equity(), "{}", p.symbol);
        }
        // Every future in the catalog is in the table.
        let futures = all()
            .iter()
            .filter(|p| !p.is_spot() && !p.is_equity())
            .count();
        assert_eq!(futures, cases.len());
        // Display: 32nds and their fractions, grains in eighths of a cent.
        let f = |id, units| get(id).unwrap().price.format::<16>(Price(units));
        assert_eq!(
            f(ZT, 110 * 256 + 3 * 8 + 3).as_str(),
            "110'033",
            "3 3/8 32nds"
        );
        assert_eq!(
            f(ZF, 108 * 256 + 7 * 8 + 6).as_str(),
            "108'077",
            "7 3/4 32nds"
        );
        assert_eq!(f(ZN, 112 * 128 + 16 * 4 + 2).as_str(), "112'165");
        assert_eq!(f(ZB, 117 * 128 + 31 * 4).as_str(), "117'31");
        assert_eq!(f(ZC, 432 * 8 + 2).as_str(), "432'2", "432 1/4 cents");
        assert_eq!(f(ZS, 1050 * 8 + 6).as_str(), "1050'6");
        assert_eq!(f(JAPANESE_YEN, 65_455).as_str(), "0.0065455");
        assert_eq!(f(SI, 32_455).as_str(), "32.455");
        assert_eq!(f(HG, 50_125).as_str(), "5.0125");
        // Micro pairs: 10 micros per mini.
        for (mini, micro) in [(YM, MYM), (RTY, M2K), (CL, MCL), (GC, MGC)] {
            assert_eq!(
                get(mini).unwrap().micro_equivalent(Qty(1)),
                Some((micro, Qty(10)))
            );
        }
    }

    /// Labels round-trip for every futures root, in 2- and 4-digit years, and longer symbols
    /// win (`MCLZ26` is MCL, `MGCG27` MGC, `M2KZ26` M2K).
    #[test]
    fn every_future_label_round_trips() {
        for p in all().iter().filter(|p| !p.is_spot() && !p.is_equity()) {
            for k in p.contracts_from(d(2026, 11, 9)).take(8) {
                let label = k.label(p);
                assert_eq!(
                    parse_target(label.as_str()),
                    Some(Target::Contract(k)),
                    "{label}"
                );
                let (y, m) = k.year_month().unwrap();
                let long = format!("{}{}{y}", p.symbol, m.code());
                assert_eq!(parse_target(&long), Some(Target::Contract(k)), "{long}");
            }
            assert_eq!(parse_target(p.symbol), Some(Target::Product(p.id)));
        }
        let label = |s| match parse_target(s) {
            Some(Target::Contract(k)) => get(k.product).unwrap().symbol,
            _ => "",
        };
        assert_eq!(label("MCLZ26"), "MCL");
        assert_eq!(label("MGCG27"), "MGC");
        assert_eq!(label("M2KZ26"), "M2K");
        assert_eq!(label("MYMH27"), "MYM");
        assert_eq!(label("6EZ26"), "6E");
        assert_eq!(label("ngz26"), "NG");
        assert_eq!(label("ZSF27"), "ZS");
        assert_eq!(label("ZCF27"), "", "corn lists no January");
    }

    /// Front month by each listing cycle, across its calendar roll date.
    #[test]
    fn front_months_across_rolls() {
        let front = |id, day: CivilDate| {
            let p = get(id).unwrap();
            let k = lines_core::roll::calendar_front(&p, day).unwrap();
            k.label(&p).as_str().to_owned()
        };
        let cases: &[(ProductId, CivilDate, &str, &str)] = &[
            // Monthly: CLZ6 expires Fri 2026-11-20, roll 5 days before.
            (CL, d(2026, 11, 14), "CLZ26", "CLF27"),
            // MCL expires a business day earlier (Thu 19th): roll on the 14th.
            (MCL, d(2026, 11, 13), "MCLZ26", "MCLF27"),
            // NGF7 expires 2026-12-29 (3rd last business day of December), roll the 24th.
            (NG, d(2026, 12, 23), "NGF27", "NGG27"),
            // Gold's active months: first notice Mon 2026-11-30, roll 2 days before.
            (GC, d(2026, 11, 27), "GCZ26", "GCG27"),
            (MGC, d(2026, 11, 27), "MGCZ26", "MGCG27"),
            (SI, d(2026, 11, 27), "SIZ26", "SIH27"),
            (HG, d(2026, 11, 27), "HGZ26", "HGH27"),
            // Quarterly FX: 6EZ6 expires Mon 2026-12-14, roll 7 days before.
            (EURO_FX, d(2026, 12, 6), "6EZ26", "6EH27"),
            (JAPANESE_YEN, d(2026, 12, 6), "6JZ26", "6JH27"),
            // Treasuries: before first notice (2026-11-30).
            (ZT, d(2026, 11, 27), "ZTZ26", "ZTH27"),
            (ZB, d(2026, 11, 27), "ZBZ26", "ZBH27"),
            // Grains: soybeans' November contract, first notice Fri 2026-10-30.
            (ZS, d(2026, 10, 27), "ZSX26", "ZSF27"),
            (ZC, d(2026, 11, 27), "ZCZ26", "ZCH27"),
            (ZW, d(2027, 2, 23), "ZWH27", "ZWK27"),
            // Equity index: YMZ6 expires 2026-12-18, roll 8 days before.
            (YM, d(2026, 12, 9), "YMZ26", "YMH27"),
        ];
        for &(id, before, a, b) in cases {
            assert_eq!(front(id, before), a, "day before the roll");
            assert_eq!(front(id, before.add_days(1)), b, "roll day");
        }
    }

    /// One product per group: hours in UTC for Monday 2026-11-09 (CST, UTC-6).
    #[test]
    fn cme_group_sessions() {
        let eth_rth = |id| {
            let c = sessions(id).unwrap();
            let e = c.session(d(2026, 11, 9), SessionKind::Eth).unwrap();
            let r = c.session(d(2026, 11, 9), SessionKind::Rth).unwrap();
            [e.open, e.close, r.open, r.close]
        };
        let globex = |rth_open, rth_close| {
            [
                t("2026-11-08T23:00:00Z"),
                t("2026-11-09T22:00:00Z"),
                t(rth_open),
                t(rth_close),
            ]
        };
        assert_eq!(
            eth_rth(YM),
            globex("2026-11-09T14:30:00Z", "2026-11-09T21:00:00Z")
        );
        assert_eq!(
            eth_rth(ZB),
            globex("2026-11-09T13:20:00Z", "2026-11-09T20:00:00Z")
        );
        assert_eq!(
            eth_rth(CL),
            globex("2026-11-09T14:00:00Z", "2026-11-09T19:30:00Z")
        );
        assert_eq!(
            eth_rth(GC),
            globex("2026-11-09T13:20:00Z", "2026-11-09T18:30:00Z")
        );
        assert_eq!(
            eth_rth(EURO_FX),
            globex("2026-11-09T13:20:00Z", "2026-11-09T20:00:00Z")
        );
        // Grains: Sunday 19:00 CT to Monday 13:20 CT; RTH from 08:30.
        assert_eq!(
            eth_rth(ZC),
            [
                t("2026-11-09T01:00:00Z"),
                t("2026-11-09T19:20:00Z"),
                t("2026-11-09T14:30:00Z"),
                t("2026-11-09T19:20:00Z"),
            ]
        );
        let zc = sessions(ZC).unwrap();
        assert!(!zc.is_open(t("2026-11-09T00:30:00Z")), "Sunday 18:30 CT");
        assert!(zc.is_open(t("2026-11-09T01:30:00Z")));
        // MLK 2027: grains closed Monday, reopening Monday 19:00 CT for Tuesday.
        assert!(!zc.is_trade_date(d(2027, 1, 18)));
        assert_eq!(
            zc.next_open(t("2027-01-16T00:00:00Z")),
            Some(t("2027-01-19T01:00:00Z"))
        );
        // Energy on Thanksgiving: a 12:30 CT halt, Friday closes 12:45 CT.
        let f = sessions(CL)
            .unwrap()
            .session(d(2026, 11, 27), SessionKind::Eth)
            .unwrap();
        assert_eq!(
            f.halt,
            Some((t("2026-11-26T18:30:00Z"), t("2026-11-26T23:00:00Z")))
        );
        assert_eq!(f.close, t("2026-11-27T18:45:00Z"));
        // Same hours and holidays within a group.
        for (a, b) in [
            (ES, YM),
            (ES, M2K),
            (ZN, ZT),
            (CL, NG),
            (GC, HG),
            (EURO_FX, BRITISH_POUND),
            (ZC, ZW),
        ] {
            assert_eq!(session_spec(a), session_spec(b));
        }
    }

    #[test]
    fn every_product_has_a_group_in_catalog_order() {
        let groups: Vec<_> = all().iter().map(|p| group(p.id).unwrap()).collect();
        assert!(groups.windows(2).all(|w| w[0] <= w[1]), "{groups:?}");
        assert_eq!(group(ZC).unwrap().label(), "Agriculture");
        assert_eq!(group(ProductId(999)), None);
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
            // A product target resolves to the shares (`--instrument SPY`, a workspace pane).
            assert_eq!(
                lines_core::roll::resolve_front(&p, d(2026, 11, 9), None, None, &[])
                    .map(|f| f.contract),
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
