//! Civil (UTC) dates and contract expiry/roll calendars (decision 0013).
//!
//! Calendar rules are the *fallback*: provider metadata wins when available. Exchange
//! holidays are not modelled (business day = Monday–Friday), so a calendar date can be one
//! business day off around holidays; providers' dates correct that.

use std::fmt;

use crate::instrument::{ContractKey, Delivery, ExpiryRule, Product, ProductKind, RollRule};
use crate::time::{UtcMillis, civil_from_days, days_from_civil};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CivilDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl CivilDate {
    pub fn new(year: u16, month: u8, day: u8) -> Option<CivilDate> {
        let d = CivilDate { year, month, day };
        ((1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)).then_some(d)
    }

    pub fn from_utc(t: UtcMillis) -> CivilDate {
        let (y, m, d, ..) = t.to_civil();
        CivilDate {
            year: y as u16,
            month: m as u8,
            day: d as u8,
        }
    }

    /// Days since 1970-01-01.
    pub fn days(self) -> i64 {
        days_from_civil(self.year as i64, self.month as u32, self.day as u32)
    }

    pub fn from_days(days: i64) -> CivilDate {
        let (y, m, d) = civil_from_days(days);
        CivilDate {
            year: y as u16,
            month: m as u8,
            day: d as u8,
        }
    }

    pub fn add_days(self, n: i64) -> CivilDate {
        CivilDate::from_days(self.days() + n)
    }

    /// 0 = Monday … 6 = Sunday.
    pub fn weekday(self) -> u8 {
        // 1970-01-01 was a Thursday (3).
        (self.days() + 3).rem_euclid(7) as u8
    }

    pub fn is_business_day(self) -> bool {
        self.weekday() < 5
    }

    pub fn start_utc(self) -> UtcMillis {
        UtcMillis(self.days() * 86_400_000)
    }
}

impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn is_leap(y: u16) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

pub fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

pub fn third_friday(y: u16, m: u8) -> CivilDate {
    let first = CivilDate {
        year: y,
        month: m,
        day: 1,
    };
    let to_friday = (4 + 7 - first.weekday()) % 7;
    first.add_days(to_friday as i64 + 14)
}

pub fn last_business_day(y: u16, m: u8) -> CivilDate {
    let mut d = CivilDate {
        year: y,
        month: m,
        day: days_in_month(y, m),
    };
    while !d.is_business_day() {
        d = d.add_days(-1);
    }
    d
}

/// `n` business days before `d`.
pub fn business_days_before(mut d: CivilDate, n: u32) -> CivilDate {
    let mut left = n;
    while left > 0 {
        d = d.add_days(-1);
        if d.is_business_day() {
            left -= 1;
        }
    }
    d
}

fn prev_month(y: u16, m: u8) -> (u16, u8) {
    if m == 1 { (y - 1, 12) } else { (y, m - 1) }
}

/// Calendar expiry (last trading day) of a dated contract. `None` for spot or for a key that
/// doesn't belong to `product`.
pub fn calendar_expiry(product: &Product, key: ContractKey) -> Option<CivilDate> {
    let (ProductKind::Future { expiry, .. }, Delivery::Monthly { year, month }) =
        (product.kind, key.delivery)
    else {
        return None;
    };
    if key.product != product.id {
        return None;
    }
    let m = month.number();
    Some(match expiry {
        ExpiryRule::ThirdFriday => third_friday(year, m),
        ExpiryRule::SeventhBusinessDayBeforeLastBusinessDay => {
            business_days_before(last_business_day(year, m), 7)
        }
    })
}

/// Calendar roll date: from this date on, the next contract is front month.
pub fn calendar_roll_date(product: &Product, key: ContractKey) -> Option<CivilDate> {
    let ProductKind::Future { roll, .. } = product.kind else {
        return None;
    };
    let (year, month) = key.year_month()?;
    Some(match roll {
        RollRule::DaysBeforeExpiry(n) => calendar_expiry(product, key)?.add_days(-(n as i64)),
        RollRule::DaysBeforeFirstNotice(n) => {
            let (py, pm) = prev_month(year, month.number());
            last_business_day(py, pm).add_days(-(n as i64))
        }
    })
}

