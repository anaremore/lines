//! Products and contracts (decisions 0003 and 0013).
//!
//! - A [`Product`] (ES, MNQ, BTC/USD) is permanent catalog data: everything shared across
//!   delivery months. Its [`ProductId`] never changes and is never reused.
//! - A [`ContractKey`] names one tradable contract (ESZ26) and is *computed*, never assigned,
//!   so every machine and recording agrees on it without a lookup table.
//! - Spot products (Coinbase BTC/USD) have no delivery months. They are modelled explicitly
//!   as [`Delivery::Spot`]: one perpetual contract per product, no expiry, never rolls.
//! - Equities (SPY, decision 0028) are [`Delivery::Equity`]: one contract per product, no
//!   expiry, quantity in shares. Their listed options are [`Delivery::Option`] on the
//!   *underlying's* product: expiry date, exact strike (a [`crate::Price`] in the underlying's
//!   price units) and right. The multiplier and exercise style are the product's
//!   [`OptionRules`]; the key never carries them, so one OCC symbol is one key.
//!
//! Vendor symbols never appear here; providers derive theirs from a key by rule inside
//! `lines-feed`.

use std::fmt;

use crate::calendar::CivilDate;
use crate::fixed_str::FixedStr;
use crate::price::{Price, PriceFormat};
use crate::qty::{Qty, QtyFormat};

/// Permanent product identifier. Never renumber or reuse one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProductId(pub u32);

impl fmt::Display for ProductId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// ISO 4217-style currency code.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Currency(pub [u8; 3]);

impl Currency {
    pub const USD: Currency = Currency(*b"USD");
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

/// Delivery month, 1 (January) ..= 12 (December).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month(u8);

/// Standard futures month codes, January..December.
const MONTH_CODES: [char; 12] = ['F', 'G', 'H', 'J', 'K', 'M', 'N', 'Q', 'U', 'V', 'X', 'Z'];

impl Month {
    pub fn new(m: u8) -> Option<Month> {
        (1..=12).contains(&m).then_some(Month(m))
    }
    pub fn number(self) -> u8 {
        self.0
    }
    /// Exchange month code (`H` = March, `Z` = December). Universal, not vendor-specific.
    pub fn code(self) -> char {
        MONTH_CODES[self.0 as usize - 1]
    }
    pub fn from_code(c: char) -> Option<Month> {
        MONTH_CODES
            .iter()
            .position(|&x| x == c.to_ascii_uppercase())
            .map(|i| Month(i as u8 + 1))
    }
}

/// The set of listed delivery months (bitmask, bit 0 = January).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MonthCycle(u16);

impl MonthCycle {
    /// March, June, September, December (H M U Z).
    pub const QUARTERLY: MonthCycle = MonthCycle(1 << 2 | 1 << 5 | 1 << 8 | 1 << 11);
    pub const MONTHLY: MonthCycle = MonthCycle(0x0fff);

    /// A cycle from month codes, e.g. `"GJMQVZ"` (February, April, ... December). For
    /// constants: an unknown code panics (at compile time in a `const`).
    pub const fn from_codes(codes: &str) -> MonthCycle {
        let b = codes.as_bytes();
        let mut bits = 0u16;
        let mut i = 0;
        while i < b.len() {
            let mut m = 0;
            while m < 12 {
                if MONTH_CODES[m] as u8 == b[i] {
                    break;
                }
                m += 1;
            }
            assert!(m < 12, "unknown month code");
            bits |= 1 << m;
            i += 1;
        }
        MonthCycle(bits)
    }

    pub fn contains(self, m: Month) -> bool {
        self.0 & (1 << (m.0 - 1)) != 0
    }

    /// Listed months as codes, e.g. "HMUZ".
    pub fn codes(self) -> FixedStr<12> {
        let mut s = FixedStr::new();
        for m in 1..=12 {
            let m = Month(m);
            if self.contains(m) {
                let _ = fmt::Write::write_char(&mut s, m.code());
            }
        }
        s
    }
}

/// How a contract is delivered.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Delivery {
    /// A dated futures contract (4-digit year).
    Monthly { year: u16, month: Month },
    /// A spot product's single perpetual "contract": no expiry, never rolls.
    Spot,
    /// An equity's shares (decision 0028): no expiry, never rolls.
    Equity,
    /// A listed option on the product (the underlying).
    Option(OptionKey),
}

