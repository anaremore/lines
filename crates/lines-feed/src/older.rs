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

/// 1-minute bars a step's builder holds per session read (a session is at most 1,440 minutes;
/// a little more never hurts).
const MINUTES_PER_SESSION: usize = 1_500;

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

    /// A new builder for `what` (a trade-built series made again): it starts over.
    pub(crate) fn reset(&mut self, what: OlderFor) {
        self.set(what, Older::Idle);
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
    if let Err(e) = step_weak(store, what, SESSIONS_PER_STEP) {
        tracing::warn!("older history: {e}");
        if let Some(s) = store.upgrade() {
            s.update_derived(|d| {
                if d.older.get(what) == Older::Loading {
                    d.older.set(what, Older::Exhausted);
                }
            });
        }
    }
}

/// One step, on the calling thread (tests; the helper thread holds only a `Weak`): reads,
/// lands and sets the series' state. The state it was left in.
pub fn step(store: &Arc<ChartStore>, what: OlderFor, sessions: usize) -> anyhow::Result<Older> {
    // Started as `load_older` starts one.
    store.update_derived(|d| d.older.set(what, Older::Loading));
    step_weak(&Arc::downgrade(store), what, sessions)?;
    Ok(store.read().older.get(what))
}

/// What a step started from: the store's contract and rewinds (a step that lands after either
/// changed is stale and changes nothing), its directory, product and oldest bar.
struct From {
    contract: Option<lines_core::ContractKey>,
    rewinds: u64,
    dir: ContractDir,
    product: lines_core::Product,
    oldest: UtcMillis,
}

