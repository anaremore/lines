//! Front-month resolution (decision 0013). Pure functions, no I/O.
//!
//! - **Volume crossover** (default): the front month moves to the next listed contract when
//!   that contract's prior-session volume exceeds the current one's.
//! - **Calendar** (fallback when volume isn't available, e.g. offline or Sierra-only data):
//!   the per-product roll rule (`calendar::calendar_roll_date`).
//!
//! The front month never moves backwards, and an expired contract is never front month.
//! Nothing here touches positions: rolling a position is always an explicit pair of orders.

use crate::calendar::{CivilDate, calendar_roll_date, resolve_expiry};
use crate::instrument::{ContractKey, Delivery, Product};
use crate::qty::Qty;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrontRule {
    VolumeCrossover,
    Calendar,
    /// Spot products have a single perpetual contract.
    Spot,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrontMonth {
    pub contract: ContractKey,
    pub rule: FrontRule,
}

/// How far ahead to look for listed contracts (quarterly products: 5 years).
const LOOKAHEAD: usize = 24;

/// The listed contract after `key`.
pub fn next_contract(product: &Product, key: ContractKey) -> Option<ContractKey> {
    let (year, month) = key.year_month()?;
    let (y, m) = if month.number() == 12 {
        (year + 1, 1)
    } else {
        (year, month.number() + 1)
    };
    product
        .contracts_from(CivilDate {
            year: y,
            month: m,
            day: 1,
        })
        .next()
}

fn expiry(
    product: &Product,
    key: ContractKey,
    provider_expiries: &[(ContractKey, CivilDate)],
) -> Option<CivilDate> {
    let p = provider_expiries
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, d)| *d);
    resolve_expiry(product, key, p).map(|e| e.date)
}

/// Calendar rule: the first listed contract whose roll date is after `today` and which has not
/// expired.
pub fn calendar_front(product: &Product, today: CivilDate) -> Option<ContractKey> {
    let mut contracts = product.contracts_from(today);
    if product.is_spot() {
        return contracts.next();
    }
    contracts
        .take(LOOKAHEAD)
        .find(|&k| calendar_roll_date(product, k).is_some_and(|r| r > today))
}

/// Volume crossover: switch from `current` to the next listed contract when the next one's
/// prior-session volume exceeds `current`'s. Contracts missing from `prior_session` count as
/// zero volume. Only ever moves forward, one contract at a time.
pub fn volume_crossover(
    product: &Product,
    current: ContractKey,
    prior_session: &[(ContractKey, Qty)],
) -> ContractKey {
    let vol = |k: ContractKey| {
        prior_session
            .iter()
            .filter(|(c, _)| *c == k)
            .map(|(_, q)| q.0)
            .sum::<i64>()
    };
    match next_contract(product, current) {
        Some(next) if vol(next) > vol(current) => next,
        _ => current,
    }
}

