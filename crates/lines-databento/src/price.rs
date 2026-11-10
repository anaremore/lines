//! Exact price conversion: Databento's fixed-precision prices (1 unit = 1e-9) into the catalog
//! product's price units. Nothing is rounded: a value that isn't a whole number of units, or
//! isn't on the tick grid, is refused and counted by the caller (CLAUDE.md invariants).

use lines_core::{Price, Product};

/// Databento's price denominator (`dbn::FIXED_PRICE_SCALE`).
pub const NANO: i128 = 1_000_000_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PriceError {
    /// `UNDEF_PRICE` (i64::MAX): no price.
    Undefined,
    /// Not a whole number of the product's price units.
    OffGrid,
    /// A whole number of units, but not a multiple of the tick.
    OffTick,
}

#[derive(Copy, Clone, Debug)]
pub struct PriceConv {
    /// Units = nanos × num / den, exactly.
    num: i128,
    den: i128,
    tick: i64,
}

impl PriceConv {
    pub fn new(product: &Product) -> Self {
        let (snum, sden) = (
            product.price.scale.num() as i128,
            product.price.scale.den() as i128,
        );
        // price = nanos / 1e9 = units × snum / sden  →  units = nanos × sden / (1e9 × snum)
        PriceConv {
            num: sden,
            den: NANO * snum,
            tick: product.price.tick,
        }
    }

    /// Exact units, or why not (not checking the tick).
    pub fn units(&self, nanos: i64) -> Result<i64, PriceError> {
        if nanos == i64::MAX {
            return Err(PriceError::Undefined);
        }
        let n = nanos as i128 * self.num;
        if n % self.den != 0 {
            return Err(PriceError::OffGrid);
        }
        i64::try_from(n / self.den).map_err(|_| PriceError::OffGrid)
    }

    /// An order-book or trade price: exact and on the tick grid.
    pub fn price(&self, nanos: i64) -> Result<Price, PriceError> {
        let u = self.units(nanos)?;
        if u % self.tick != 0 {
            return Err(PriceError::OffTick);
        }
        Ok(Price(u))
    }

    /// The product's tick in Databento nanos (for checking `min_price_increment`).
    pub fn tick_nanos(&self) -> Option<i64> {
        let n = self.tick as i128 * self.den;
        (n % self.num == 0)
            .then(|| i64::try_from(n / self.num).ok())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::catalog;

    #[test]
    fn es_prices_convert_exactly() {
        let c = PriceConv::new(&catalog::by_symbol("ES").unwrap());
        // 6000.25 = 6_000_250_000_000 nanos = 600025 units.
        assert_eq!(c.price(6_000_250_000_000), Ok(Price(600025)));
        assert_eq!(c.price(-250_000_000), Ok(Price(-25)));
        assert_eq!(c.price(0), Ok(Price(0)));
        // 6000.10: whole units but off the 0.25 tick.
        assert_eq!(c.price(6_000_100_000_000), Err(PriceError::OffTick));
        // 6000.125: not a whole unit (1/100).
        assert_eq!(c.price(6_000_125_000_000), Err(PriceError::OffGrid));
        assert_eq!(c.price(1), Err(PriceError::OffGrid));
        assert_eq!(c.price(i64::MAX), Err(PriceError::Undefined));
        assert_eq!(c.tick_nanos(), Some(250_000_000));
    }

    #[test]
    fn every_futures_product_has_an_exact_tick() {
        for p in catalog::all()
            .iter()
            .filter(|p| !p.is_spot() && !p.is_equity())
        {
            let c = PriceConv::new(p);
            let t = c.tick_nanos().expect(p.symbol);
            assert_eq!(c.price(t), Ok(Price(p.price.tick)), "{}", p.symbol);
        }
        // The gateway's ticks (2026-11-09) for a few scales: 1/256 (ZT), 1/8 cent (ZC), 1e-7.
        let tick = |s| PriceConv::new(&catalog::by_symbol(s).unwrap()).tick_nanos();
        assert_eq!(tick("ZT"), Some(3_906_250));
        assert_eq!(tick("ZC"), Some(250_000_000));
        assert_eq!(tick("6J"), Some(500));
        assert_eq!(tick("HG"), Some(500_000));
        // ZC 432.25 cents = 3458 eighths; 6J 0.0065455 = 65455 units.
        let price = |s, n| PriceConv::new(&catalog::by_symbol(s).unwrap()).price(n);
        assert_eq!(price("ZC", 432_250_000_000), Ok(Price(3458)));
        assert_eq!(price("ZC", 432_125_000_000), Err(PriceError::OffTick));
        assert_eq!(price("6J", 6_545_500), Ok(Price(65_455)));
    }
}