/// Call or put.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Right {
    Call,
    Put,
}

impl Right {
    /// `C` or `P` (OCC and our labels).
    pub fn letter(self) -> char {
        match self {
            Right::Call => 'C',
            Right::Put => 'P',
        }
    }

    pub fn from_letter(c: char) -> Option<Right> {
        match c.to_ascii_uppercase() {
            'C' => Some(Right::Call),
            'P' => Some(Right::Put),
            _ => None,
        }
    }
}

/// One listed option series of an underlying: what an OCC symbol names (decision 0028).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OptionKey {
    /// The expiration date (the last trade date; settlement per the product's rules).
    pub expiry: CivilDate,
    /// Exact, in the underlying product's price units (SPY 450.50 = `Price(45050)`).
    pub strike: Price,
    pub right: Right,
}

/// How an option may be exercised.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExerciseStyle {
    /// Any business day up to expiry (equity and ETF options).
    American,
    /// At expiry only (index options).
    European,
}

/// An option's minimum price increment, in the underlying's price units.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum OptionTick {
    /// One increment at every price (the penny interval program: SPY, QQQ).
    Flat(i64),
    /// `below` under `threshold`, `above` at or over it (e.g. 0.01 below $3, 0.05 above).
    Split {
        threshold: i64,
        below: i64,
        above: i64,
    },
}

impl OptionTick {
    /// The increment for an option premium `price`. Providers may report a chain's own rule;
    /// theirs wins where they differ (decision 0028).
    pub fn at(self, price: Price) -> i64 {
        match self {
            OptionTick::Flat(t) => t,
            OptionTick::Split {
                threshold,
                below,
                above,
            } => {
                if price.0 < threshold {
                    below
                } else {
                    above
                }
            }
        }
    }
}

/// A product's listed options (catalog data).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct OptionRules {
    /// Shares (or units) per contract: money = premium × multiplier × quantity.
    pub multiplier: u32,
    pub style: ExerciseStyle,
    pub tick: OptionTick,
}

/// One contract, computed from the product and its delivery.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContractKey {
    pub product: ProductId,
    pub delivery: Delivery,
}

impl ContractKey {
    pub fn monthly(product: ProductId, year: u16, month: Month) -> ContractKey {
        ContractKey {
            product,
            delivery: Delivery::Monthly { year, month },
        }
    }
    pub fn spot(product: ProductId) -> ContractKey {
        ContractKey {
            product,
            delivery: Delivery::Spot,
        }
    }
    pub fn equity(product: ProductId) -> ContractKey {
        ContractKey {
            product,
            delivery: Delivery::Equity,
        }
    }
    /// An option on `underlying` (the equity product's id).
    pub fn option(
        underlying: ProductId,
        expiry: CivilDate,
        strike: Price,
        right: Right,
    ) -> ContractKey {
        ContractKey {
            product: underlying,
            delivery: Delivery::Option(OptionKey {
                expiry,
                strike,
                right,
            }),
        }
    }
    pub fn year_month(&self) -> Option<(u16, Month)> {
        match self.delivery {
            Delivery::Monthly { year, month } => Some((year, month)),
            Delivery::Spot | Delivery::Equity | Delivery::Option(_) => None,
        }
    }

    pub fn option_key(&self) -> Option<OptionKey> {
        match self.delivery {
            Delivery::Option(o) => Some(o),
            _ => None,
        }
    }

    /// Units of the product per contract for money: the option multiplier (100 for SPY
    /// options), else 1 (a future's point value already is per contract). `None`: an option
    /// on a product that lists none.
    pub fn multiplier(&self, product: &Product) -> Option<i64> {
        match self.delivery {
            Delivery::Option(_) => product.options().map(|o| i64::from(o.multiplier)),
            _ => Some(1),
        }
    }