/// Decision 0013's resolution: volume crossover when prior-session volumes are known, otherwise
/// the calendar rule. `current` is the chart's present front month (never moved backwards);
/// `provider_expiries` override calendar expiries (providers win).
pub fn resolve_front(
    product: &Product,
    today: CivilDate,
    current: Option<ContractKey>,
    prior_session: Option<&[(ContractKey, Qty)]>,
    provider_expiries: &[(ContractKey, CivilDate)],
) -> Option<FrontMonth> {
    if product.is_spot() {
        return Some(FrontMonth {
            contract: product.contracts_from(today).next()?,
            rule: FrontRule::Spot,
        });
    }
    let calendar = calendar_front(product, today)?;
    let (mut contract, rule) = match prior_session {
        Some(vols) => (
            volume_crossover(product, current.unwrap_or(calendar), vols),
            FrontRule::VolumeCrossover,
        ),
        // Without volume, follow the calendar but never step back past a contract that volume
        // (or an earlier calendar decision) already moved to.
        None => (
            current.map_or(calendar, |c| c.max(calendar)),
            FrontRule::Calendar,
        ),
    };
    // An expired contract can't be front month, whatever the volumes say.
    let mut guard = 0;
    while expiry(product, contract, provider_expiries).is_some_and(|e| e < today)
        && guard < LOOKAHEAD
    {
        contract = next_contract(product, contract)?;
        guard += 1;
    }
    debug_assert!(matches!(contract.delivery, Delivery::Monthly { .. }));
    Some(FrontMonth { contract, rule })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::Month;
    use crate::instrument::testing::*;

    fn d(y: u16, m: u8, day: u8) -> CivilDate {
        CivilDate::new(y, m, day).unwrap()
    }
    fn k(y: u16, m: u8) -> ContractKey {
        ContractKey::monthly(es().id, y, Month::new(m).unwrap())
    }

    #[test]
    fn calendar_rule_rolls_8_days_before_expiry() {
        let es = es();
        // ESZ5 expires Fri 2025-12-19; roll date Thu 2025-12-11.
        assert_eq!(calendar_front(&es, d(2025, 12, 10)), Some(k(2025, 12)));
        assert_eq!(calendar_front(&es, d(2025, 12, 11)), Some(k(2026, 3)));
        assert_eq!(calendar_front(&es, d(2025, 10, 1)), Some(k(2025, 12)));
        assert_eq!(calendar_front(&es, d(2025, 12, 31)), Some(k(2026, 3)));
    }

    #[test]
    fn volume_crossover_switches_only_when_next_exceeds_current() {
        let es = es();
        let cur = k(2025, 12);
        assert_eq!(
            volume_crossover(&es, cur, &[(cur, Qty(900)), (k(2026, 3), Qty(800))]),
            cur
        );
        assert_eq!(
            volume_crossover(&es, cur, &[(cur, Qty(900)), (k(2026, 3), Qty(900))]),
            cur,
            "equal: stay"
        );
        assert_eq!(
            volume_crossover(&es, cur, &[(cur, Qty(900)), (k(2026, 3), Qty(901))]),
            k(2026, 3)
        );
        // Only the *next* contract matters; a far month with volume is ignored.
        assert_eq!(
            volume_crossover(&es, cur, &[(cur, Qty(10)), (k(2026, 6), Qty(99))]),
            cur
        );
        // No data for the next contract: stay.
        assert_eq!(volume_crossover(&es, cur, &[(cur, Qty(10))]), cur);
    }

    #[test]
    fn resolution_prefers_volume_and_falls_back_to_calendar() {
        let es = es();
        let today = d(2025, 12, 9); // before the calendar roll date
        let vols = [(k(2025, 12), Qty(400_000)), (k(2026, 3), Qty(1_200_000))];
        let r = resolve_front(&es, today, Some(k(2025, 12)), Some(&vols), &[]).unwrap();
        assert_eq!(
            (r.contract, r.rule),
            (k(2026, 3), FrontRule::VolumeCrossover),
            "volume rolls early"
        );
        let r = resolve_front(&es, today, Some(k(2025, 12)), None, &[]).unwrap();
        assert_eq!((r.contract, r.rule), (k(2025, 12), FrontRule::Calendar));
    }

    #[test]
    fn never_backwards_and_never_expired() {
        let es = es();
        // Already rolled to H6 by volume on Dec 9; the calendar (still Z5) must not undo it.
        let r = resolve_front(&es, d(2025, 12, 10), Some(k(2026, 3)), None, &[]).unwrap();
        assert_eq!(r.contract, k(2026, 3));
        // Volumes still favour Z5 but it expired on Dec 19: H6.
        let vols = [(k(2025, 12), Qty(5)), (k(2026, 3), Qty(1))];
        let r = resolve_front(&es, d(2025, 12, 22), Some(k(2025, 12)), Some(&vols), &[]).unwrap();
        assert_eq!(r.contract, k(2026, 3));
        // Provider says Z5 expired a day early: provider wins.
        let r = resolve_front(
            &es,
            d(2025, 12, 19),
            Some(k(2025, 12)),
            Some(&vols),
            &[(k(2025, 12), d(2025, 12, 18))],
        )
        .unwrap();
        assert_eq!(r.contract, k(2026, 3));
    }

    #[test]
    fn spot_is_its_own_front_month() {
        let btc = btc();
        let r = resolve_front(&btc, d(2026, 1, 1), None, None, &[]).unwrap();
        assert_eq!(
            (r.contract, r.rule),
            (ContractKey::spot(btc.id), FrontRule::Spot)
        );
    }
}
