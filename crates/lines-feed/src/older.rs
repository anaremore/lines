//! Older history on demand: a chart scrolled (or zoomed) back to its oldest bar asks for the
//! stored days before it, and they are loaded on a helper thread from the local store
//! (decision 0019), a few sessions per step ([`SESSIONS_PER_STEP`]).
//!
//! - **1-minute bars** (and every time-based timeframe derived from them): built from the
//!   stored trades of those days and put in front of the store's series
//!   ([`lines_core::BarSeries::extend_back`]), which grows by as many bars. Nothing changes
//!   for a chart never scrolled back: startup and its caps stay as they are.
//! - **Trade-built bars** (tick, volume, range, seconds): the same days through a builder of
//!   the same kind, **price and volume only** (no footprint), put in front of the chart's
//!   builder ([`lines_core::TickBars::prepend`]); the chart says where full order flow begins.
//!
//! Only trades the store's coverage claims are used, so a day with a hole doesn't pass for
//! complete. A store with no directory (a replay; a source not stored) has nothing older: the
//! state says so. One step runs at a time per series; a chart asks again when it reaches the
//! new oldest bar. Lands through [`ChartStore::update_derived`]: the feed's clocks don't move.

use std::sync::{Arc, Weak};

use lines_core::{BarSeries, CivilDate, UtcMillis};

use crate::disk::{ContractDir, partition};
use crate::store::{ChartData, ChartStore};
use crate::ticks::{TickKey, TickSource};

/// Stored sessions read per step.
pub const SESSIONS_PER_STEP: usize = 5;

/// Older bars a step builds before landing, at most (a step's trade-built bars; far more than
/// five sessions make).
const MAX_STEP_BARS: usize = 1 << 21;

/// What older history a series has.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Older {
    /// None asked for, or the last step landed: there may be more.
    #[default]
    Idle,
    /// A step is reading the store.
    Loading,
    /// The store holds nothing older (or nothing older that can be joined on).
    Exhausted,
    /// No local store for this feed (a replay, a source not stored).
    Unavailable,
}

impl Older {
    /// The chart's note while a step runs: "loading older history" (nothing otherwise: a
    /// chart with nothing older simply stops at its oldest bar).
    pub fn label(self) -> Option<&'static str> {
        match self {
            Older::Loading => Some("loading older history"),
            Older::Exhausted | Older::Idle | Older::Unavailable => None,
        }
    }
}

/// Which series asks: the 1-minute bars, or a trade-built builder.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OlderFor {
    Minutes,
    Trades(TickKey),
}

/// Each series' state ([`ChartData::older`]).
#[derive(Clone, Debug, Default)]
pub struct OlderStates {
    minutes: Older,
    trades: Vec<(TickKey, Older)>,
}

impl OlderStates {
    pub fn get(&self, what: OlderFor) -> Older {
        match what {
            OlderFor::Minutes => self.minutes,
            OlderFor::Trades(k) => self
                .trades
                .iter()
                .find(|(t, _)| *t == k)
                .map_or(Older::Idle, |(_, s)| *s),
        }
    }

    fn set(&mut self, what: OlderFor, state: Older) {
        match what {
            OlderFor::Minutes => self.minutes = state,
            OlderFor::Trades(k) => match self.trades.iter_mut().find(|(t, _)| *t == k) {
                Some((_, s)) => *s = state,
                None => self.trades.push((k, state)),
            },
        }
    }
}

impl ChartStore {
    /// Load the stored days before `what`'s oldest bar, on a helper thread (one step: up to
    /// [`SESSIONS_PER_STEP`] sessions). Nothing happens while a step runs or when there is
    /// nothing older; `true` if a step started.
    pub fn load_older(self: &Arc<Self>, what: OlderFor) -> bool {
        // Asked every tick while a chart sits at its oldest bar: decided under a read lock,
        // so nothing is published (no repaint) unless a step starts. No store directory (a
        // replay): never.
        {
            let d = self.read();
            if d.older.get(what) != Older::Idle
                || !matches!(d.tick_source, Some(TickSource::Stored(_)))
            {
                return false;
            }
        }
        let start = self.update_derived(|d| match d.older.get(what) {
            Older::Idle => {
                d.older.set(what, Older::Loading);
                true
            }
            _ => false,
        });
        if !start {
            return false;
        }
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("older-history".into())
            .spawn(move || step_on(&weak, what));
        if spawned.is_err() {
            self.update_derived(|d| d.older.set(what, Older::Idle));
            return false;
        }
        true
    }
}

fn step_on(store: &Weak<ChartStore>, what: OlderFor) {
    let Some(s) = store.upgrade() else { return };
    let state = match step(&s, what, SESSIONS_PER_STEP) {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!("older history: {e}");
            Older::Exhausted
        }
    };
    s.update_derived(|d| d.older.set(what, state));
}

