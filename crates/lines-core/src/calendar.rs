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
    third_weekday(y, m, 4)
}

/// The third `weekday` (0 = Monday … 6 = Sunday) of a month.
pub fn third_weekday(y: u16, m: u8, weekday: u8) -> CivilDate {
    let first = CivilDate {
        year: y,
        month: m,
        day: 1,
    };
    let to_day = (weekday + 7 - first.weekday()) % 7;
    first.add_days(to_day as i64 + 14)
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
    let (py, pm) = prev_month(year, m);
    Some(match expiry {
        ExpiryRule::ThirdFriday => third_friday(year, m),
        ExpiryRule::SeventhBusinessDayBeforeLastBusinessDay => {
            business_days_before(last_business_day(year, m), 7)
        }
        ExpiryRule::NthLastBusinessDay(n) => {
            business_days_before(last_business_day(year, m), u32::from(n.max(1)) - 1)
        }
        ExpiryRule::NthLastBusinessDayOfPriorMonth(n) => {
            business_days_before(last_business_day(py, pm), u32::from(n.max(1)) - 1)
        }
        ExpiryRule::BusinessDaysBefore25thOfPriorMonth(n) => {
            let mut anchor = CivilDate {
                year: py,
                month: pm,
                day: 25,
            };
            if !anchor.is_business_day() {
                anchor = business_days_before(anchor, 1);
            }
            business_days_before(anchor, u32::from(n))
        }
        ExpiryRule::BusinessDayBefore15th => business_days_before(
            CivilDate {
                year,
                month: m,
                day: 15,
            },
            1,
        ),
        ExpiryRule::BusinessDaysBeforeThirdWednesday(n) => {
            business_days_before(third_weekday(year, m, 2), u32::from(n))
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

    /// The other CME rules (catalog sources: docs/catalog-sources.md). Dates chosen away from
    /// holidays, which the calendar doesn't model.
    #[test]
    fn other_expiry_rules() {
        let es = es();
        let with = |expiry, roll| Product {
            kind: ProductKind::Future {
                cycle: MonthCycle::MONTHLY,
                expiry,
                roll,
            },
            ..es.clone()
        };
        let exp = |p: &Product, y, m| {
            calendar_expiry(p, ContractKey::monthly(p.id, y, Month::new(m).unwrap())).unwrap()
        };
        let roll = RollRule::DaysBeforeExpiry(5);
        // WTI: 3 business days before the 25th of the prior month. CLZ6: 25 Nov 2026 is a
        // Wednesday → Friday 20th. CLX6: 25 Oct 2026 is a Sunday → from Friday 23rd → 20th.
        let cl = with(ExpiryRule::BusinessDaysBefore25thOfPriorMonth(3), roll);
        assert_eq!(exp(&cl, 2026, 12), d(2026, 11, 20));
        assert_eq!(exp(&cl, 2026, 11), d(2026, 10, 20));
        // Micro WTI: one business day before CL.
        let mcl = with(ExpiryRule::BusinessDaysBefore25thOfPriorMonth(4), roll);
        assert_eq!(exp(&mcl, 2026, 11), d(2026, 10, 19));
        assert_eq!(exp(&mcl, 2026, 12), d(2026, 11, 19));
        // Natural gas: 3rd last business day of the prior month. NGX6 → 28 Oct 2026.
        let ng = with(ExpiryRule::NthLastBusinessDayOfPriorMonth(3), roll);
        assert_eq!(exp(&ng, 2026, 11), d(2026, 10, 28));
        // Gold: 3rd last business day of the month (31 Dec 2026 is a Thursday).
        let gc = with(ExpiryRule::NthLastBusinessDay(3), roll);
        assert_eq!(exp(&gc, 2026, 12), d(2026, 12, 29));
        // 2- and 5-year notes: the last business day (31 Oct 2026 is a Saturday).
        let zf = with(ExpiryRule::NthLastBusinessDay(1), roll);
        assert_eq!(exp(&zf, 2026, 12), d(2026, 12, 31));
        assert_eq!(exp(&zf, 2026, 10), d(2026, 10, 30));
        // Grains: the business day before the 15th (15 Mar 2027 is a Monday → Friday 12th).
        let zc = with(ExpiryRule::BusinessDayBefore15th, roll);
        assert_eq!(exp(&zc, 2026, 12), d(2026, 12, 14));
        assert_eq!(exp(&zc, 2027, 3), d(2027, 3, 12));
        // FX: 2 business days before the third Wednesday. December 2026: Wednesday 16th →
        // Monday 14th. October 2026 starts on a Thursday: third Wednesday the 21st.
        let fx = with(ExpiryRule::BusinessDaysBeforeThirdWednesday(2), roll);
        assert_eq!(third_weekday(2026, 10, 2), d(2026, 10, 21));
        assert_eq!(exp(&fx, 2026, 12), d(2026, 12, 14));
        assert_eq!(exp(&fx, 2027, 3), d(2027, 3, 15));
        // Roll before first notice (last business day of the prior month) for a monthly key.
        let gc = with(
            ExpiryRule::NthLastBusinessDay(3),
            RollRule::DaysBeforeFirstNotice(2),
        );
        let gcz6 = ContractKey::monthly(gc.id, 2026, Month::new(12).unwrap());
        assert_eq!(calendar_roll_date(&gc, gcz6), Some(d(2026, 11, 28)));
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