    /// Canonical Lines label: product symbol + month code + 2-digit year (`ESZ26`), the bare
    /// symbol for spot and equities (`BTC/USD`, `SPY`), and for an option the underlying,
    /// expiry (`YYMMDD`), strike without trailing zeros and right (`SPY 261218 450.5C`). Ours,
    /// not any vendor's format (OCC symbols are derived in `lines-feed`).
    pub fn label(&self, product: &Product) -> FixedStr<24> {
        let mut s = FixedStr::new();
        let _ = match self.delivery {
            Delivery::Monthly { year, month } => fmt::Write::write_fmt(
                &mut s,
                format_args!("{}{}{:02}", product.symbol, month.code(), year % 100),
            ),
            Delivery::Spot | Delivery::Equity => fmt::Write::write_str(&mut s, product.symbol),
            Delivery::Option(o) => {
                let mut strike = FixedStr::<20>::new();
                let _ = product.price.write(o.strike, &mut strike);
                let st = strike.as_str();
                let st = if st.contains('.') {
                    st.trim_end_matches('0').trim_end_matches('.')
                } else {
                    st
                };
                fmt::Write::write_fmt(
                    &mut s,
                    format_args!(
                        "{} {:02}{:02}{:02} {}{}",
                        product.symbol,
                        o.expiry.year % 100,
                        o.expiry.month,
                        o.expiry.day,
                        st,
                        o.right.letter()
                    ),
                )
            }
        };
        s
    }

    /// Parse a canonical label (`ESZ26`, `ESZ2026`) for a known product.
    pub fn parse_label(product: &Product, label: &str) -> Option<ContractKey> {
        let rest = label.trim().strip_prefix(product.symbol)?;
        if rest.is_empty() {
            return None;
        }
        let month = Month::from_code(rest.chars().next()?)?;
        let digits = &rest[1..];
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let year: u16 = match digits.len() {
            2 => 2000 + digits.parse::<u16>().ok()?,
            // A typed year before 1900 is a mistake, not a contract (and has no calendar).
            4 => digits.parse().ok().filter(|y| *y >= 1900)?,
            _ => return None,
        };
        match product.kind {
            ProductKind::Future { cycle, .. } if cycle.contains(month) => {
                Some(ContractKey::monthly(product.id, year, month))
            }
            _ => None,
        }
    }
}

/// Exact positive ratio `num / den`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ratio {
    pub num: i64,
    pub den: i64,
}

impl Ratio {
    pub const fn new(num: i64, den: i64) -> Ratio {
        Ratio { num, den }
    }
}

/// Micro/mini equivalence (decisions 0006, 0013). Expressed in *micro* units so caps stay
/// integers: NQ = 10 MNQ, MNQ = 1 MNQ. A product without a micro is its own unit (1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MicroEquivalent {
    /// The micro product all sizes are expressed in.
    pub micro: ProductId,
    /// Micro contracts per one contract of this product.
    pub per_contract: u32,
}

/// When a dated contract stops trading (calendar fallback; providers' dates win).
/// Exchange holidays are not modelled: business day = Monday–Friday.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExpiryRule {
    /// Third Friday of the contract month (CME equity index futures).
    ThirdFriday,
    /// The 7th business day before the last business day of the contract month (CBOT
    /// Treasury note futures).
    SeventhBusinessDayBeforeLastBusinessDay,
    /// The `n`th last business day of the contract month (1 = the last: CBOT 2- and 5-year
    /// notes; 3: COMEX gold, silver and copper).
    NthLastBusinessDay(u8),
    /// The `n`th last business day of the month *before* the contract month (NYMEX Henry Hub
    /// natural gas: 3, "the third business day prior to the first day of the delivery month").
    NthLastBusinessDayOfPriorMonth(u8),
    /// `n` business days before the 25th calendar day of the month before the contract month,
    /// counted from the business day before the 25th when the 25th isn't one (NYMEX WTI crude
    /// oil: 3; Micro WTI, one business day earlier: 4).
    BusinessDaysBefore25thOfPriorMonth(u8),
    /// The business day before the 15th calendar day of the contract month (CBOT grains).
    BusinessDayBefore15th,
    /// `n` business days before the third Wednesday of the contract month (CME FX: 2).
    BusinessDaysBeforeThirdWednesday(u8),
}

