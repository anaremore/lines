//! Exact, vendor-neutral prices (decision 0002).
//!
//! A [`Price`] is an integer count of the *instrument's* price unit. Each instrument defines
//! that unit as an exact rational [`PriceScale`] (e.g. 1/100 for ES, so 4925.25 is
//! `Price(492_525)`). Providers convert their wire format into these units once, at their
//! edge, using the exact conversions here: rational rescaling ([`UnitConverter`], e.g. CQG
//! scaled integers), decimal text ([`PriceScale::parse_decimal`], e.g. Coinbase JSON), or
//! doubles snapped to the unit grid ([`PriceScale::units_from_f64`], e.g. Rithmic).
//! `f64` appears only in viewport/pixel math.

use std::fmt;

use crate::fixed_str::FixedStr;

/// A price as an integer count of the instrument's price unit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Price(pub i64);

impl Price {
    #[inline]
    pub fn units(self) -> i64 {
        self.0
    }
}

/// Exact rational size of one price unit: `num / den` in the quote currency.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PriceScale {
    num: i64,
    den: i64,
}

/// Failure to represent a vendor value exactly in instrument units.
#[derive(Debug, Clone, PartialEq)]
pub enum UnitError {
    NotFinite(f64),
    NotPositive(f64),
    /// No denominator of the form 2^a * 10^b (<= 10^15) represents the double exactly.
    NotRational(f64),
    /// The value is not a whole number of units (e.g. an off-grid price).
    NotMultiple(String),
    /// Malformed decimal text.
    Parse(String),
    Overflow,
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::NotFinite(v) => write!(f, "{v} is not finite"),
            UnitError::NotPositive(v) => write!(f, "{v} is not positive"),
            UnitError::NotRational(v) => write!(f, "{v} has no exact 2^a*10^b representation"),
            UnitError::NotMultiple(v) => write!(f, "{v} is not a whole number of units"),
            UnitError::Parse(v) => write!(f, "malformed decimal {v:?}"),
            UnitError::Overflow => write!(f, "value out of range"),
        }
    }
}
impl std::error::Error for UnitError {}

const MAX_DEN: i64 = 1_000_000_000_000_000; // 10^15

impl PriceScale {
    pub const ONE: PriceScale = PriceScale { num: 1, den: 1 };

    pub fn new(num: i64, den: i64) -> Self {
        assert!(num > 0 && den > 0, "PriceScale must be positive");
        let g = gcd(num, den);
        PriceScale {
            num: num / g,
            den: den / g,
        }
    }

    /// `10^-digits`, the common case for decimal-priced instruments.
    pub fn decimal(digits: u32) -> Self {
        PriceScale::new(1, 10i64.pow(digits))
    }

    /// Convert a scale a vendor sends as a double into an exact rational.
    ///
    /// Price scales are of the form `n / (2^a * 10^b)` (decimal ticks, binary fractions such
    /// as 1/32, or a mix). We keep the smallest such denominator whose correctly-rounded
    /// quotient is *bit-identical* to the double: the vendor's double is the nearest double
    /// to the true rational, and IEEE division of two exact integers is correctly rounded.
    /// (A tolerance would wrongly accept 1/3 ≈ 333…/10^15.)
    pub fn from_f64(scale: f64) -> Result<Self, UnitError> {
        if !scale.is_finite() {
            return Err(UnitError::NotFinite(scale));
        }
        if scale <= 0.0 {
            return Err(UnitError::NotPositive(scale));
        }
        let mut best: Option<PriceScale> = None;
        let mut pow2 = 1i64;
        while pow2 <= 4096 {
            let mut pow10 = 1i64;
            loop {
                let den = pow2.saturating_mul(pow10);
                if den > MAX_DEN {
                    break;
                }
                let num_f = (scale * den as f64).round();
                if (1.0..9.0e15).contains(&num_f) {
                    let num = num_f as i64;
                    if num as f64 / den as f64 == scale {
                        let cand = PriceScale::new(num, den);
                        if best.is_none_or(|b| cand.den < b.den) {
                            best = Some(cand);
                        }
                    }
                }
                pow10 *= 10;
            }
            pow2 *= 2;
        }
        best.ok_or(UnitError::NotRational(scale))
    }

    pub fn num(self) -> i64 {
        self.num
    }
    pub fn den(self) -> i64 {
        self.den
    }

    /// Lossy conversion for display geometry only. Never feed this back into a `Price`.
    #[inline]
    pub fn to_f64(self, p: Price) -> f64 {
        p.0 as f64 * self.num as f64 / self.den as f64
    }

