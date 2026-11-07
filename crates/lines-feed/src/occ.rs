//! OCC option symbols (the OSI 21-character form), derived from a [`ContractKey`] by rule
//! (decisions 0013, 0028).
//!
//! `SPY   261218C00450500`: the root padded to 6 characters with spaces, the expiry as
//! `YYMMDD`, `C` or `P`, and the strike × 1000 as 8 digits. This is the industry's standard
//! symbology (published by the Options Clearing Corporation), not one vendor's, so it is
//! public here for every provider and extension; a provider whose symbols differ derives its
//! own from the key, as for futures.
//!
//! Exact: the strike goes through the product's price scale with integer arithmetic, and a
//! strike that isn't a whole number of thousandths (or doesn't fit 8 digits) has no symbol.

use lines_core::{CivilDate, ContractKey, Delivery, Price, Product, Right};

use crate::catalog;

/// The OCC root: the underlying's catalog symbol (adjusted series with other roots are not
/// modelled).
fn root(product: &Product) -> Option<&'static str> {
    (product.is_equity() && product.symbol.len() <= 6).then_some(product.symbol)
}

/// The 21-character OCC symbol of an option key (`None` for any other key, a product without
/// options, or a strike OCC can't write).
pub fn occ_symbol(product: &Product, key: &ContractKey) -> Option<String> {
    let Delivery::Option(o) = key.delivery else {
        return None;
    };
    if key.product != product.id || product.options().is_none() || o.strike.0 <= 0 {
        return None;
    }
    let root = root(product)?;
    // strike × 1000 = units × num × 1000 / den, exactly.
    let scale = product.price.scale;
    let n = i128::from(o.strike.0) * i128::from(scale.num()) * 1000;
    let den = i128::from(scale.den());
    if n % den != 0 {
        return None;
    }
    let milli = n / den;
    if !(1..=99_999_999).contains(&milli) || o.expiry.year < 2000 || o.expiry.year > 2099 {
        return None;
    }
    Some(format!(
        "{root:<6}{:02}{:02}{:02}{}{milli:08}",
        o.expiry.year % 100,
        o.expiry.month,
        o.expiry.day,
        o.right.letter()
    ))
}

/// Parse an OCC symbol, padded (`SPY   261218C00450500`) or not (`SPY261218C00450500`), for
/// a catalog equity with listed options. `None` for anything else, or a strike off the
/// product's price units.
pub fn parse_occ(s: &str) -> Option<ContractKey> {
    let s = s.trim_end();
    if !s.is_ascii() || s.len() < 16 || s.len() > 21 {
        return None;
    }
    let (root, rest) = s.split_at(s.len() - 15);
    let root = root.trim_end();
    let product = catalog::by_symbol(root).filter(|p| p.symbol == root)?;
    product.options()?;
    let digits = |r: &str| -> Option<u32> {
        r.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| r.parse().ok())
            .flatten()
    };
    let (yy, mm, dd) = (
        digits(&rest[0..2])?,
        digits(&rest[2..4])?,
        digits(&rest[4..6])?,
    );
    let expiry = CivilDate::new(2000 + yy as u16, mm as u8, dd as u8)?;
    let right = Right::from_letter(rest[6..7].chars().next()?)?;
    let milli = i128::from(digits(&rest[7..15])?);
    // units = strike × 1000 × den / (num × 1000).
    let scale = product.price.scale;
    let n = milli * i128::from(scale.den());
    let d = i128::from(scale.num()) * 1000;
    if milli == 0 || n % d != 0 {
        return None;
    }
    let strike = Price(i64::try_from(n / d).ok()?);
    Some(ContractKey::option(product.id, expiry, strike, right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spy_opt(y: u16, m: u8, d: u8, strike: i64, right: Right) -> ContractKey {
        ContractKey::option(
            catalog::SPY,
            CivilDate::new(y, m, d).unwrap(),
            Price(strike),
            right,
        )
    }

    #[test]
    fn occ_symbols_round_trip_exactly() {
        let spy = catalog::get(catalog::SPY).unwrap();
        let qqq = catalog::get(catalog::QQQ).unwrap();
        for (p, k, occ) in [
            (
                &spy,
                spy_opt(2026, 12, 18, 45_000, Right::Call),
                "SPY   261218C00450000",
            ),
            (
                &spy,
                spy_opt(2026, 11, 9, 45_050, Right::Put),
                "SPY   261109P00450500",
            ),
            (
                &spy,
                spy_opt(2027, 1, 15, 1, Right::Call),
                "SPY   270115C00000010",
            ),
            (
                &qqq,
                ContractKey::option(
                    catalog::QQQ,
                    CivilDate::new(2026, 12, 31).unwrap(),
                    Price(52_150),
                    Right::Put,
                ),
                "QQQ   261231P00521500",
            ),
        ] {
            assert_eq!(occ_symbol(p, &k).as_deref(), Some(occ));
            assert_eq!(occ.len(), 21);
            assert_eq!(parse_occ(occ), Some(k), "{occ}");
            assert_eq!(parse_occ(&occ.replace(' ', "")), Some(k), "unpadded");
        }
    }

    #[test]
    fn what_has_no_occ_symbol() {
        let spy = catalog::get(catalog::SPY).unwrap();
        let es = catalog::get(catalog::ES).unwrap();
        assert_eq!(occ_symbol(&spy, &ContractKey::equity(catalog::SPY)), None);
        let es_key = ContractKey::option(
            catalog::ES,
            CivilDate::new(2026, 12, 18).unwrap(),
            Price(600_000),
            Right::Call,
        );
        assert_eq!(occ_symbol(&es, &es_key), None, "no listed options");
        assert_eq!(
            occ_symbol(&spy, &spy_opt(2026, 12, 18, 0, Right::Call)),
            None,
            "a zero strike"
        );
        assert_eq!(
            occ_symbol(&spy, &spy_opt(2026, 12, 18, 10_000_000_000, Right::Call)),
            None,
            "too many digits"
        );
        for bad in [
            "",
            "SPY",
            "ES    261218C06000000",
            "SPY   261318C00450000",
            "SPY   261218X00450000",
            "SPY   261218C0045000x",
            "SPY   261218C00000000",
            "SPY   261218C00450001",
            "ÉSPY  261218C00450000",
            "TOOLONGROOT261218C00450000",
        ] {
            assert_eq!(parse_occ(bad), None, "{bad:?}");
        }
    }
}
