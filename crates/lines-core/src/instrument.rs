//! Products and contracts (decisions 0003 and 0013).
//!
//! - A [`Product`] (ES, MNQ, BTC/USD) is permanent catalog data: everything shared across
//!   delivery months. Its [`ProductId`] never changes and is never reused.
//! - A [`ContractKey`] names one tradable contract (ESZ26) and is *computed*, never assigned,
//!   so every machine and recording agrees on it without a lookup table.
//! - Spot products (Coinbase BTC/USD) have no delivery months. They are modelled explicitly
//!   as [`Delivery::Spot`]: one perpetual contract per product, no expiry, never rolls.
//!
//! Vendor symbols never appear here; providers derive theirs from a key by rule inside
//! `lines-feed`.

use std::fmt;

use crate::calendar::CivilDate;
use crate::fixed_str::FixedStr;
use crate::price::PriceFormat;
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
    pub fn year_month(&self) -> Option<(u16, Month)> {
        match self.delivery {
            Delivery::Monthly { year, month } => Some((year, month)),
            Delivery::Spot => None,
        }
    }

    /// Canonical Lines label: product symbol + month code + 2-digit year (`ESZ26`), or the
    /// bare symbol for spot (`BTC/USD`). Ours, not any vendor's format.
    pub fn label(&self, product: &Product) -> FixedStr<24> {
        let mut s = FixedStr::new();
        let _ = match self.delivery {
            Delivery::Monthly { year, month } => fmt::Write::write_fmt(
                &mut s,
                format_args!("{}{}{:02}", product.symbol, month.code(), year % 100),
            ),
            Delivery::Spot => fmt::Write::write_str(&mut s, product.symbol),
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
            4 => digits.parse().ok()?,
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
}

/// The calendar roll date used when volume isn't available.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RollRule {
    /// Roll this many calendar days before expiry (equity index: 8, i.e. the Thursday before
    /// the second Friday preceding expiry week).
    DaysBeforeExpiry(u8),
    /// Roll this many calendar days before first notice day, which is the last business day of
    /// the month before the contract month (physically delivered Treasuries).
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

    pub fn is_spot(&self) -> bool {
        matches!(self.kind, ProductKind::Spot)
    }

    /// Contracts listed after `date`'s month, in delivery order, starting with the first
    /// listed month at or after `date`'s month. Spot yields its single contract.
    pub fn contracts_from(&self, date: CivilDate) -> impl Iterator<Item = ContractKey> + '_ {
        let id = self.id;
        let (spot, cycle) = match self.kind {
            ProductKind::Spot => (true, MonthCycle(0)),
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
                return Some(ContractKey::spot(id));
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
        assert_eq!(
            ContractKey::parse_label(&es, "ESZ6"),
            None,
            "1-digit years are ambiguous"
        );
        let btc = btc();
        assert_eq!(ContractKey::spot(btc.id).label(&btc).as_str(), "BTC/USD");
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
