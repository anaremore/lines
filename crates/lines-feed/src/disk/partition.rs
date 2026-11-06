//! Which file a stored event goes to: one segment per contract per **session trade date**
//! (decision 0019), from the product's sessions calendar (`catalog::sessions`).
//!
//! An instant inside a session belongs to that session's trade date (CME: the session opening
//! Sunday 17:00 CT trades for Monday). Data stamped outside every session (the daily halt, a
//! weekend, a closed holiday) goes to the next session's trade date. Products without a
//! calendar fall back to the UTC date.

use lines_core::sessions::SessionKind;
use lines_core::{CivilDate, Product, UtcMillis};

use crate::catalog;

/// Maps an instant to the trade date it belongs to, for a product.
pub trait TradeDates: Send + Sync {
    fn trade_date(&self, product: &Product, t: UtcMillis) -> CivilDate;
}

/// The trade date from the product's sessions calendar.
#[derive(Copy, Clone, Debug, Default)]
pub struct SessionDates;

impl TradeDates for SessionDates {
    fn trade_date(&self, product: &Product, t: UtcMillis) -> CivilDate {
        catalog::sessions(product.id)
            .and_then(|cal| {
                cal.session_at(t, SessionKind::Eth)
                    .or_else(|| cal.next_session(t, SessionKind::Eth))
            })
            .map_or_else(|| CivilDate::from_utc(t), |s| s.trade_date)
    }
}

/// The partition for an event at `t`.
pub fn trade_date(product: &Product, t: UtcMillis) -> CivilDate {
    SessionDates.trade_date(product, t)
}

/// The UTC span `[open, close)` of a product's trade date (for coverage), or the UTC day.
pub fn trade_date_span(product: &Product, date: CivilDate) -> (UtcMillis, UtcMillis) {
    catalog::sessions(product.id)
        .and_then(|cal| cal.session(date, SessionKind::Eth))
        .map_or_else(
            || (date.start_utc(), date.add_days(1).start_utc()),
            |s| (s.open, s.close),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sunday 2026-11-08 23:30 UTC is 17:30 CST: the CME Globex session that opened at 17:00
    /// CT trades for **Monday 2026-11-09** (flipped from the UTC date when sessions landed).
    #[test]
    fn evening_session_belongs_to_the_next_trade_date() {
        let es = catalog::get(catalog::ES).unwrap();
        let t = UtcMillis::parse_iso("2026-11-08T23:30:00Z").unwrap();
        assert_eq!(trade_date(&es, t), CivilDate::new(2026, 11, 9).unwrap());
        // Inside Monday's day session: still Monday.
        let t = UtcMillis::parse_iso("2026-11-09T15:00:00Z").unwrap();
        assert_eq!(trade_date(&es, t), CivilDate::new(2026, 11, 9).unwrap());
        // In the daily halt (16:30 CST): the next session's date.
        let t = UtcMillis::parse_iso("2026-11-09T22:30:00Z").unwrap();
        assert_eq!(trade_date(&es, t), CivilDate::new(2026, 11, 10).unwrap());
        // Coinbase: the UTC day.
        let btc = catalog::get(catalog::BTC_USD).unwrap();
        let t = UtcMillis::parse_iso("2026-11-08T23:30:00Z").unwrap();
        assert_eq!(trade_date(&btc, t), CivilDate::new(2026, 11, 8).unwrap());
        let (open, close) = trade_date_span(&es, CivilDate::new(2026, 11, 9).unwrap());
        assert_eq!(open, UtcMillis::parse_iso("2026-11-08T23:00:00Z").unwrap());
        assert_eq!(close, UtcMillis::parse_iso("2026-11-09T22:00:00Z").unwrap());
    }
}
