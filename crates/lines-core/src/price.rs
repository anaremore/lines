//! Exact price representation.
//!
//! CQG sends prices as scaled integers (`scaled_price`) plus a per-contract `correct_price_scale`
//! (a protobuf `double`): `correct_price = scaled_price * correct_price_scale`.
//!
//! We never multiply by that double. On receipt of contract metadata the double is converted
//! once into an exact rational [`PriceScale`] (validated), and every stored or compared price
//! stays an `i64` [`Price`]. `f64` appears only in the final pixel mapping.

use std::fmt;

use crate::fixed_str::FixedStr;

/// A price in the contract's native scaled-integer units (CQG `scaled_price`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Price(pub i64);

impl Price {
    #[inline]
    pub fn raw(self) -> i64 {
        self.0
    }
}

/// Exact rational `num / den` equal to CQG's `correct_price_scale`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PriceScale {
    num: i64,
    den: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScaleError {
    NotFinite(f64),
    NotPositive(f64),
    /// No denominator of the form 2^a * 10^b (<= 10^15) represents the value exactly.
    NotRational(f64),
    /// `tick_size / correct_price_scale` is not a whole number of scaled units.
    TickNotMultiple {
        tick_size: f64,
    },
}

impl fmt::Display for ScaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScaleError::NotFinite(v) => write!(f, "price scale {v} is not finite"),
            ScaleError::NotPositive(v) => write!(f, "price scale {v} is not positive"),
            ScaleError::NotRational(v) => {
                write!(f, "price scale {v} has no exact 2^a*10^b representation")
            }
            ScaleError::TickNotMultiple { tick_size } => {
                write!(
                    f,
                    "tick size {tick_size} is not a whole number of scaled units"
                )
            }
        }
    }
}
impl std::error::Error for ScaleError {}

const MAX_DEN: i64 = 1_000_000_000_000_000; // 10^15, CQG's finest decimal display scale.

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

    /// Convert CQG's `double` scale into an exact rational.
    ///
    /// Futures price scales are always of the form `n / (2^a * 10^b)` (decimal ticks, binary
    /// fractions such as 1/32 for treasuries, or a mix). We search those denominators and keep
    /// the smallest whose correctly-rounded quotient `num/den` is *bit-identical* to the double
    /// (the double CQG sends is the nearest double to the true rational, and IEEE division of
    /// two exact integers is correctly rounded). A tolerance would accept 1/3 ≈ 333…/10^15.
    pub fn from_f64(scale: f64) -> Result<Self, ScaleError> {
        if !scale.is_finite() {
            return Err(ScaleError::NotFinite(scale));
        }
        if scale <= 0.0 {
            return Err(ScaleError::NotPositive(scale));
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
                    let back = num as f64 / den as f64;
                    if back == scale {
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
        best.ok_or(ScaleError::NotRational(scale))
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

    /// Convert a real-unit quantity given as a double (e.g. CQG `tick_size`) into an exact
    /// number of scaled units, rejecting anything that is not a whole multiple.
    pub fn units_from_f64(self, value: f64) -> Result<i64, ScaleError> {
        let units = value * self.den as f64 / self.num as f64;
        let r = units.round();
        if !units.is_finite() || r < 1.0 || (units - r).abs() > 1e-6 {
            return Err(ScaleError::TickNotMultiple { tick_size: value });
        }
        Ok(r as i64)
    }
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.abs().max(1)
}

/// How prices are rendered as text. Mirrors CQG `display_price_scale`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DisplayStyle {
    /// `display_price_scale` 0..=15: plain decimal with this many fraction digits.
    Decimal { digits: u8 },
    /// 101..=110: whole + `'` + numerator of `1/denominator` (e.g. 32nds as `112'16`).
    Fraction { denominator: u16 },
    /// 201..=205: fractions of a fraction, CQG "truncated" style. E.g. quarter-32nds:
    /// 112 + 16.75/32 is shown `112'167`.
    SubFraction { denominator: u16, sub: u16 },
}

impl DisplayStyle {
    /// Map CQG's `display_price_scale` code. Unknown codes fall back to 6 decimals.
    pub fn from_cqg(code: u32) -> Self {
        match code {
            0..=15 => DisplayStyle::Decimal { digits: code as u8 },
            101..=110 => DisplayStyle::Fraction {
                denominator: 1 << (code - 100),
            },
            201 => DisplayStyle::SubFraction {
                denominator: 32,
                sub: 2,
            },
            202 => DisplayStyle::SubFraction {
                denominator: 32,
                sub: 4,
            },
            203 => DisplayStyle::SubFraction {
                denominator: 32,
                sub: 8,
            },
            204 => DisplayStyle::SubFraction {
                denominator: 64,
                sub: 2,
            },
            205 => DisplayStyle::SubFraction {
                denominator: 64,
                sub: 4,
            },
            // 206 (SixteenthThirtySeconds) needs two sub-digits; not used by any contract we
            // chart yet. Fall back to decimals rather than render something misleading.
            _ => DisplayStyle::Decimal { digits: 6 },
        }
    }
}

/// Everything needed to interpret and print prices for one contract.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PriceFormat {
    pub scale: PriceScale,
    pub style: DisplayStyle,
    /// Minimum price increment in scaled units (from CQG `tick_size`).
    pub tick: i64,
}