/// Where an expiry date came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExpirySource {
    Provider,
    Calendar,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Expiry {
    pub date: CivilDate,
    pub source: ExpirySource,
    /// Set when the provider's date differs from the calendar rule (callers log a warning).
    pub calendar_disagrees: Option<CivilDate>,
}

/// Decision 0013: provider metadata wins; the calendar is the fallback; disagreement is
/// reported so the caller can warn.
pub fn resolve_expiry(
    product: &Product,
    key: ContractKey,
    provider: Option<CivilDate>,
) -> Option<Expiry> {
    let cal = calendar_expiry(product, key);
    match (provider, cal) {
        (Some(p), c) => Some(Expiry {
            date: p,
            source: ExpirySource::Provider,
            calendar_disagrees: c.filter(|c| *c != p),
        }),
        (None, Some(c)) => Some(Expiry {
            date: c,
            source: ExpirySource::Calendar,
            calendar_disagrees: None,
        }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::testing::*;
    use crate::instrument::{Month, MonthCycle};

    fn d(y: u16, m: u8, day: u8) -> CivilDate {
        CivilDate::new(y, m, day).unwrap()
    }

    #[test]
    fn date_basics() {
        assert_eq!(d(2026, 11, 6).weekday(), 4, "2026-11-06 is a Friday");
        assert_eq!(d(1970, 1, 1).weekday(), 3);
        assert_eq!(d(2024, 2, 28).add_days(1), d(2024, 2, 29));
        assert!(CivilDate::new(2025, 2, 29).is_none());
        assert_eq!(
            CivilDate::from_utc(UtcMillis::parse_iso("2025-09-11T23:59:59Z").unwrap()),
            d(2025, 9, 11)
        );
        assert_eq!(d(2025, 9, 11).to_string(), "2025-09-11");
    }

    #[test]
    fn known_expiries() {
        // CME equity index: third Friday. ESZ5 expired 2025-12-19, ESH6 2026-03-20.
        assert_eq!(third_friday(2025, 12), d(2025, 12, 19));
        assert_eq!(third_friday(2026, 3), d(2026, 3, 20));
        assert_eq!(
            third_friday(2026, 5),
            d(2026, 5, 15),
            "month starting on a Friday"
        );
        let es = es();
        let z25 = ContractKey::monthly(es.id, 2025, Month::new(12).unwrap());
        assert_eq!(calendar_expiry(&es, z25), Some(d(2025, 12, 19)));
        assert_eq!(
            calendar_roll_date(&es, z25),
            Some(d(2025, 12, 11)),
            "8 days before: the Thursday"
        );
        // Treasury: the real ZNZ5 last trading day was 2025-12-19, because Christmas is not a
        // business day. Without a holiday calendar the rule gives 2025-12-22; provider metadata
        // corrects it (resolve_expiry), which is why providers win.
        let zn = Product {
            kind: ProductKind::Future {
                cycle: MonthCycle::QUARTERLY,
                expiry: ExpiryRule::SeventhBusinessDayBeforeLastBusinessDay,
                roll: RollRule::DaysBeforeFirstNotice(2),
            },
            ..es.clone()
        };
        let zk = ContractKey::monthly(zn.id, 2025, Month::new(12).unwrap());
        assert_eq!(calendar_expiry(&zn, zk), Some(d(2025, 12, 22)));
        // First notice day = last business day of November 2025 (Friday 28th); roll 2 days earlier.
        assert_eq!(calendar_roll_date(&zn, zk), Some(d(2025, 11, 26)));
    }

    #[test]
    fn provider_wins_and_disagreement_is_reported() {
        let es = es();
        let k = ContractKey::monthly(es.id, 2025, Month::new(12).unwrap());
        let e = resolve_expiry(&es, k, None).unwrap();
        assert_eq!(
            (e.date, e.source),
            (d(2025, 12, 19), ExpirySource::Calendar)
        );
        let e = resolve_expiry(&es, k, Some(d(2025, 12, 19))).unwrap();
        assert_eq!(
            (e.source, e.calendar_disagrees),
            (ExpirySource::Provider, None)
        );
        let e = resolve_expiry(&es, k, Some(d(2025, 12, 18))).unwrap();
        assert_eq!(
            (e.date, e.calendar_disagrees),
            (d(2025, 12, 18), Some(d(2025, 12, 19)))
        );
        assert!(resolve_expiry(&btc(), ContractKey::spot(btc().id), None).is_none());
    }
}
