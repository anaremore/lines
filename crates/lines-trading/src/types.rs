//! Identifiers, sides, order kinds, exact money and per-contract trading specs.

use std::fmt;

use lines_core::{ContractKey, Delivery, Month, Price, Product, ProductId, Qty};
use serde::{Deserialize, Serialize};

/// A trading account (paper or, later, live). Chosen by the user; determines broker and rules.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn new(s: &str) -> AccountId {
        AccountId(s.to_owned())
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An order, assigned by the trading core when the command is accepted (the broker uses it
/// as the client order id).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderId(pub u64);

/// A command, assigned at the single entry point.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommandId(pub u64);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// +1 for buys, −1 for sells.
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// What kind of order, with its exact prices (decision 0002).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrderKind {
    Market,
    Limit {
        #[serde(with = "price_serde")]
        price: Price,
    },
    /// Becomes a market order when a trade prints at or through `trigger`.
    Stop {
        #[serde(with = "price_serde")]
        trigger: Price,
    },
    /// Becomes a limit order at `limit` when triggered.
    StopLimit {
        #[serde(with = "price_serde")]
        trigger: Price,
        #[serde(with = "price_serde")]
        limit: Price,
    },
}

impl OrderKind {
    /// Every price in the order (for tick and band checks).
    pub fn prices(&self) -> impl Iterator<Item = Price> {
        let (a, b) = match *self {
            OrderKind::Market => (None, None),
            OrderKind::Limit { price } => (Some(price), None),
            OrderKind::Stop { trigger } => (Some(trigger), None),
            OrderKind::StopLimit { trigger, limit } => (Some(trigger), Some(limit)),
        };
        a.into_iter().chain(b)
    }
}

/// Exact money in units of 10⁻¹⁰ of the account currency. Wide enough for any account; fine
/// enough that every catalog product's P&L per (price unit × quantity unit) is a whole number
/// of units (checked by [`ContractSpec::new`]).
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Money(pub i128);

impl Money {
    pub const ZERO: Money = Money(0);

    /// Exactly zero (an optional saved amount is left out).
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
    pub const UNITS_PER_WHOLE: i128 = 10_000_000_000;

    /// Whole currency units (e.g. dollars).
    pub fn whole(n: i64) -> Money {
        Money(n as i128 * Self::UNITS_PER_WHOLE)
    }

    /// Cents, for presets and fees.
    pub fn cents(n: i64) -> Money {
        Money(n as i128 * (Self::UNITS_PER_WHOLE / 100))
    }

    /// For display only (never for accounting).
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / Self::UNITS_PER_WHOLE as f64
    }
}

impl std::ops::Add for Money {
    type Output = Money;
    fn add(self, o: Money) -> Money {
        Money(self.0 + o.0)
    }
}

impl std::ops::Sub for Money {
    type Output = Money;
    fn sub(self, o: Money) -> Money {
        Money(self.0 - o.0)
    }
}

impl std::ops::Neg for Money {
    type Output = Money;
    fn neg(self) -> Money {
        Money(-self.0)
    }
}

impl std::ops::AddAssign for Money {
    fn add_assign(&mut self, o: Money) {
        self.0 += o.0;
    }
}

impl std::ops::SubAssign for Money {
    fn sub_assign(&mut self, o: Money) {
        self.0 -= o.0;
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Rounded to cents for display.
        let cents = self.0.div_euclid(Self::UNITS_PER_WHOLE / 100)
            + i128::from(
                self.0.rem_euclid(Self::UNITS_PER_WHOLE / 100) * 2 >= Self::UNITS_PER_WHOLE / 100,
            );
        let sign = if cents < 0 { "-" } else { "" };
        let c = cents.abs();
        write!(f, "{sign}{}.{:02}", c / 100, c % 100)
    }
}

/// Everything the trading core needs to know about a contract.
#[derive(Clone, Debug)]
pub struct ContractSpec {
    pub contract: ContractKey,
    pub product: Product,
    /// Money per (price unit × quantity unit): exact.
    pub money_per_unit: i128,
    /// Fee per contract per side (whole-contract products) or per quantity unit (fractional).
    pub fee_per_qty: Money,
}

impl ContractSpec {
    /// `None` if the product's P&L isn't a whole number of money units (none in the catalog).
    pub fn new(
        product: Product,
        contract: ContractKey,
        fee_per_qty: Money,
    ) -> Option<ContractSpec> {
        // money/unit = scale × point_value × multiplier / 10^qty_decimals, in units of 10⁻¹⁰
        // (the multiplier: an option's shares per contract, else 1).
        let multiplier = contract.multiplier(&product)?;
        let num = product.price.scale.num() as i128
            * product.point_value.num as i128
            * multiplier as i128
            * Money::UNITS_PER_WHOLE;
        let den = product.price.scale.den() as i128
            * product.point_value.den as i128
            * 10i128.pow(product.qty.decimals as u32);
        (num % den == 0).then(|| ContractSpec {
            contract,
            money_per_unit: num / den,
            product,
            fee_per_qty,
        })
    }