impl PriceFormat {
    pub fn from_cqg(
        correct_price_scale: f64,
        display_price_scale: u32,
        tick_size: f64,
    ) -> Result<Self, ScaleError> {
        let scale = PriceScale::from_f64(correct_price_scale)?;
        let tick = scale.units_from_f64(tick_size)?;
        Ok(PriceFormat {
            scale,
            style: DisplayStyle::from_cqg(display_price_scale),
            tick,
        })
    }

    /// Write the price without allocating. Exact: uses integer arithmetic only.
    pub fn write<W: fmt::Write>(&self, p: Price, out: &mut W) -> fmt::Result {
        let (num, den) = (self.scale.num as i128, self.scale.den as i128);
        // real = p * num / den
        let v = p.0 as i128 * num;
        let neg = v < 0;
        let v = v.abs();
        if neg {
            out.write_char('-')?;
        }
        match self.style {
            DisplayStyle::Decimal { digits } => {
                let pow = 10i128.pow(digits as u32);
                // Scaled to `digits` decimals; exact when the contract's scale is decimal.
                let scaled = v * pow;
                let q = scaled / den;
                let rem = scaled % den;
                // Round half away from zero if the scale is finer than the display digits.
                let q = if rem * 2 >= den { q + 1 } else { q };
                let whole = q / pow;
                let frac = q % pow;
                write!(out, "{whole}")?;
                if digits > 0 {
                    write!(out, ".{frac:0width$}", width = digits as usize)?;
                }
                Ok(())
            }
            DisplayStyle::Fraction { denominator } => {
                let d = denominator as i128;
                let total = v * d / den; // whole number of 1/d units (truncated)
                let whole = total / d;
                let numer = total % d;
                let width = digits_of(d - 1);
                write!(out, "{whole}'{numer:0width$}")
            }
            DisplayStyle::SubFraction { denominator, sub } => {
                let d = denominator as i128;
                let s = sub as i128;
                let total = v * d * s / den; // whole number of 1/(d*s) units
                let whole = total / (d * s);
                let rem = total % (d * s);
                let numer = rem / s;
                let sub_units = rem % s;
                // CQG "truncated" sub-digit: tenths of a 1/d, truncated (0.25 -> 2, 0.75 -> 7).
                let sub_digit = sub_units * 10 / s;
                let width = digits_of(d - 1);
                write!(out, "{whole}'{numer:0width$}{sub_digit}")
            }
        }
    }

    /// Format into a stack buffer (no heap). Truncates silently if the buffer is too small.
    pub fn format<const N: usize>(&self, p: Price) -> FixedStr<N> {
        let mut s = FixedStr::new();
        let _ = self.write(p, &mut s);
        s
    }

