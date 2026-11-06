//! UTC timestamps in milliseconds. Everything is UTC; exchange-local time exists only inside
//! [`crate::sessions`].

use std::fmt;

/// Milliseconds since the Unix epoch, UTC.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UtcMillis(pub i64);

impl UtcMillis {
    pub const MINUTE: i64 = 60_000;

    /// Parse CQG's `base_time` ("2013-04-03T14:23:20", optionally with fractional seconds
    /// and/or a trailing `Z`). CQG documents it as UTC.
    pub fn parse_iso(s: &str) -> Option<UtcMillis> {
        let s = s.trim().trim_end_matches('Z');
        let (date, time) = s.split_once('T').or_else(|| s.split_once(' '))?;
        let mut d = date.split('-');
        let y: i64 = d.next()?.parse().ok()?;
        let mo: u32 = d.next()?.parse().ok()?;
        let da: u32 = d.next()?.parse().ok()?;
        if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
            return None;
        }
        let (hms, frac) = match time.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (time, None),
        };
        let mut t = hms.split(':');
        let h: i64 = t.next()?.parse().ok()?;
        let mi: i64 = t.next()?.parse().ok()?;
        let se: i64 = t.next().map_or(Some(0), |v| v.parse().ok())?;
        if t.next().is_some() || h > 23 || mi > 59 || se > 60 {
            return None;
        }
        let ms = match frac {
            Some(f) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => {
                let mut v = 0i64;
                for (i, b) in f.bytes().take(3).enumerate() {
                    v += (b - b'0') as i64 * [100, 10, 1][i];
                }
                v
            }
            Some(_) => return None,
            None => 0,
        };
        // Checked: a hostile year (any i64) must not overflow. Years beyond ±10^11 can't
        // reach the multiplications below without overflowing `days_from_civil` itself.
        if !(-100_000_000_000..=100_000_000_000).contains(&y) {
            return None;
        }
        let days = days_from_civil(y, mo, da);
        let ms = days
            .checked_mul(24)?
            .checked_add(h)?
            .checked_mul(60)?
            .checked_add(mi)?
            .checked_mul(60_000)?
            .checked_add(se * 1000 + ms)?;
        Some(UtcMillis(ms))
    }

    /// Start of the bucket of width `period_ms` containing this instant.
    #[inline]
    pub fn floor_to(self, period_ms: i64) -> UtcMillis {
        UtcMillis(self.0 - self.0.rem_euclid(period_ms))
    }

    /// (year, month, day, hour, minute, second)
    pub fn to_civil(self) -> (i64, u32, u32, u32, u32, u32) {
        let days = self.0.div_euclid(86_400_000);
        let ms_of_day = self.0.rem_euclid(86_400_000);
        let (y, m, d) = civil_from_days(days);
        let secs = ms_of_day / 1000;
        (
            y,
            m,
            d,
            (secs / 3600) as u32,
            (secs / 60 % 60) as u32,
            (secs % 60) as u32,
        )
    }

    /// Day of week, 0 = Sunday.
    pub fn weekday(self) -> u32 {
        // 1970-01-01 was a Thursday.
        (self.0.div_euclid(86_400_000) + 4).rem_euclid(7) as u32
    }

    /// Write the full crosshair time tag, "Fri 2026-11-06 14:05" (plus ":SS" for bar periods
    /// that aren't whole minutes), without allocating.
    pub fn write_crosshair_label<W: fmt::Write>(self, period_ms: i64, out: &mut W) -> fmt::Result {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let (y, mo, d, h, mi, s) = self.to_civil();
        let day = DAYS[self.weekday() as usize];
        write!(out, "{day} {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")?;
        if period_ms % Self::MINUTE != 0 {
            write!(out, ":{s:02}")?;
        }
        Ok(())
    }
}

impl fmt::Display for UtcMillis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, mo, d, h, mi, s) = self.to_civil();
        write!(
            f,
            "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
            self.0.rem_euclid(1000)
        )
    }
}

// Howard Hinnant's algorithms (public domain).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cqg_base_time() {
        // 2013-04-03T14:23:20Z == 1364999000 s
        assert_eq!(
            UtcMillis::parse_iso("2013-04-03T14:23:20"),
            Some(UtcMillis(1_364_999_000_000))
        );
        assert_eq!(
            UtcMillis::parse_iso("2013-04-03T14:23:20Z"),
            Some(UtcMillis(1_364_999_000_000))
        );
        assert_eq!(
            UtcMillis::parse_iso("2013-04-03T14:23:20.5"),
            Some(UtcMillis(1_364_999_000_500))
        );
        assert_eq!(
            UtcMillis::parse_iso("1970-01-01T00:00:00"),
            Some(UtcMillis(0))
        );
        assert_eq!(
            UtcMillis::parse_iso("2024-02-29T23:59:59"),
            Some(UtcMillis(1_709_251_199_000))
        );
        assert_eq!(UtcMillis::parse_iso("garbage"), None);
        // Fuzz regression: huge years overflowed (panic in debug, wrong time in release).
        assert_eq!(
            UtcMillis::parse_iso("9223372036854775807-01-01T00:00:00Z"),
            None
        );
        assert_eq!(UtcMillis::parse_iso("300000000-01-01T00:00:00Z"), None);
        assert_eq!(UtcMillis::parse_iso("-300000000-01-01T00:00:00Z"), None);
        assert!(UtcMillis::parse_iso("200000-01-01T00:00:00Z").is_some());
        assert_eq!(UtcMillis::parse_iso("2013-13-03T14:23:20"), None);
    }

    #[test]
    fn civil_roundtrip() {
        for ms in [0i64, 1_364_999_000_000, -86_400_001, 4_102_444_800_000] {
            let t = UtcMillis(ms - ms.rem_euclid(1000));
            let s = t.to_string();
            assert_eq!(UtcMillis::parse_iso(&s), Some(t), "{s}");
        }
    }

    #[test]
    fn crosshair_label() {
        let t = UtcMillis::parse_iso("2026-11-06T14:05:07Z").unwrap();
        let mut s = crate::FixedStr::<32>::new();
        t.write_crosshair_label(60_000, &mut s).unwrap();
        assert_eq!(s.as_str(), "Fri 2026-11-06 14:05");
        s.clear();
        t.write_crosshair_label(5_000, &mut s).unwrap();
        assert_eq!(s.as_str(), "Fri 2026-11-06 14:05:07");
        assert_eq!(UtcMillis(0).weekday(), 4, "1970-01-01 was a Thursday");
        assert_eq!(UtcMillis(-1).weekday(), 3);
    }

    #[test]
    fn floor_handles_negative() {
        assert_eq!(UtcMillis(-1).floor_to(60_000), UtcMillis(-60_000));
        assert_eq!(UtcMillis(119_999).floor_to(60_000), UtcMillis(60_000));
    }
}