    /// Exact units for an *exact* quantity given as a double (e.g. a tick size of 0.25).
    /// The double is first recovered as an exact rational, so this never rounds.
    pub fn exact_units_from_f64(self, value: f64) -> Result<i64, UnitError> {
        let r = PriceScale::from_f64(value)?;
        UnitConverter::new(r, self)
            .convert(1)
            .ok_or_else(|| UnitError::NotMultiple(value.to_string()))
    }

    /// Snap a *price* double (e.g. Rithmic's wire format) to the unit grid. Accepts only
    /// doubles within float noise of a grid point, so genuinely off-grid prices are rejected
    /// instead of silently rounded.
    pub fn units_from_f64(self, value: f64) -> Result<i64, UnitError> {
        if !value.is_finite() {
            return Err(UnitError::NotFinite(value));
        }
        let u = value * self.den as f64 / self.num as f64;
        let r = u.round();
        if r.abs() >= 9.0e18 {
            return Err(UnitError::Overflow);
        }
        let tolerance = (u.abs() * 8.0 * f64::EPSILON).max(1e-9);
        if (u - r).abs() > tolerance {
            return Err(UnitError::NotMultiple(value.to_string()));
        }
        Ok(r as i64)
    }

    /// Exact units for decimal text such as `"75415.72"` or `"-0.5"` (no exponents).
    pub fn parse_decimal(self, s: &str) -> Result<i64, UnitError> {
        let (mantissa, frac_digits) = parse_decimal_text(s)?;
        // units = mantissa / 10^k / (num/den) = mantissa * den / (10^k * num)
        let numer = mantissa
            .checked_mul(self.den as i128)
            .ok_or(UnitError::Overflow)?;
        let denom = 10i128
            .checked_pow(frac_digits)
            .and_then(|p| p.checked_mul(self.num as i128))
            .ok_or(UnitError::Overflow)?;
        if numer % denom != 0 {
            return Err(UnitError::NotMultiple(s.to_owned()));
        }
        i64::try_from(numer / denom).map_err(|_| UnitError::Overflow)
    }
}

/// Parse `[-+]digits[.digits]` into (mantissa, number of fraction digits).
pub(crate) fn parse_decimal_text(s: &str) -> Result<(i128, u32), UnitError> {
    let t = s.trim();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || int.len() + frac.len() > 36
    {
        return Err(UnitError::Parse(s.to_owned()));
    }
    let mut m: i128 = 0;
    for b in int.bytes().chain(frac.bytes()) {
        m = m
            .checked_mul(10)
            .and_then(|m| m.checked_add((b - b'0') as i128))
            .ok_or(UnitError::Overflow)?;
    }
    Ok((if neg { -m } else { m }, frac.len() as u32))
}

/// Exact rescaling between two unit systems: `to_units = from_units * mul / div`.
/// Used at provider edges, e.g. CQG `scaled_price` (CQG scale) → instrument units.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UnitConverter {
    mul: i64,
    div: i64,
}

impl UnitConverter {
    pub fn new(from: PriceScale, to: PriceScale) -> Self {
        // value = u_from * from.num/from.den = u_to * to.num/to.den
        // => u_to = u_from * (from.num * to.den) / (from.den * to.num)
        let mul = from.num as i128 * to.den as i128;
        let div = from.den as i128 * to.num as i128;
        let g = gcd128(mul, div);
        UnitConverter {
            mul: i64::try_from(mul / g).expect("scale ratio overflow"),
            div: i64::try_from(div / g).expect("scale ratio overflow"),
        }
    }

    pub fn is_identity(self) -> bool {
        self.mul == 1 && self.div == 1
    }

    /// `None` if the value is not a whole number of target units (or overflows).
    #[inline]
    pub fn convert(self, from_units: i64) -> Option<i64> {
        let n = from_units as i128 * self.mul as i128;
        if n % self.div as i128 != 0 {
            return None;
        }
        i64::try_from(n / self.div as i128).ok()
    }
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.abs().max(1)
}

fn gcd128(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.abs().max(1)
}

/// How prices are rendered as text.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DisplayStyle {
    /// Plain decimal with this many fraction digits.
    Decimal { digits: u8 },
    /// Whole + `'` + numerator of `1/denominator` (treasury 32nds: `112'16`).
    Fraction { denominator: u16 },
    /// Fractions of a fraction, "truncated" style: quarter-32nds 112 + 16.75/32 → `112'167`.
    SubFraction { denominator: u16, sub: u16 },
}

/// Everything needed to interpret and print prices for one instrument.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PriceFormat {
    pub scale: PriceScale,
    pub style: DisplayStyle,
    /// Minimum price increment, in units.
    pub tick: i64,
}

