//! Built-in instrument catalog: stable internal ids and exact, provider-neutral specs.
//!
//! Each instrument's price unit and tick are defined here, once. Providers must convert their
//! wire values into these units and reject metadata that disagrees (decision 0002). Vendor
//! symbols are NOT here; each provider maps ids to its own symbols (decision 0003).
//!
//! Ids are permanent: never renumber or reuse one.

use lines_core::{
    Currency, DisplayStyle, Instrument, InstrumentId, PriceFormat, PriceScale, QtyFormat,
};

pub const ES: InstrumentId = InstrumentId(1);
pub const NQ: InstrumentId = InstrumentId(2);
pub const ZN: InstrumentId = InstrumentId(3);
pub const MES: InstrumentId = InstrumentId(4);
pub const MNQ: InstrumentId = InstrumentId(5);
pub const BTC_USD: InstrumentId = InstrumentId(1001);
pub const ETH_USD: InstrumentId = InstrumentId(1002);

fn decimal(digits: u32, tick_units: i64) -> PriceFormat {
    PriceFormat::new(
        PriceScale::decimal(digits),
        DisplayStyle::Decimal {
            digits: digits as u8,
        },
        tick_units,
    )
}

pub fn all() -> [Instrument; 7] {
    [
        Instrument {
            id: ES,
            symbol: "ES",
            description: "E-mini S&P 500",
            // Unit 0.01 (finer than the 0.25 tick) so off-tick values such as some
            // settlements remain representable.
            price: decimal(2, 25),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
        },
        Instrument {
            id: NQ,
            symbol: "NQ",
            description: "E-mini Nasdaq-100",
            price: decimal(2, 25),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
        },
        Instrument {
            id: MES,
            symbol: "MES",
            description: "Micro E-mini S&P 500",
            price: decimal(2, 25),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
        },
        Instrument {
            id: MNQ,
            symbol: "MNQ",
            description: "Micro E-mini Nasdaq-100",
            price: decimal(2, 25),
            qty: QtyFormat::WHOLE,
            currency: Currency::USD,
        },
        Instrument {
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
        },
        Instrument {
            id: BTC_USD,
            symbol: "BTC/USD",
            description: "Bitcoin / US Dollar (spot)",
            price: decimal(2, 1),
            qty: QtyFormat { decimals: 8 },
            currency: Currency::USD,
        },
        Instrument {
            id: ETH_USD,
            symbol: "ETH/USD",
            description: "Ether / US Dollar (spot)",
            price: decimal(2, 1),
            qty: QtyFormat { decimals: 8 },
            currency: Currency::USD,
        },
    ]
}

pub fn get(id: InstrumentId) -> Option<Instrument> {
    all().into_iter().find(|i| i.id == id)
}

/// Case-insensitive lookup by internal symbol; `BTC-USD` is accepted for `BTC/USD`.
pub fn by_symbol(symbol: &str) -> Option<Instrument> {
    let want = symbol.trim().replace('-', "/");
    all()
        .into_iter()
        .find(|i| i.symbol.eq_ignore_ascii_case(&want))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn lookup() {
        assert_eq!(by_symbol("es").unwrap().id, ES);
        assert_eq!(by_symbol("BTC-USD").unwrap().id, BTC_USD);
        assert_eq!(get(ZN).unwrap().symbol, "ZN");
        assert!(
            by_symbol("EP").is_none(),
            "vendor symbols are not catalog symbols"
        );
    }
}
