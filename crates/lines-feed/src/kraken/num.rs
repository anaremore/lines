//! Exact number handling for Kraken (decision 0002) and its book checksum.
//!
//! Kraken sends prices and sizes as decimal *strings* (REST) or as JSON *number literals*
//! (WebSocket v2, Futures). Both are taken as text (`serde_json::value::RawValue` for
//! literals) and converted to integer units of a decimal grid here; nothing goes through
//! `f64`. Exponent notation (`1E-8`) is accepted, because JSON allows it. A value finer than
//! the grid is rejected (`None`), never rounded.

use lines_core::UtcMillis;

/// Integer units of `10^-decimals` for decimal text such as `"75602.8"`, `"0.00006120"`,
/// `"-1.5"` or `"1E-8"`. `None` if malformed, out of range, or finer than the grid.
pub fn units(text: &str, decimals: u32) -> Option<i64> {
    let t = text.trim().trim_matches('"');
    let (neg, body) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()?),
        None => (body, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // value = digits * 10^(exp - frac_len); units = value * 10^decimals.
    let mut digits: i128 = 0;
    let mut significant = 0usize;
    for b in int.bytes().chain(frac.bytes()) {
        if significant > 0 || b != b'0' {
            significant += 1;
            if significant > 36 {
                return None;
            }
        }
        digits = digits * 10 + (b - b'0') as i128;
    }
    let shift = exp as i64 - frac.len() as i64 + decimals as i64;
    let scaled = if shift >= 0 {
        let p = 10i128.checked_pow(u32::try_from(shift).ok()?)?;
        digits.checked_mul(p)?
    } else {
        let p = 10i128.checked_pow(u32::try_from(-shift).ok()?)?;
        if digits % p != 0 {
            return None; // finer than the grid
        }
        digits / p
    };
    let v = i64::try_from(scaled).ok()?;
    Some(if neg { -v } else { v })
}