impl PriceFormat {
    pub fn new(scale: PriceScale, style: DisplayStyle, tick: i64) -> Self {
        assert!(tick > 0, "tick must be positive");
        PriceFormat { scale, style, tick }
    }

    /// Write the price without allocating. Exact: integer arithmetic only.
    pub fn write<W: fmt::Write>(&self, p: Price, out: &mut W) -> fmt::Result {
        let (num, den) = (self.scale.num as i128, self.scale.den as i128);
        let v = p.0 as i128 * num; // real value = v / den
        let neg = v < 0;
        let v = v.abs();
        if neg {
            out.write_char('-')?;
        }
        match self.style {
            DisplayStyle::Decimal { digits } => {
                let pow = 10i128.pow(digits as u32);
                let scaled = v * pow;
                let q = scaled / den;
                let rem = scaled % den;
                // Round half away from zero if the unit is finer than the display digits.
                let q = if rem * 2 >= den { q + 1 } else { q };
                write!(out, "{}", q / pow)?;
                if digits > 0 {
                    write!(out, ".{:0width$}", q % pow, width = digits as usize)?;
                }
                Ok(())
            }
            DisplayStyle::Fraction { denominator } => {
                let d = denominator as i128;
                let total = v * d / den;
                write!(
                    out,
                    "{}'{:0width$}",
                    total / d,
                    total % d,
                    width = digits_of(d - 1)
                )
            }
            DisplayStyle::SubFraction { denominator, sub } => {
                let (d, s) = (denominator as i128, sub as i128);
                let total = v * d * s / den;
                let rem = total % (d * s);
                let sub_digit = (rem % s) * 10 / s;
                write!(
                    out,
                    "{}'{:0width$}{sub_digit}",
                    total / (d * s),
                    rem / s,
                    width = digits_of(d - 1)
                )
            }
        }
    }

    /// Format into a stack buffer (no heap). Truncates silently if the buffer is too small.
    pub fn format<const N: usize>(&self, p: Price) -> FixedStr<N> {
        let mut s = FixedStr::new();
        let _ = self.write(p, &mut s);
        s
    }

    /// Snap a unit value to the nearest tick.
    pub fn round_to_tick(&self, v: i64) -> i64 {
        let t = self.tick;
        (v as i128 * 2 + t as i128).div_euclid(2 * t as i128) as i64 * t
    }
}

