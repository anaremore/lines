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

use lines_core::{
    ContractKey, Currency, DisplayStyle, ExpiryRule, MicroEquivalent, MonthCycle, PriceFormat,
    PriceScale, Product, ProductId, ProductKind, QtyFormat, Ratio, RollRule,
};

pub const ES: ProductId = ProductId(1);
pub const NQ: ProductId = ProductId(2);
pub const ZN: ProductId = ProductId(3);
pub const MES: ProductId = ProductId(4);
pub const MNQ: ProductId = ProductId(5);
pub const BTC_USD: ProductId = ProductId(1001);
pub const ETH_USD: ProductId = ProductId(1002);

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
    Product {
        id,
        symbol,
        description,
        price: decimal(2, 1),
        qty: QtyFormat { decimals: 8 },
        currency: Currency::USD,
        point_value: Ratio::new(1, 1),
        kind: ProductKind::Spot,
        micro: None,
    }
}

pub fn all() -> [Product; 7] {
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
    ]
}

pub fn get(id: ProductId) -> Option<Product> {
    all().into_iter().find(|p| p.id == id)
}

/// Case-insensitive lookup by product symbol; `BTC-USD` is accepted for `BTC/USD`.
pub fn by_symbol(symbol: &str) -> Option<Product> {
    let want = symbol.trim().replace('-', "/");
    all()
        .into_iter()
        .find(|p| p.symbol.eq_ignore_ascii_case(&want))
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
    use lines_core::{Month, Qty};

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
}