/// One step, on the calling thread: what the series' state becomes (the bars landed).
pub fn step(store: &ChartStore, what: OlderFor, sessions: usize) -> anyhow::Result<Older> {
    // What to read: the store's directory, and the oldest bar to go back from.
    let (dir, product, oldest, builder) = {
        let d = store.read();
        let Some(TickSource::Stored(dir)) = d.tick_source.clone() else {
            return Ok(Older::Unavailable);
        };
        let (oldest, builder) = match what {
            OlderFor::Minutes => (d.series.bars().first().map(|b| b.time), None),
            OlderFor::Trades(k) => {
                let Some(b) = d.ticks.bars(k) else {
                    return Ok(Older::Idle);
                };
                (
                    b.bars().first().map(|x| x.time),
                    Some(b.empty_like(MAX_STEP_BARS)),
                )
            }
        };
        let Some(oldest) = oldest else {
            return Ok(Older::Exhausted);
        };
        (dir, d.product.clone(), oldest, builder)
    };
    let Some((first, last)) = days_before(&dir, &product, oldest, what, sessions) else {
        return Ok(Older::Exhausted);
    };
    let covered = dir.trade_coverage();
    let keep = |_: (i64, i64), t: i64| t < oldest.0 && covered.contains(t);
    let mut cancelled = || false;
    let n = match builder {
        None => {
            let mut series = BarSeries::new(store.read().series.period_ms(), MAX_STEP_BARS);
            crate::ticks::read_stored_trades(
                &dir,
                &product,
                (first, Some(last)),
                keep,
                |p| {
                    series.apply_trade(p.time, p.price, p.qty);
                    true
                },
                &mut cancelled,
            )?;
            if series.is_empty() {
                0
            } else {
                store.update_derived(|d: &mut ChartData| d.series.extend_back(series.bars()))
            }
        }
        Some(mut older) => {
            crate::ticks::read_stored_trades(
                &dir,
                &product,
                (first, Some(last)),
                keep,
                |p| {
                    older.push(p.time, p.price, p.qty, p.aggressor);
                    true
                },
                &mut cancelled,
            )?;
            let OlderFor::Trades(k) = what else {
                unreachable!("a builder only for trade-built bars")
            };
            if older.is_empty() {
                0
            } else {
                store.update_derived(|d| d.ticks.prepend_older(k, older))
            }
        }
    };
    // Bars landed: the chart may ask again from the new oldest bar. None (no stored day before
    // it, or none with covered trades): nothing older can be joined on.
    Ok(if n > 0 { Older::Idle } else { Older::Exhausted })
}

/// The stored trade dates a step reads, oldest and newest: up to `sessions` before `oldest`'s
/// (1-minute bars: its own date too, for the part of it before `oldest`; trade-built bars
/// start their session, so only earlier dates).
fn days_before(
    dir: &ContractDir,
    product: &lines_core::Product,
    oldest: UtcMillis,
    what: OlderFor,
    sessions: usize,
) -> Option<(CivilDate, CivilDate)> {
    let own = partition::trade_date(product, oldest);
    let dates: Vec<CivilDate> = dir
        .dates()
        .into_iter()
        .filter(|d| match what {
            OlderFor::Minutes => *d <= own,
            OlderFor::Trades(_) => *d < own,
        })
        .collect();
    let take = &dates[dates.len().saturating_sub(sessions.max(1))..];
    Some((*take.first()?, *take.last()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::disk::day::DayWriter;
    use crate::native::{self, NativeEvent};
    use lines_core::scratch::Scratch;
    use lines_core::{Capabilities, ContractKey, Month, Price, Qty};

    /// Four stored days (Mon 11-02 to Thu 11-05), 60 trades a day a minute apart; the store
    /// holds the newest day's bars, as a chart at startup. Each step (two sessions) puts the
    /// day before in front; the last finds nothing older. A store without a directory has
    /// nothing older to load.
    #[test]
    fn steps_put_older_stored_days_in_front() {
        let root = Scratch::new("older-steps");
        let es = catalog::get(catalog::ES).unwrap();
        let k = Some(ContractKey::monthly(es.id, 2026, Month::new(12).unwrap()));
        let after = UtcMillis::parse_iso("2026-11-07T00:00:00Z").unwrap();
        let minute = |day: u8, m: i64| {
            UtcMillis(
                UtcMillis::parse_iso(&format!("2026-11-{day:02}T15:00:00Z"))
                    .unwrap()
                    .0
                    + m * 60_000,
            )
        };
        for day in 2..=5u8 {
            let date = CivilDate::new(2026, 11, day).unwrap();
            let mut w = DayWriter::begin(&root, "test", &es, k, date, after).unwrap();
            for m in 0..60 {
                let t = minute(day, m).0 * 1000;
                w.block(
                    t,
                    &[NativeEvent::Trade {
                        time_us: t,
                        price: Price(600_000 + m * 25),
                        qty: Qty(1),
                        aggressor: native::Aggressor::Buy,
                    }],
                )
                .unwrap();
            }
            w.commit().unwrap();
        }
        let store = ChartStore::new(es.clone(), k, Capabilities::default(), "test", 60_000, 100);
        store.update(|d| {
            d.tick_source = Some(TickSource::Stored(ContractDir::new(&root, "test", &es, k)));
            for m in 0..60 {
                d.series.apply_trade(minute(5, m), Price(600_000), Qty(1));
            }
        });
        let oldest = |s: &ChartStore| s.read().series.bars()[0].time;
        for (day, len) in [(4u8, 120), (3, 180), (2, 240)] {
            assert_eq!(step(&store, OlderFor::Minutes, 2).unwrap(), Older::Idle);
            assert_eq!(oldest(&store), minute(day, 0));
            assert_eq!(store.read().series.len(), len);
        }
        assert_eq!(store.read().series.prepended(), 180);
        assert_eq!(
            step(&store, OlderFor::Minutes, 2).unwrap(),
            Older::Exhausted
        );
        // The series grew for it; a new series (another contract) starts at the base size.
        assert!(store.read().series.max_len() >= 240);
        assert_eq!(store.read().series.base_len(), 100);
        // No store directory: nothing to load.
        let bare = ChartStore::new(es, k, Capabilities::default(), "test", 60_000, 100);
        assert_eq!(
            step(&bare, OlderFor::Minutes, 2).unwrap(),
            Older::Unavailable
        );
        assert_eq!(Older::Loading.label(), Some("loading older history"));
    }
}