fn digits_of(mut v: i128) -> usize {
    let mut n = 1;
    while v >= 10 {
        v /= 10;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(f: &PriceFormat, p: i64) -> String {
        f.format::<32>(Price(p)).as_str().to_owned()
    }

    fn es() -> PriceFormat {
        PriceFormat::new(
            PriceScale::decimal(2),
            DisplayStyle::Decimal { digits: 2 },
            25,
        )
    }

    #[test]
    fn scale_from_common_doubles() {
        assert_eq!(PriceScale::from_f64(0.01).unwrap(), PriceScale::new(1, 100));
        assert_eq!(PriceScale::from_f64(0.25).unwrap(), PriceScale::new(1, 4));
        assert_eq!(PriceScale::from_f64(1.0).unwrap(), PriceScale::ONE);
        assert_eq!(
            PriceScale::from_f64(1.0 / 32.0).unwrap(),
            PriceScale::new(1, 32)
        );
        assert_eq!(
            PriceScale::from_f64(0.0000005).unwrap(),
            PriceScale::new(1, 2_000_000)
        );
        assert_eq!(PriceScale::from_f64(5.0).unwrap(), PriceScale::new(5, 1));
        assert_eq!(PriceScale::from_f64(0.1).unwrap(), PriceScale::new(1, 10));
    }

    #[test]
    fn scale_rejects_garbage() {
        assert!(PriceScale::from_f64(f64::NAN).is_err());
        assert!(PriceScale::from_f64(0.0).is_err());
        assert!(PriceScale::from_f64(-0.25).is_err());
        assert!(matches!(
            PriceScale::from_f64(1.0 / 3.0),
            Err(UnitError::NotRational(_))
        ));
    }

    #[test]
    fn decimal_display() {
        let f = es();
        assert_eq!(fmt(&f, 492_525), "4925.25");
        assert_eq!(fmt(&f, 5), "0.05");
        assert_eq!(fmt(&f, 0), "0.00");
        assert_eq!(fmt(&f, -3_712), "-37.12");
        let tenth = PriceFormat::new(
            PriceScale::decimal(1),
            DisplayStyle::Decimal { digits: 1 },
            1,
        );
        assert_eq!(fmt(&tenth, 3), "0.3"); // not 0.30000000000000004
        let micro = PriceFormat::new(
            PriceScale::decimal(6),
            DisplayStyle::Decimal { digits: 6 },
            1,
        );
        assert_eq!(fmt(&micro, 9_007_199_254_740_993), "9007199254.740993");
    }

    #[test]
    fn treasury_display() {
        let f32nds = PriceFormat::new(
            PriceScale::new(1, 32),
            DisplayStyle::Fraction { denominator: 32 },
            1,
        );
        assert_eq!(fmt(&f32nds, 112 * 32 + 16), "112'16");
        assert_eq!(fmt(&f32nds, 112 * 32 + 3), "112'03");
        let q32 = PriceFormat::new(
            PriceScale::new(1, 128),
            DisplayStyle::SubFraction {
                denominator: 32,
                sub: 4,
            },
            1,
        );
        let p = |w: i64, n: i64, q: i64| w * 128 + n * 4 + q;
        assert_eq!(fmt(&q32, p(108, 22, 0)), "108'220");
        assert_eq!(fmt(&q32, p(108, 22, 1)), "108'222");
        assert_eq!(fmt(&q32, p(108, 22, 2)), "108'225");
        assert_eq!(fmt(&q32, p(108, 22, 3)), "108'227");
    }

    #[test]
    fn parse_decimal_is_exact() {
        let cents = PriceScale::decimal(2);
        assert_eq!(cents.parse_decimal("75415.72"), Ok(7_541_572));
        assert_eq!(cents.parse_decimal("75523"), Ok(7_552_300));
        assert_eq!(cents.parse_decimal("75415.7200000"), Ok(7_541_572));
        assert_eq!(cents.parse_decimal("-0.5"), Ok(-50));
        assert_eq!(cents.parse_decimal(".25"), Ok(25));
        assert!(matches!(
            cents.parse_decimal("75415.725"),
            Err(UnitError::NotMultiple(_))
        ));
        for bad in ["", ".", "1e5", "12a", "--1", "1.2.3"] {
            assert!(
                matches!(cents.parse_decimal(bad), Err(UnitError::Parse(_))),
                "{bad}"
            );
        }
        let quarters = PriceScale::new(1, 4);
        assert_eq!(quarters.parse_decimal("4925.25"), Ok(19_701));
        assert!(quarters.parse_decimal("4925.10").is_err());
        let thirtyseconds = PriceScale::new(1, 32);
        assert_eq!(thirtyseconds.parse_decimal("112.5"), Ok(112 * 32 + 16));
    }

    #[test]
    fn f64_prices_snap_or_reject() {
        // Rithmic-style doubles.
        let cents = PriceScale::decimal(2);
        assert_eq!(cents.units_from_f64(4925.25), Ok(492_525));
        assert_eq!(cents.units_from_f64(0.1 + 0.2), Ok(30)); // float noise is fine
        assert_eq!(cents.units_from_f64(75415.72), Ok(7_541_572));
        assert!(matches!(
            cents.units_from_f64(4925.255),
            Err(UnitError::NotMultiple(_))
        ));
        assert!(cents.units_from_f64(f64::INFINITY).is_err());
        assert_eq!(PriceScale::new(1, 32).units_from_f64(112.5), Ok(3600));
    }

    #[test]
    fn exact_units_from_f64() {
        assert_eq!(PriceScale::decimal(2).exact_units_from_f64(0.25), Ok(25));
        assert!(PriceScale::new(1, 4).exact_units_from_f64(0.1).is_err());
        assert_eq!(
            PriceScale::new(1, 128).exact_units_from_f64(1.0 / 64.0),
            Ok(2)
        );
    }

    #[test]
    fn unit_converter() {
        // CQG-style ES quotes in 1/100 → instrument in 1/4 (ticks).
        let c = UnitConverter::new(PriceScale::decimal(2), PriceScale::new(1, 4));
        assert_eq!(c.convert(492_525), Some(19_701));
        assert_eq!(c.convert(492_510), None, "off-grid must not round");
        let id = UnitConverter::new(PriceScale::decimal(2), PriceScale::new(10, 1000));
        assert!(id.is_identity());
        // Coarser source → finer target always succeeds.
        let c = UnitConverter::new(PriceScale::new(1, 64), PriceScale::new(1, 128));
        assert_eq!(c.convert(3), Some(6));
    }

    #[test]
    fn round_to_tick() {
        let f = es();
        assert_eq!(f.round_to_tick(492_512), 492_500);
        assert_eq!(f.round_to_tick(492_513), 492_525);
        assert_eq!(f.round_to_tick(-12), 0);
        assert_eq!(f.round_to_tick(-13), -25);
    }
}
