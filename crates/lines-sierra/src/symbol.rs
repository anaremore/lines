//! Sierra Chart symbol → catalog instrument (decision 0003). Sierra names never leave this
//! crate; the output recording names only the catalog instrument.
//!
//! Seen on disk: `NQZ5.CME`, `MNQZ6.CME`, `NQH25-CME`, `NQH25_FUT_CME`, and single-digit-year
//! files such as `NQH2.CME` (ambiguous decade).

use lines_core::{ContractKey, Month, Product, ProductKind};
use lines_feed::catalog;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SierraSymbol {
    pub root: String,
    pub month: char,
    /// Year digits as written (1 or 2 digits); the decade of 1-digit years is ambiguous.
    pub year: String,
}

const MONTH_CODES: &str = "FGHJKMNQUVXZ";

pub fn parse(symbol: &str) -> Option<SierraSymbol> {
    // Strip the exchange suffix: ".CME", "-CME", "_FUT_CME".
    let base = symbol.split(['.', '-']).next()?;
    let base = base.strip_suffix("_FUT_CME").unwrap_or(base);
    let base = base.split('_').next()?;
    let digits = base
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .count();
    if !(1..=2).contains(&digits) || base.len() < digits + 2 {
        return None;
    }
    let (head, year) = base.split_at(base.len() - digits);
    let month = head.chars().last()?;
    if !MONTH_CODES.contains(month) {
        return None;
    }
    let root = &head[..head.len() - 1];
    if root.is_empty() || !root.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    Some(SierraSymbol {
        root: root.to_owned(),
        month,
        year: year.to_owned(),
    })
}

/// Catalog instrument and the Sierra price multiplier for a root.
///
/// The multiplier is Sierra's per-symbol "price multiplier": files store `price × m` as f32.
/// Observed on the owner's Apex/Rithmic data: NQ stored ×100 (24128.75 → 2412875).
/// Assumed the same for ES/MES/MNQ until checked; `--price-multiplier` overrides it, and
/// off-grid rejections would expose a wrong value.
pub fn map_root(root: &str) -> Option<(Product, f64)> {
    let id = match root {
        "ES" => catalog::ES,
        "NQ" => catalog::NQ,
        "MES" => catalog::MES,
        "MNQ" => catalog::MNQ,
        _ => return None,
    };
    Some((catalog::get(id)?, 100.0))
}

/// The contract a Sierra file holds (decision 0013): month from the code, year from the
/// digits. Two digits mean 20YY. A single digit is resolved from the data: the earliest year
/// ending in that digit that is not before `last_data_year`, because a contract stops
/// trading at expiry, so its last record falls in or before its delivery year.
///
/// This differs from decision 0013's wording ("nearest decade at or before the file's data
/// dates"), which picks the wrong decade for a contract expiring early in a decade whose data
/// was recorded in the previous one: `NQH0` traded in December 2029 is the March 2030
/// contract, not March 2020.
pub fn contract_for(
    sym: &SierraSymbol,
    product: &Product,
    last_data_year: u16,
) -> Option<ContractKey> {
    let month = Month::from_code(sym.month)?;
    let year = match sym.year.len() {
        2 => 2000 + sym.year.parse::<u16>().ok()?,
        1 => {
            let d: u16 = sym.year.parse().ok()?;
            let mut y = last_data_year - last_data_year % 10 + d;
            if y < last_data_year {
                y += 10;
            }
            y
        }
        _ => return None,
    };
    let key = ContractKey::monthly(product.id, year, month);
    // Must be a listed month of the product (e.g. ES has no January contract).
    match product.kind {
        ProductKind::Future { cycle, .. } if cycle.contains(month) => Some(key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_seen_forms() {
        let p = |s| parse(s).map(|x| (x.root, x.month, x.year));
        assert_eq!(p("NQZ5.CME"), Some(("NQ".into(), 'Z', "5".into())));
        assert_eq!(p("MNQZ6.CME"), Some(("MNQ".into(), 'Z', "6".into())));
        assert_eq!(p("NQH25-CME"), Some(("NQ".into(), 'H', "25".into())));
        assert_eq!(p("NQH25_FUT_CME"), Some(("NQ".into(), 'H', "25".into())));
        assert_eq!(p("MESZ24-CME"), Some(("MES".into(), 'Z', "24".into())));
        assert_eq!(p("NQ.CME"), None);
        assert_eq!(p("NQA5.CME"), None, "A is not a month code");
        assert_eq!(p("Z5.CME"), None);
    }

    #[test]
    fn contracts_from_file_names() {
        let label = |s: &str, last_year: u16| {
            let sym = parse(s).unwrap();
            let (p, _) = map_root(&sym.root).unwrap();
            contract_for(&sym, &p, last_year).map(|k| k.label(&p).as_str().to_owned())
        };
        assert_eq!(label("NQZ5.CME", 2025).as_deref(), Some("NQZ25"));
        assert_eq!(label("NQH25-CME", 2025).as_deref(), Some("NQH25"));
        assert_eq!(label("NQH25_FUT_CME", 2025).as_deref(), Some("NQH25"));
        assert_eq!(label("MNQZ6.CME", 2026).as_deref(), Some("MNQZ26"));
        // Data recorded only in the year before expiry still resolves forward.
        assert_eq!(label("NQH5.CME", 2024).as_deref(), Some("NQH25"));
        // The decade boundary case the decision's wording gets wrong.
        assert_eq!(label("NQH0.CME", 2029).as_deref(), Some("NQH30"));
        assert_eq!(label("NQH2.CME", 2022).as_deref(), Some("NQH22"));
        assert_eq!(
            label("NQF6.CME", 2026),
            None,
            "January is not a listed NQ month"
        );
    }

    #[test]
    fn maps_roots_to_catalog() {
        assert_eq!(map_root("NQ").unwrap().0.symbol, "NQ");
        assert_eq!(map_root("MNQ").unwrap().0.symbol, "MNQ");
        assert_eq!(map_root("MES").unwrap().0.symbol, "MES");
        assert!(map_root("CL").is_none());
    }
}