/// The calendar roll date used when volume isn't available.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RollRule {
    /// Roll this many calendar days before expiry (equity index: 8, i.e. the Thursday before
    /// the second Friday preceding expiry week).
    DaysBeforeExpiry(u8),
    /// Roll this many calendar days before first notice day, which is the last business day of
    /// the month before the contract month (physically delivered Treasuries, metals, grains).
    DaysBeforeFirstNotice(u8),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProductKind {
    Future {
        cycle: MonthCycle,
        expiry: ExpiryRule,
        roll: RollRule,
    },
    Spot,
    /// Shares (decision 0028), with the options listed on them, if any.
    Equity {
        options: Option<OptionRules>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Product {
    pub id: ProductId,
    /// Internal canonical symbol for display and lookup (e.g. `ES`, `BTC/USD`).
    pub symbol: &'static str,
    pub description: &'static str,
    pub price: PriceFormat,
    pub qty: QtyFormat,
    /// Currency prices are quoted in.
    pub currency: Currency,
    /// Currency per 1.0 of price per contract (ES 50, MES 5, NQ 20, MNQ 2, ZN 1000; spot 1 per
    /// unit of quantity).
    pub point_value: Ratio,
    pub kind: ProductKind,
    pub micro: Option<MicroEquivalent>,
}

impl Product {
    /// Convert a quantity of this product into micro-equivalent contracts (whole-contract
    /// products only). `None` if the product has no micro relation.
    pub fn micro_equivalent(&self, qty: Qty) -> Option<(ProductId, Qty)> {
        let m = self.micro?;
        if self.qty.decimals != 0 {
            return None;
        }
        Some((m.micro, Qty(qty.0.checked_mul(m.per_contract as i64)?)))
    }

    /// No delivery months and no expiry (crypto spot and perpetuals). Equities are not spot:
    /// see [`Product::is_equity`].
    pub fn is_spot(&self) -> bool {
        matches!(self.kind, ProductKind::Spot)
    }

    pub fn is_equity(&self) -> bool {
        matches!(self.kind, ProductKind::Equity { .. })
    }

    /// The product's listed options, if any.
    pub fn options(&self) -> Option<OptionRules> {
        match self.kind {
            ProductKind::Equity { options } => options,
            _ => None,
        }
    }

    /// Whether `key` is a contract of this product: its delivery kind matches the product's
    /// (an option needs listed options and a strike on the price grid's unit, > 0).
    pub fn lists(&self, key: &ContractKey) -> bool {
        key.product == self.id
            && match (self.kind, key.delivery) {
                (ProductKind::Future { cycle, .. }, Delivery::Monthly { month, .. }) => {
                    cycle.contains(month)
                }
                (ProductKind::Spot, Delivery::Spot) => true,
                (ProductKind::Equity { .. }, Delivery::Equity) => true,
                (ProductKind::Equity { options: Some(_) }, Delivery::Option(o)) => o.strike.0 > 0,
                _ => false,
            }
    }

    /// Contracts listed after `date`'s month, in delivery order, starting with the first
    /// listed month at or after `date`'s month. Spot and equities yield their single contract
    /// (options are listed by the provider's chain, not computed).
    pub fn contracts_from(&self, date: CivilDate) -> impl Iterator<Item = ContractKey> + '_ {
        let id = self.id;
        let equity = self.is_equity();
        let (spot, cycle) = match self.kind {
            ProductKind::Spot | ProductKind::Equity { .. } => (true, MonthCycle(0)),
            ProductKind::Future { cycle, .. } => (false, cycle),
        };
        let mut y = date.year as i32;
        let mut m = date.month;
        let mut emitted_spot = false;
        std::iter::from_fn(move || {
            if spot {
                if emitted_spot {
                    return None;
                }
                emitted_spot = true;
                return Some(if equity {
                    ContractKey::equity(id)
                } else {
                    ContractKey::spot(id)
                });
            }
            if cycle.0 == 0 {
                return None;
            }
            loop {
                let month = Month(m);
                let year = y;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
                if cycle.contains(month) {
                    return Some(ContractKey::monthly(id, year as u16, month));
                }
            }
        })
    }
}

/// What a market-data provider can deliver. The UI enables features by capability,
/// never by asset class or provider name.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Server-side historical bars (back-fill on connect/reconnect).
    pub history: bool,
    /// Best bid/offer updates.
    pub bbo: bool,
    /// Order-book depth.
    pub depth: bool,
    /// Quantities may be fractional.
    pub fractional_qty: bool,
    /// Available around the clock (useful for development).
    pub always_open: bool,
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::{DisplayStyle, PriceScale};

    pub fn es() -> Product {
        Product {
            id: ProductId(1),
            symbol: "ES",
            description: "E-mini S&P 500",
            price: PriceFormat::new(
                PriceScale::decimal(2),
                DisplayStyle::Decimal { digits: 2 },
                25,
            ),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
            point_value: Ratio::new(50, 1),
            kind: ProductKind::Future {
                cycle: MonthCycle::QUARTERLY,
                expiry: ExpiryRule::ThirdFriday,
                roll: RollRule::DaysBeforeExpiry(8),
            },
            micro: Some(MicroEquivalent {
                micro: ProductId(4),
                per_contract: 10,
            }),
        }
    }

    /// An ETF with listed options (as the catalog's SPY).
    pub fn spy() -> Product {
        Product {
            id: ProductId(2001),
            symbol: "SPY",
            description: "SPDR S&P 500 ETF",
            price: PriceFormat::new(
                PriceScale::decimal(2),
                DisplayStyle::Decimal { digits: 2 },
                1,
            ),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
            point_value: Ratio::new(1, 1),
            kind: ProductKind::Equity {
                options: Some(OptionRules {
                    multiplier: 100,
                    style: ExerciseStyle::American,
                    tick: OptionTick::Flat(1),
                }),
            },
            micro: None,
        }
    }

    pub fn btc() -> Product {
        Product {
            id: ProductId(1001),
            symbol: "BTC/USD",
            description: "Bitcoin",
            price: PriceFormat::new(
                PriceScale::decimal(2),
                DisplayStyle::Decimal { digits: 2 },
                1,
            ),
            qty: QtyFormat { decimals: 8 },
            currency: Currency::USD,
            point_value: Ratio::new(1, 1),
            kind: ProductKind::Spot,
            micro: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn month_codes() {
        assert_eq!(Month::new(12).unwrap().code(), 'Z');
        assert_eq!(Month::from_code('h'), Month::new(3));
        assert_eq!(Month::from_code('A'), None);
        assert_eq!(Month::new(13), None);
        assert_eq!(MonthCycle::QUARTERLY.codes().as_str(), "HMUZ");
        assert_eq!(MonthCycle::from_codes("HMUZ"), MonthCycle::QUARTERLY);
        assert_eq!(MonthCycle::from_codes("FGHJKMNQUVXZ"), MonthCycle::MONTHLY);
        const GRAINS: MonthCycle = MonthCycle::from_codes("ZHKNU");
        assert_eq!(GRAINS.codes().as_str(), "HKNUZ", "order doesn't matter");
    }

    #[test]
    fn labels_roundtrip() {
        let es = es();
        let k = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        assert_eq!(k.label(&es).as_str(), "ESZ26");
        assert_eq!(ContractKey::parse_label(&es, "ESZ26"), Some(k));
        assert_eq!(ContractKey::parse_label(&es, "ESZ2026"), Some(k));
        assert_eq!(
            ContractKey::parse_label(&es, "ESF26"),
            None,
            "January is not listed for ES"
        );
        assert_eq!(ContractKey::parse_label(&es, "ES"), None);
        assert_eq!(ContractKey::parse_label(&es, "ESZ0000"), None, "year 0");
        assert_eq!(ContractKey::parse_label(&es, "ESZ1899"), None);
        assert!(ContractKey::parse_label(&es, "ESZ1900").is_some());
        assert_eq!(
            ContractKey::parse_label(&es, "ESZ6"),
            None,
            "1-digit years are ambiguous"
        );
        let btc = btc();
        assert_eq!(ContractKey::spot(btc.id).label(&btc).as_str(), "BTC/USD");
    }

    /// Decision 0028: equities and listed options, keyed exactly.
    #[test]
    fn equities_and_options() {
        let spy = spy();
        let eq = ContractKey::equity(spy.id);
        assert_eq!(eq.label(&spy).as_str(), "SPY");
        assert!(spy.lists(&eq) && !spy.lists(&ContractKey::spot(spy.id)));
        assert_eq!(
            spy.contracts_from(CivilDate::new(2026, 11, 7).unwrap())
                .collect::<Vec<_>>(),
            [eq]
        );
        let expiry = CivilDate::new(2026, 12, 18).unwrap();
        let call = ContractKey::option(spy.id, expiry, Price(45_050), Right::Call);
        let put = ContractKey::option(spy.id, expiry, Price(45_000), Right::Put);
        assert_eq!(call.label(&spy).as_str(), "SPY 261218 450.5C");
        assert_eq!(put.label(&spy).as_str(), "SPY 261218 450P");
        assert!(spy.lists(&call));
        assert!(!spy.lists(&ContractKey::option(spy.id, expiry, Price(0), Right::Call)));
        assert_eq!(call.option_key().unwrap().strike, Price(45_050));
        assert_eq!(call.year_month(), None);
        // Money: 100 shares per contract; a future or the shares themselves: 1.
        assert_eq!(call.multiplier(&spy), Some(100));
        assert_eq!(eq.multiplier(&spy), Some(1));
        let es = es();
        let es_opt = ContractKey::option(es.id, expiry, Price(600_000), Right::Call);
        assert_eq!(es_opt.multiplier(&es), None, "ES lists no options here");
        assert!(!es.lists(&es_opt) && !es.lists(&ContractKey::equity(es.id)));
        // Distinct keys, and the same series is the same key.
        assert_ne!(call, put);
        assert_eq!(
            call,
            ContractKey::option(spy.id, expiry, Price(45_050), Right::Call)
        );
        // Option ticks: flat, or split at a premium threshold.
        assert_eq!(OptionTick::Flat(1).at(Price(1_000)), 1);
        let split = OptionTick::Split {
            threshold: 300,
            below: 1,
            above: 5,
        };
        assert_eq!((split.at(Price(299)), split.at(Price(300))), (1, 5));
        assert_eq!(Right::from_letter('p'), Some(Right::Put));
        assert_eq!(Right::from_letter('x'), None);
    }

    #[test]
    fn ordering_is_chronological() {
        let p = ProductId(1);
        let m = |x| Month::new(x).unwrap();
        assert!(ContractKey::monthly(p, 2025, m(12)) < ContractKey::monthly(p, 2026, m(3)));
        assert!(ContractKey::monthly(p, 2026, m(3)) < ContractKey::monthly(p, 2026, m(6)));
    }

    #[test]
    fn listed_contracts() {
        let es = es();
        let d = CivilDate::new(2025, 11, 20).unwrap();
        let next: Vec<_> = es
            .contracts_from(d)
            .take(3)
            .map(|k| k.label(&es).as_str().to_owned())
            .collect();
        assert_eq!(next, ["ESZ25", "ESH26", "ESM26"]);
        let btc = btc();
        assert_eq!(
            btc.contracts_from(d).collect::<Vec<_>>(),
            [ContractKey::spot(btc.id)]
        );
    }

    #[test]
    fn micro_equivalence() {
        let es = es();
        assert_eq!(es.micro_equivalent(Qty(3)), Some((ProductId(4), Qty(30))));
        assert_eq!(btc().micro_equivalent(Qty(1)), None);
    }
}