/// Like [`units`], for text that is a vendor's binary *float* printed in decimal (e.g. Kraken
/// Futures candle volumes, summed in `f64`: `"0.3359999999999999"`). The text is read exactly,
/// then accepted only if it lies within a millionth of a grid step of a grid point, which it is
/// snapped to; a genuinely off-grid value is still `None`. Float sums of thousands of trades
/// drift by ~1e-12 relative (observed: `1066.949999999996`, 4e-9 of a 0.001 step), so the
/// margin is ~250x the noise and far below any real off-grid value. The same idea as
/// `PriceScale::units_from_f64` for float prices (decision 0002).
pub fn units_from_float_text(text: &str, decimals: u32) -> Option<i64> {
    if let Some(u) = units(text, decimals) {
        return Some(u);
    }
    let t = text.trim().trim_matches('"');
    if t.contains(['e', 'E']) {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let extra = frac.len().checked_sub(decimals as usize)?;
    // value in units = digits / 10^extra
    let digits: i128 = format!("{int}{frac}").parse().ok()?;
    let p = 10i128.checked_pow(extra as u32)?;
    let nearest = (digits + p / 2) / p;
    // |value - nearest| in grid steps, compared exactly: diff / p <= 1e-6.
    let diff = (digits - nearest * p).abs();
    if diff.checked_mul(1_000_000)? > p {
        return None;
    }
    let v = i64::try_from(nearest).ok()?;
    Some(if neg { -v } else { v })
}

/// Milliseconds since the epoch from decimal *seconds* text (`"1793988102.4049883"`,
/// Kraken REST trade times), truncated to the millisecond like every other time in Lines.
pub fn seconds_to_ms(text: &str) -> Option<UtcMillis> {
    let t = text.trim().trim_matches('"');
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: i64 = int.parse().ok()?;
    let mut ms = 0i64;
    for (i, b) in frac.bytes().take(3).enumerate() {
        ms += (b - b'0') as i64 * [100, 10, 1][i];
    }
    secs.checked_mul(1000)?.checked_add(ms).map(UtcMillis)
}

/// CRC-32 (IEEE 802.3, as zlib's `crc32`), the checksum Kraken uses.
pub fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    let mut crc = !0u32;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Kraken spot (WebSocket v2) book checksum over the top 10 levels: for each of the 10 best
/// asks (lowest first) then the 10 best bids (highest first), the price and the quantity each
/// formatted at the pair's precision with the decimal point and leading zeros removed,
/// concatenated, then CRC-32.
///
/// Formatting a value at exactly the grid's precision and removing the point *is* its integer
/// count of grid units, zero-padded; removing leading zeros leaves that integer's decimal
/// digits. So with prices and quantities already in exact units of Kraken's precision, each
/// field is just `units.to_string()`: no float, no string re-parsing.
pub fn book_checksum(
    asks: impl Iterator<Item = (i64, i64)>,
    bids: impl Iterator<Item = (i64, i64)>,
) -> u32 {
    use std::fmt::Write;
    let mut s = String::with_capacity(20 * 2 * 20);
    for (price, qty) in asks.take(10).chain(bids.take(10)) {
        let _ = write!(s, "{price}{qty}");
    }
    crc32(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_units() {
        assert_eq!(units("75602.8", 1), Some(756_028));
        assert_eq!(units("75602.0", 1), Some(756_020));
        assert_eq!(units("75602", 1), Some(756_020));
        assert_eq!(units("0.00006120", 8), Some(6_120));
        assert_eq!(units("\"0.01\"", 2), Some(1), "quoted REST strings");
        assert_eq!(units("1E-8", 8), Some(1));
        assert_eq!(units("1.5e-7", 8), Some(15));
        assert_eq!(units("2.5E3", 0), Some(2_500));
        assert_eq!(units("-1.5", 1), Some(-15));
        assert_eq!(
            units("75602.85", 1),
            None,
            "finer than the tick grid: rejected"
        );
        assert_eq!(units("1e-9", 8), None);
        assert_eq!(units("abc", 2), None);
        assert_eq!(units("", 2), None);
        assert_eq!(units("1.2.3", 2), None);
        assert_eq!(units("9999999999999999999999", 0), None, "out of range");
    }

    #[test]
    fn float_noise_is_snapped_but_off_grid_is_not() {
        assert_eq!(units_from_float_text("0.3359999999999999", 4), Some(3_360));
        assert_eq!(units_from_float_text("1.090099999999999", 4), Some(10_901));
        assert_eq!(units_from_float_text("0.0727000000000001", 4), Some(727));
        // Sums of many trades drift further (seen live).
        assert_eq!(units_from_float_text("55.1285000000002", 4), Some(551_285));
        assert_eq!(
            units_from_float_text("1066.949999999996", 3),
            Some(1_066_950)
        );
        assert_eq!(
            units_from_float_text("0.3360", 4),
            Some(3_360),
            "exact passes through"
        );
        assert_eq!(
            units_from_float_text("0.33605", 4),
            None,
            "genuinely off-grid"
        );
        assert_eq!(
            units_from_float_text("0.336100001", 4), // 1e-5 of a step off
            None,
            "beyond float noise"
        );
        assert_eq!(units_from_float_text("abc", 4), None);
    }

    #[test]
    fn seconds_text_to_ms() {
        assert_eq!(
            seconds_to_ms("1793988102.4049883"),
            Some(UtcMillis(1_793_988_102_404))
        );
        assert_eq!(
            seconds_to_ms("1793988102"),
            Some(UtcMillis(1_793_988_102_000))
        );
        assert_eq!(
            seconds_to_ms("1793988102.5"),
            Some(UtcMillis(1_793_988_102_500))
        );
        assert_eq!(seconds_to_ms("-1"), None);
        assert_eq!(seconds_to_ms("1.2e3"), None);
    }

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn checksum_is_the_formatted_concatenation() {
        // Prices at 1 decimal, quantities at 8: "75602.9" -> 756029, "1.49890116" -> 149890116.
        // Reference value computed with Python's zlib.crc32 on the concatenated string.
        let asks = [(756_029, 149_890_116), (756_037, 13_879_137)];
        let bids = [(756_028, 7_607_609)];
        assert_eq!(
            book_checksum(asks.into_iter(), bids.into_iter()),
            1_115_902_688
        );
        // Only the top 10 of each side count.
        let many: Vec<_> = (0..30).map(|i| (1_000 + i, 1)).collect();
        let top: Vec<_> = (0..10).map(|i| (1_000 + i, 1)).collect();
        assert_eq!(
            book_checksum(many.iter().copied(), many.iter().copied()),
            book_checksum(top.iter().copied(), top.iter().copied())
        );
    }
}