fn step_weak(weak: &Weak<ChartStore>, what: OlderFor, sessions: usize) -> anyhow::Result<()> {
    let Some(store) = weak.upgrade() else {
        return Ok(());
    };
    // What to read, and the builder to read into (sized from what is held).
    let begun = {
        let d = store.read();
        let Some(TickSource::Stored(dir)) = d.tick_source.clone() else {
            drop(d);
            store.update_derived(|d| d.older.set(what, Older::Unavailable));
            return Ok(());
        };
        let (oldest, builder) = match what {
            OlderFor::Minutes => (d.series.bars().first().map(|b| b.time), None),
            OlderFor::Trades(k) => match d.ticks.bars(k) {
                // Rebuilding: try again later.
                None => (None, Some(None)),
                Some(b) => {
                    let per_session = b.len() / sessions_held(b);
                    let cap = (per_session * 2 * sessions.max(1)).max(4096);
                    (
                        b.bars().first().map(|x| x.time),
                        Some(Some(b.empty_like(cap))),
                    )
                }
            },
        };
        oldest.map(|oldest| {
            (
                From {
                    contract: d.contract,
                    rewinds: d.rewinds,
                    dir,
                    product: d.product.clone(),
                    oldest,
                },
                builder,
                d.series.period_ms(),
            )
        })
    };
    drop(store);
    let Some((from, builder, period_ms)) = begun else {
        // Nothing held (or a trade-built series rebuilding): idle, the chart asks again.
        let idle = matches!(what, OlderFor::Trades(_));
        return land(weak, &FromLess, what, |_| None, idle);
    };
    if matches!(builder, Some(None)) {
        return land(weak, &FromLess, what, |_| None, true);
    }
    let Some((first, last)) = days_before(&from.dir, &from.product, from.oldest, what, sessions)
    else {
        return land(weak, &from, what, |_| Some(0), false);
    };
    let covered = from.dir.trade_coverage();
    let oldest = from.oldest;
    let keep = |_: (i64, i64), t: i64| t < oldest.0 && covered.contains(t);
    let mut cancelled = || weak.strong_count() == 0;
    let days = from
        .dir
        .dates()
        .iter()
        .filter(|d| **d >= first && **d <= last)
        .count();
    match builder {
        None => {
            let mut series = BarSeries::new(period_ms, days.max(1) * MINUTES_PER_SESSION);
            crate::ticks::read_stored_trades(
                &from.dir,
                &from.product,
                (first, Some(last)),
                keep,
                |p| {
                    series.apply_trade(p.time, p.price, p.qty);
                    true
                },
                &mut cancelled,
            )?;
            land(
                weak,
                &from,
                what,
                |d| Some(d.series.extend_back(series.bars())),
                false,
            )
        }
        Some(Some(mut older)) => {
            crate::ticks::read_stored_trades(
                &from.dir,
                &from.product,
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
            let mut older = Some(older);
            land(
                weak,
                &from,
                what,
                |d| match older.take() {
                    Some(o) if !o.is_empty() => d.ticks.prepend_older(k, o),
                    _ => Some(0),
                },
                false,
            )
        }
        Some(None) => unreachable!("handled above"),
    }
}

/// A step that read nothing (no contract check needed).
struct FromLess;

/// Something a landing checks: the contract and rewinds the step began with.
trait Began {
    fn current(&self, d: &ChartData) -> bool;
}

impl Began for From {
    fn current(&self, d: &ChartData) -> bool {
        d.contract == self.contract && d.rewinds == self.rewinds
    }
}

impl Began for FromLess {
    fn current(&self, _: &ChartData) -> bool {
        true
    }
}

/// Land a step under one lock: only if the store is still on the contract (and rewind) it
/// began with and the series still waits for it; `apply` puts the bars in (`None`: its builder
/// is rebuilding: idle, asked again; `Some(0)`: nothing older joins on: exhausted). A stale
/// step changes nothing (the new contract's state is its own).
fn land(
    weak: &Weak<ChartStore>,
    began: &dyn Began,
    what: OlderFor,
    apply: impl FnOnce(&mut ChartData) -> Option<usize>,
    idle_if_none: bool,
) -> anyhow::Result<()> {
    let Some(store) = weak.upgrade() else {
        return Ok(());
    };
    store.update_derived(|d| {
        if !began.current(d) || d.older.get(what) != Older::Loading {
            return;
        }
        let state = match apply(d) {
            None if idle_if_none => Older::Idle,
            None => Older::Exhausted,
            // Bars landed: the chart may ask again from the new oldest bar.
            Some(n) if n > 0 => Older::Idle,
            Some(_) => Older::Exhausted,
        };
        d.older.set(what, state);
    });
    Ok(())
}

/// Sessions a builder's bars span (each bar that starts a session; at least one).
fn sessions_held(b: &lines_core::TickBars) -> usize {
    (0..b.len()).filter(|&i| b.starts_session(i)).count().max(1)
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
        let store = Arc::new(ChartStore::new(
            es.clone(),
            k,
            Capabilities::default(),
            "test",
            60_000,
            100,
        ));
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
        let bare = Arc::new(ChartStore::new(
            es.clone(),
            k,
            Capabilities::default(),
            "test",
            60_000,
            100,
        ));
        assert_eq!(
            step(&bare, OlderFor::Minutes, 2).unwrap(),
            Older::Unavailable
        );
        assert_eq!(Older::Loading.label(), Some("loading older history"));
    }

    /// A step that lands after the store switched contracts (or rewound) is stale: the new
    /// contract's bars and its older-history state are untouched.
    #[test]
    fn a_stale_step_changes_nothing() {
        let es = catalog::get(catalog::ES).unwrap();
        let dec = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        let mar = ContractKey::monthly(es.id, 2027, Month::new(3).unwrap());
        let store = Arc::new(ChartStore::new(
            es.clone(),
            Some(dec),
            Capabilities::default(),
            "test",
            60_000,
            100,
        ));
        store.update(|d| d.older.set(OlderFor::Minutes, Older::Loading));
        let began = From {
            contract: Some(dec),
            rewinds: 0,
            dir: ContractDir::new(std::path::Path::new("unused"), "test", &es, Some(dec)),
            product: es.clone(),
            oldest: UtcMillis(0),
        };
        store.update(|d| d.switch_contract(mar));
        let old_bar = lines_core::Bar::from_trade(UtcMillis(0), Price(1), Qty(1));
        land(
            &Arc::downgrade(&store),
            &began,
            OlderFor::Minutes,
            |d| Some(d.series.extend_back(&[old_bar])),
            false,
        )
        .unwrap();
        assert!(
            store.read().series.is_empty(),
            "the old contract's bars stay out"
        );
        assert_eq!(store.read().older.get(OlderFor::Minutes), Older::Idle);
    }
}
