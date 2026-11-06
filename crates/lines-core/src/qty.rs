//! Exact quantities (decision 0002): an integer count of `10^-decimals` of the instrument's
//! base unit. Futures use `decimals = 0` (whole contracts); spot crypto uses e.g. 8.

use std::fmt;

use crate::price::{UnitError, parse_decimal_text};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Qty(pub i64);

impl Qty {
    pub const ZERO: Qty = Qty(0);

    /// Saturating: a bar's volume can't realistically reach i64::MAX units, and an
    /// overflow must not wrap into a negative volume.
    #[inline]
    pub fn saturating_add(self, other: Qty) -> Qty {
        Qty(self.0.saturating_add(other.0))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct QtyFormat {
    pub decimals: u8,
}

impl QtyFormat {
    pub const WHOLE: QtyFormat = QtyFormat { decimals: 0 };

    /// Exact units for decimal text such as `"0.00123456"`.
    pub fn parse_decimal(self, s: &str) -> Result<Qty, UnitError> {
        let (m, k) = parse_decimal_text(s)?;
        self.from_parts(m, -(k as i32)).map_err(|e| match e {
            UnitError::NotMultiple(_) => UnitError::NotMultiple(s.to_owned()),
            e => e,
        })
    }

    /// Exact units for `significand * 10^exponent` (e.g. CQG `cqg.Decimal`).
    pub fn from_parts(self, significand: i128, exponent: i32) -> Result<Qty, UnitError> {
        let shift = exponent + self.decimals as i32;
        let v = if shift >= 0 {
            10i128
                .checked_pow(shift as u32)
                .and_then(|p| significand.checked_mul(p))
                .ok_or(UnitError::Overflow)?
        } else {
            let p = 10i128
                .checked_pow(shift.unsigned_abs())
                .ok_or(UnitError::Overflow)?;
            if significand % p != 0 {
                return Err(UnitError::NotMultiple(format!("{significand}e{exponent}")));
            }
            significand / p
        };
        i64::try_from(v).map(Qty).map_err(|_| UnitError::Overflow)
    }

    /// Write without allocating; trailing fraction zeros are trimmed (`1.5`, not `1.50000000`).
    pub fn write<W: fmt::Write>(self, q: Qty, out: &mut W) -> fmt::Result {
        if self.decimals == 0 {
            return write!(out, "{}", q.0);
        }
        let pow = 10i64.pow(self.decimals as u32);
        if q.0 < 0 {
            out.write_char('-')?;
        }
        let a = q.0.unsigned_abs();
        let (whole, mut frac, mut width) = (a / pow as u64, a % pow as u64, self.decimals as usize);
        write!(out, "{whole}")?;
        if frac == 0 {
            return Ok(());
        }
        while frac % 10 == 0 {
            frac /= 10;
            width -= 1;
        }
        write!(out, ".{frac:0width$}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedStr;

    fn show(f: QtyFormat, q: i64) -> String {
        let mut s = FixedStr::<40>::new();
        f.write(Qty(q), &mut s).unwrap();
        s.as_str().to_owned()
    }

    #[test]
    fn parse_and_print_fractional() {
        let btc = QtyFormat { decimals: 8 };
        assert_eq!(btc.parse_decimal("0.00123456"), Ok(Qty(123_456)));
        assert_eq!(btc.parse_decimal("30.69151416"), Ok(Qty(3_069_151_416)));
        assert_eq!(btc.parse_decimal("2"), Ok(Qty(200_000_000)));
        assert!(
            btc.parse_decimal("0.000000001").is_err(),
            "finer than the instrument allows"
        );
        assert_eq!(show(btc, 123_456), "0.00123456");
        assert_eq!(show(btc, 150_000_000), "1.5");
        assert_eq!(show(btc, 200_000_000), "2");
        assert_eq!(show(btc, -50_000_000), "-0.5");
    }

    #[test]
    fn whole_contracts() {
        let c = QtyFormat::WHOLE;
        assert_eq!(c.from_parts(5, 0), Ok(Qty(5)));
        assert_eq!(c.from_parts(5, 2), Ok(Qty(500)));
        assert_eq!(
            c.from_parts(1500, -3),
            Err(UnitError::NotMultiple("1500e-3".into()))
        );
        assert_eq!(c.from_parts(2000, -3), Ok(Qty(2)));
        assert_eq!(show(c, 42), "42");
    }

    #[test]
    fn saturates() {
        assert_eq!(Qty(i64::MAX).saturating_add(Qty(1)), Qty(i64::MAX));
    }
}