    /// Snap a scaled value to the nearest tick.
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
            PriceScale::from_f64(1.0 / 128.0).unwrap(),
            PriceScale::new(1, 128)
        );
        // 6J-style tiny scale: 0.0000005 = 1/2_000_000
        assert_eq!(
            PriceScale::from_f64(0.0000005).unwrap(),
            PriceScale::new(1, 2_000_000)
        );
        assert_eq!(PriceScale::from_f64(5.0).unwrap(), PriceScale::new(5, 1));
        // 0.1 is not exactly representable as a double; must still map to exactly 1/10.
        assert_eq!(PriceScale::from_f64(0.1).unwrap(), PriceScale::new(1, 10));
    }

    #[test]
    fn scale_rejects_garbage() {
        assert!(PriceScale::from_f64(f64::NAN).is_err());
        assert!(PriceScale::from_f64(0.0).is_err());
        assert!(PriceScale::from_f64(-0.25).is_err());
        assert!(matches!(
            PriceScale::from_f64(1.0 / 3.0),
            Err(ScaleError::NotRational(_))
        ));
    }

    #[test]
    fn es_like_decimal() {
        // E-mini S&P: correct_price_scale 0.01, display 2 decimals, tick 0.25.
        let f = PriceFormat::from_cqg(0.01, 2, 0.25).unwrap();
        assert_eq!(f.tick, 25);
        assert_eq!(fmt(&f, 492_525), "4925.25");
        assert_eq!(fmt(&f, 5), "0.05");
        assert_eq!(fmt(&f, 0), "0.00");
        assert_eq!(fmt(&f, -3_712), "-37.12"); // CL went negative in 2020; i64 handles it.
    }

    #[test]
    fn exactness_where_f64_fails() {
        // 0.1 + 0.2 style trap: 3 ticks of 0.1 must print 0.3, not 0.30000000000000004.
        let f = PriceFormat::from_cqg(0.1, 1, 0.1).unwrap();
        assert_eq!(fmt(&f, 3), "0.3");
        // Large prices keep every digit.
        let f = PriceFormat::from_cqg(0.000001, 6, 0.000001).unwrap();
        assert_eq!(fmt(&f, 9_007_199_254_740_993), "9007199254.740993");
    }

    #[test]
    fn treasury_32nds() {
        // ZB-style: scale 1/32, display "ThirtySeconds" (105).
        let f = PriceFormat::from_cqg(1.0 / 32.0, 105, 1.0 / 32.0).unwrap();
        assert_eq!(f.tick, 1);
        assert_eq!(fmt(&f, 112 * 32 + 16), "112'16");
        assert_eq!(fmt(&f, 112 * 32 + 3), "112'03");
    }

    #[test]
    fn treasury_quarter_32nds() {
        // ZF-style: scale 1/128, display "QuarterThirtySeconds" (202).
        let f = PriceFormat::from_cqg(1.0 / 128.0, 202, 1.0 / 128.0).unwrap();
        let p = |whole: i64, n32: i64, quarters: i64| Price(whole * 128 + n32 * 4 + quarters);
        assert_eq!(f.format::<32>(p(108, 22, 0)).as_str(), "108'220");
        assert_eq!(f.format::<32>(p(108, 22, 1)).as_str(), "108'222");
        assert_eq!(f.format::<32>(p(108, 22, 2)).as_str(), "108'225");
        assert_eq!(f.format::<32>(p(108, 22, 3)).as_str(), "108'227");
    }

    #[test]
    fn tick_must_be_multiple_of_scale() {
        assert!(matches!(
            PriceFormat::from_cqg(0.25, 2, 0.1),
            Err(ScaleError::TickNotMultiple { .. })
        ));
    }

    #[test]
    fn round_to_tick() {
        let f = PriceFormat::from_cqg(0.01, 2, 0.25).unwrap();
        assert_eq!(f.round_to_tick(492_512), 492_500);
        assert_eq!(f.round_to_tick(492_513), 492_525);
        assert_eq!(f.round_to_tick(-12), 0);
        assert_eq!(f.round_to_tick(-13), -25);
    }
}
