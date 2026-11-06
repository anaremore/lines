//! Which file a stored event goes to: one segment per contract per **session trade date**
//! (decision 0019).
//!
//! TODO(m5c-sessions): the sessions calendar (branch `m5c-sessions`) defines each product's
//! trade date (e.g. CME equity futures: the session that opens Sunday 17:00 CT belongs to
//! Monday). Until it lands, the partition is the UTC calendar date. Everything that names a
//! partition goes through [`trade_date`], so switching is one change here plus flipping the
//! `utc_until_sessions_land` test below.

use lines_core::{CivilDate, Product, UtcMillis};

/// Maps an instant to the trade date it belongs to, for a product.
pub trait TradeDates: Send + Sync {
    fn trade_date(&self, product: &Product, t: UtcMillis) -> CivilDate;
}

/// The interim rule: the UTC calendar date.
#[derive(Copy, Clone, Debug, Default)]
pub struct UtcDates;

impl TradeDates for UtcDates {
    fn trade_date(&self, _product: &Product, t: UtcMillis) -> CivilDate {
        CivilDate::from_utc(t)
    }
}

/// The partition for an event at `t`.
pub fn trade_date(product: &Product, t: UtcMillis) -> CivilDate {
    UtcDates.trade_date(product, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    /// Sunday 2026-11-08 23:30 UTC is 17:30 CT: the CME Globex session that opened at 17:00
    /// CT trades for **Monday 2026-11-09**. FLIP when the sessions calendar lands: assert
    /// 2026-11-09 (and delete this note).
    #[test]
    fn utc_until_sessions_land() {
        let es = catalog::get(catalog::ES).unwrap();
        let t = UtcMillis::parse_iso("2026-11-08T23:30:00Z").unwrap();
        assert_eq!(trade_date(&es, t), CivilDate::new(2026, 11, 8).unwrap());
    }
}