    /// Money for a price difference (units) times a quantity (units).
    pub fn money(&self, price_units: i128, qty_units: i128) -> Money {
        Money(price_units * qty_units * self.money_per_unit)
    }

    pub fn tick(&self) -> i64 {
        self.product.price.tick
    }

    pub fn on_tick_grid(&self, p: Price) -> bool {
        p.0.rem_euclid(self.tick()) == 0
    }

    pub fn fee(&self, qty: Qty) -> Money {
        Money(self.fee_per_qty.0 * qty.0 as i128)
    }
}

/// A contract key as it is saved and sent (lines-core types carry no serde).
///
/// A future has `year` and `month`; spot has nothing else; an equity has `equity: true`; an
/// option has `option` (decision 0028). Files that can hold the last two moved their version
/// (trading file v3, journal v2), so an older build refuses them instead of reading an option
/// as spot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContractDoc {
    pub product: u32,
    /// `None` for spot, equities and options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub equity: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<OptionDoc>,
}

/// An option series as saved: expiry as `YYYYMMDD`, the strike in the underlying's price
/// units (exact), and the right.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OptionDoc {
    pub expiry: u32,
    pub strike: i64,
    pub right: RightDoc,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightDoc {
    Call,
    Put,
}

impl ContractDoc {
    pub fn from_key(k: ContractKey) -> ContractDoc {
        let base = ContractDoc {
            product: k.product.0,
            year: None,
            month: None,
            equity: false,
            option: None,
        };
        match k.delivery {
            Delivery::Monthly { year, month } => ContractDoc {
                year: Some(year),
                month: Some(month.number()),
                ..base
            },
            Delivery::Spot => base,
            Delivery::Equity => ContractDoc {
                equity: true,
                ..base
            },
            Delivery::Option(o) => ContractDoc {
                option: Some(OptionDoc {
                    expiry: u32::from(o.expiry.year) * 10_000
                        + u32::from(o.expiry.month) * 100
                        + u32::from(o.expiry.day),
                    strike: o.strike.0,
                    right: match o.right {
                        lines_core::Right::Call => RightDoc::Call,
                        lines_core::Right::Put => RightDoc::Put,
                    },
                }),
                ..base
            },
        }
    }

    pub fn to_key(self) -> Option<ContractKey> {
        let p = ProductId(self.product);
        match (self.year, self.month, self.equity, self.option) {
            (Some(y), Some(m), false, None) => Some(ContractKey::monthly(p, y, Month::new(m)?)),
            (None, None, false, None) => Some(ContractKey::spot(p)),
            (None, None, true, None) => Some(ContractKey::equity(p)),
            (None, None, false, Some(o)) => {
                let expiry = lines_core::CivilDate::new(
                    u16::try_from(o.expiry / 10_000).ok()?,
                    (o.expiry / 100 % 100) as u8,
                    (o.expiry % 100) as u8,
                )?;
                if o.strike <= 0 {
                    return None;
                }
                let right = match o.right {
                    RightDoc::Call => lines_core::Right::Call,
                    RightDoc::Put => lines_core::Right::Put,
                };
                Some(ContractKey::option(
                    p,
                    expiry,
                    lines_core::Price(o.strike),
                    right,
                ))
            }
            _ => None,
        }
    }
}

/// `#[serde(with = ...)]` helpers: lines-core types carry no serde, so they save as plain
/// integers (exact units) and contracts as [`ContractDoc`].
pub mod price_serde {
    use lines_core::Price;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(p: &Price, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(p.0)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Price, D::Error> {
        i64::deserialize(d).map(Price)
    }
}

pub mod opt_price_serde {
    use lines_core::Price;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(p: &Option<Price>, s: S) -> Result<S::Ok, S::Error> {
        match p {
            Some(p) => s.serialize_some(&p.0),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Price>, D::Error> {
        Option::<i64>::deserialize(d).map(|o| o.map(Price))
    }
}

pub mod qty_serde {
    use lines_core::Qty;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(q: &Qty, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(q.0)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Qty, D::Error> {
        i64::deserialize(d).map(Qty)
    }
}

pub mod contract_serde {
    use lines_core::ContractKey;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::ContractDoc;
    pub fn serialize<S: Serializer>(k: &ContractKey, s: S) -> Result<S::Ok, S::Error> {
        ContractDoc::from_key(*k).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ContractKey, D::Error> {
        ContractDoc::deserialize(d)?
            .to_key()
            .ok_or_else(|| serde::de::Error::custom("bad contract"))
    }
}

pub mod time_serde {
    use lines_core::UtcMillis;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(t: &UtcMillis, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(t.0)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<UtcMillis, D::Error> {
        i64::deserialize(d).map(UtcMillis)
    }
}

pub mod opt_time_serde {
    use lines_core::UtcMillis;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(t: &Option<UtcMillis>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_some(&t.0),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<UtcMillis>, D::Error> {
        Option::<i64>::deserialize(d).map(|o| o.map(UtcMillis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::catalog;

    /// Decision 0028: the saved contract shape. Futures and spot are written exactly as before
    /// (pinned); equities and options round-trip; mixed or broken shapes are refused.
    #[test]
    fn contract_docs_keep_futures_and_add_equities_and_options() {
        let z = Month::new(12).unwrap();
        let es = ContractKey::monthly(catalog::ES, 2026, z);
        let doc = |k: ContractKey| serde_json::to_string(&ContractDoc::from_key(k)).unwrap();
        assert_eq!(doc(es), r#"{"product":1,"year":2026,"month":12}"#);
        assert_eq!(
            doc(ContractKey::spot(catalog::BTC_USD)),
            r#"{"product":1001}"#
        );
        let opt = ContractKey::option(
            catalog::SPY,
            lines_core::CivilDate::new(2026, 12, 18).unwrap(),
            lines_core::Price(45_050),
            lines_core::Right::Call,
        );
        assert_eq!(
            doc(opt),
            r#"{"product":2001,"option":{"expiry":20261218,"strike":45050,"right":"call"}}"#
        );
        assert_eq!(
            doc(ContractKey::equity(catalog::SPY)),
            r#"{"product":2001,"equity":true}"#
        );
        for k in [
            es,
            ContractKey::spot(catalog::BTC_USD),
            ContractKey::equity(catalog::SPY),
            opt,
        ] {
            let back: ContractDoc = serde_json::from_str(&doc(k)).unwrap();
            assert_eq!(back.to_key(), Some(k));
        }
        for bad in [
            r#"{"product":2001,"equity":true,"year":2026,"month":12}"#,
            r#"{"product":2001,"equity":true,"option":{"expiry":20261218,"strike":1,"right":"put"}}"#,
            r#"{"product":2001,"option":{"expiry":20261318,"strike":1,"right":"put"}}"#,
            r#"{"product":2001,"option":{"expiry":20261218,"strike":0,"right":"put"}}"#,
        ] {
            let d: ContractDoc = serde_json::from_str(bad).unwrap();
            assert_eq!(d.to_key(), None, "{bad}");
        }
    }

    /// An option's money: premium units × 100 shares (SPY: $0.01 × 100 = $1 per unit).
    #[test]
    fn option_money_uses_the_multiplier() {
        let spy = catalog::get(catalog::SPY).unwrap();
        let opt = ContractKey::option(
            catalog::SPY,
            lines_core::CivilDate::new(2026, 12, 18).unwrap(),
            lines_core::Price(45_000),
            lines_core::Right::Put,
        );
        let o = ContractSpec::new(spy.clone(), opt, Money::ZERO).unwrap();
        // 2 contracts, premium up 1.25 (125 units): $250.
        assert_eq!(o.money(125, 2), Money::whole(250));
        let shares =
            ContractSpec::new(spy, ContractKey::equity(catalog::SPY), Money::ZERO).unwrap();
        assert_eq!(shares.money(125, 2), Money(Money::whole(5).0 / 2), "$2.50");
        let es = catalog::get(catalog::ES).unwrap();
        let es_opt = ContractKey::option(
            catalog::ES,
            lines_core::CivilDate::new(2026, 12, 18).unwrap(),
            lines_core::Price(600_000),
            lines_core::Right::Call,
        );
        assert!(
            ContractSpec::new(es, es_opt, Money::ZERO).is_none(),
            "no listed options"
        );
    }

    #[test]
    fn money_factors_are_exact_for_every_catalog_product() {
        for p in catalog::all() {
            let k = p
                .contracts_from(lines_core::CivilDate::new(2026, 11, 6).unwrap())
                .next()
                .unwrap();
            assert!(
                ContractSpec::new(p.clone(), k, Money::ZERO).is_some(),
                "{}",
                p.symbol
            );
        }
        let es = catalog::get(catalog::ES).unwrap();
        let k = es
            .contracts_from(lines_core::CivilDate::new(2026, 11, 6).unwrap())
            .next()
            .unwrap();
        let s = ContractSpec::new(es, k, Money::ZERO).unwrap();
        // One ES tick (25 units of 0.01) on one contract = $12.50.
        assert_eq!(s.money(25, 1), Money::cents(1250));
        assert_eq!(format!("{}", Money::cents(-1250)), "-12.50");
        assert_eq!(
            format!("{}", Money(Money::UNITS_PER_WHOLE / 1000 * 5)),
            "0.01"
        );
    }

    #[test]
    fn contract_docs_round_trip() {
        let k = ContractKey::monthly(ProductId(1), 2026, Month::new(12).unwrap());
        assert_eq!(ContractDoc::from_key(k).to_key(), Some(k));
        let s = ContractKey::spot(ProductId(1001));
        assert_eq!(ContractDoc::from_key(s).to_key(), Some(s));
    }
}
