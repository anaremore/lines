//! Tick bars (roadmap 9b): a bar every `n` counted trades.
//!
//! [`TickBars`] is an incremental builder fed one counted trade at a time (the trades the
//! store's tape, volume at price and footprints take: `lines_feed::store`), in feed order. It
//! keeps **bars, not trades**: per bar the OHLC and exact `Qty` volume, the first and last
//! trade's times, the trade count and the volume per exact price split by aggressor (the
//! footprint and the visible-range profile read it). Memory is bounded: at `capacity` bars the
//! oldest are dropped in chunks, like the store's series.
//!
//! **Resets.** By default ([`TickReset::Session`]) a bar never spans a session break: the first
//! trade of a new ETH session (the product's calendar; trades between sessions belong to the
//! next one, as the local store partitions them; without a calendar, the UTC day), or the first
//! after a holiday halt inside a session, closes the forming bar, which is then *partial*
//! (fewer than `n` trades). Sessions never go backwards: a late trade stamped in an earlier
//! session than the forming bar's (a feed delivering out of order across the break) joins the
//! forming bar instead of opening one. Early closes and DST changes come from the calendar.
//! A session's last bar can't take another trade once the market clock passes its close (or a
//! holiday halt's start): read at a market time ([`TickBars::is_partial_at`]), it is partial
//! from then on, not only from the next session's first trade.
//! [`TickReset::Continuous`] counts across sessions. Either way each bar knows whether it took
//! its session's first trade ([`TickBars::starts_session`]: the chart's session divider).
//!
//! **Time.** A bar starts at its first trade. For the chart's time mapping
//! ([`BarsView::end`]), a bar's span runs to the next bar's start within a session (so any time
//! maps to the bar holding it), and to just after its last trade at a reset or at the newest
//! bar. Bars are evenly spaced on screen whatever they span.
//!
//! **Cost.** A trade costs O(log prices in the bar). Buffers are reserved for `capacity`, and
//! a dropped bar's price list is reused by the next new bar, so steady-state updates don't
//! allocate (`tests/no_alloc.rs`).

use crate::bars::Bar;
use crate::price::Price;
use crate::profile::SideVolume;
use crate::qty::Qty;
use crate::sessions::{SessionCalendar, SessionKind};
use crate::tape::Aggressor;
use crate::time::UtcMillis;
use crate::timeframe::BarsView;

/// Tick bars offered in lists (header, palette); any count in `1..=MAX_TICKS` can be typed.
pub const TICK_PRESETS: [u32; 5] = [100, 233, 500, 1000, 2000];
/// The largest tick count.
pub const MAX_TICKS: u32 = 100_000;

const DAY: i64 = 86_400_000;

/// Where tick bars restart their count.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum TickReset {
    /// At each session open (the default): a session's last bar may be partial.
    #[default]
    Session,
    /// Never: bars run across sessions.
    Continuous,
}

/// Price levels of one bar: exact price → volume by side, ascending.
type Levels = Vec<(i64, SideVolume)>;

/// Prices a new bar's list is reserved for, at most (fewer for bars of fewer trades; more grow
/// it, off the paint path).
const LEVELS_RESERVE: usize = 32;

/// Tick bars of one count, built incrementally from counted trades.
#[derive(Clone, Debug)]
pub struct TickBars {
    n: u32,
    reset: TickReset,
    cal: Option<&'static SessionCalendar>,
    capacity: usize,
    bars: Vec<Bar>,
    /// Span ends for [`BarsView`] (see the module docs).
    ends: Vec<UtcMillis>,
    /// Each bar's last trade time.
    last: Vec<UtcMillis>,
    counts: Vec<u32>,
    /// Closed by a session reset before reaching `n` trades.
    partial: Vec<bool>,
    /// Took its session's first trade.
    first: Vec<bool>,
    levels: Vec<Levels>,
    /// Price lists of dropped bars, reused by new bars.
    spare: Vec<Levels>,
    /// The cached session: trades in `[lo, close)` belong to the session opening at `key`.
    session: Option<(i64, i64, i64)>,
    /// The session key of the newest bar.
    bar_session: i64,
    /// Where the newest bar's session part ends (its close, or a holiday halt's start).
    bar_close: i64,
    trimmed: u64,
    revision: u64,
    trades: u64,
}

impl TickBars {
    /// Bars of `n` trades (clamped to `1..=MAX_TICKS`), at most `capacity` kept.
    pub fn new(
        n: u32,
        reset: TickReset,
        cal: Option<&'static SessionCalendar>,
        capacity: usize,
    ) -> TickBars {
        let capacity = capacity.max(16);
        TickBars {
            n: n.clamp(1, MAX_TICKS),
            reset,
            cal,
            capacity,
            bars: Vec::with_capacity(capacity),
            ends: Vec::with_capacity(capacity),
            last: Vec::with_capacity(capacity),
            counts: Vec::with_capacity(capacity),
            partial: Vec::with_capacity(capacity),
            first: Vec::with_capacity(capacity),
            levels: Vec::with_capacity(capacity),
            spare: Vec::with_capacity(capacity),
            session: None,
            bar_session: i64::MIN,
            bar_close: i64::MAX,
            trimmed: 0,
            revision: 0,
            trades: 0,
        }
    }

    pub fn ticks(&self) -> u32 {
        self.n
    }

    pub fn reset(&self) -> TickReset {
        self.reset
    }

    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    /// Bars dropped from the front so far (a chart keeps its view still with it).
    pub fn trimmed(&self) -> u64 {
        self.trimmed
    }

    /// Moves on every change.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Trades counted into these bars so far.
    pub fn trades(&self) -> u64 {
        self.trades
    }

    /// Bar `i`'s trade count.
    pub fn count(&self, i: usize) -> u32 {
        self.counts.get(i).copied().unwrap_or(0)
    }

    /// Bar `i`'s last trade time.
    pub fn last_trade(&self, i: usize) -> Option<UtcMillis> {
        self.last.get(i).copied()
    }

    /// Whether bar `i` was closed by a session reset before reaching `n` trades.
    pub fn is_partial(&self, i: usize) -> bool {
        self.partial.get(i).copied().unwrap_or(false)
    }

    /// Whether bar `i` is the newest and still short of `n` trades (not closed by a reset).
    pub fn is_forming(&self, i: usize) -> bool {
        i + 1 == self.bars.len() && self.count(i) < self.n && !self.is_partial(i)
    }

    /// Whether the newest bar's session part has closed by `now` (session resets only): it can
    /// never take another trade.
    fn newest_closed(&self, now: Option<UtcMillis>) -> bool {
        self.reset == TickReset::Session && now.is_some_and(|t| t.0 >= self.bar_close)
    }

    /// [`TickBars::is_partial`] at market time `now`: also the newest bar short of `n` trades
    /// once its session part has closed.
    pub fn is_partial_at(&self, i: usize, now: Option<UtcMillis>) -> bool {
        self.is_partial(i) || (self.is_forming(i) && self.newest_closed(now))
    }

    /// [`TickBars::is_forming`] at market time `now`: not once its session part has closed.
    pub fn is_forming_at(&self, i: usize, now: Option<UtcMillis>) -> bool {
        self.is_forming(i) && !self.newest_closed(now)
    }

    /// Whether bar `i` took its session's first trade (a session divider goes before it).
    pub fn starts_session(&self, i: usize) -> bool {
        self.first.get(i).copied().unwrap_or(false)
    }

    /// Bar `i`'s volume per exact price, ascending.
    pub fn levels(&self, i: usize) -> impl Iterator<Item = (Price, SideVolume)> + '_ {
        self.levels
            .get(i)
            .into_iter()
            .flatten()
            .map(|(p, v)| (Price(*p), *v))
    }

    /// The bars for display. Their nominal period (axis steps, extrapolation past the ends)
    /// is the bars' average span rounded down to a step the time axis uses.
    pub fn view(&self) -> BarsView<'_> {
        let period_ms = match (self.bars.first(), self.last.last()) {
            (Some(f), Some(l)) => nice_period((l.0 - f.time.0) / self.bars.len() as i64),
            _ => 60_000,
        };
        BarsView {
            bars: &self.bars,
            ends: Some(&self.ends),
            period_ms,
            trades: Some(self),
        }
    }

    /// Forget every bar (a contract switch): counted as trimmed, so a chart's view stays put.
    pub fn clear(&mut self) {
        self.trimmed += self.bars.len() as u64;
        self.drop_front(self.bars.len());
        self.session = None;
        self.bar_session = i64::MIN;
        self.bar_close = i64::MAX;
        self.revision += 1;
    }

    /// Add one counted trade (feed order).
    pub fn push(&mut self, time: UtcMillis, price: Price, qty: Qty, aggressor: Option<Aggressor>) {
        self.trades += 1;
        self.revision += 1;
        let (mut key, mut close) = self.session_key(time);
        if !self.bars.is_empty() && key < self.bar_session {
            // A late trade from an earlier session: sessions never go backwards.
            key = self.bar_session;
            close = self.bar_close;
        }
        let new_session = key != self.bar_session;
        let reset = new_session && self.reset == TickReset::Session;
        let full = self.counts.last().is_none_or(|c| *c >= self.n);
        if !full && reset {
            // A new session closes the forming bar short.
            if let Some(p) = self.partial.last_mut() {
                *p = true;
            }
        }
        if full || reset {
            self.open_bar(time, price, qty, key, new_session);
            self.bar_close = close;
        } else {
            if new_session {
                // Continuous: this bar runs into the new session and takes its first trade.
                self.bar_session = key;
                self.bar_close = close;
                if let Some(f) = self.first.last_mut() {
                    *f = true;
                }
            }
            let i = self.bars.len() - 1;
            let b = &mut self.bars[i];
            b.high = b.high.max(price);
            b.low = b.low.min(price);
            b.close = price;
            b.volume = b.volume.saturating_add(qty);
            self.counts[i] += 1;
            self.last[i] = self.last[i].max(time);
            self.ends[i] = UtcMillis(self.last[i].0.saturating_add(1)).max(self.ends[i]);
        }
        let levels = self.levels.last_mut().expect("a bar is open");
        match levels.binary_search_by_key(&price.0, |(p, _)| *p) {
            Ok(k) => levels[k].1.add(qty, aggressor),
            Err(k) => levels.insert(k, (price.0, SideVolume::of(qty, aggressor))),
        }
    }

    fn open_bar(&mut self, time: UtcMillis, price: Price, qty: Qty, key: i64, first: bool) {
        if self.bars.len() >= self.capacity {
            // Drop a chunk at once: moving the buffers costs O(capacity) every chunk, not
            // every bar.
            let k = (self.capacity / 16).max(1);
            self.trimmed += k as u64;
            self.drop_front(k);
        }
        // Starts never go backwards (a late trade), so the bars stay sorted for time lookups.
        let time = self.bars.last().map_or(time, |b| time.max(b.time));
        if let Some(i) = self.bars.len().checked_sub(1)
            && (key == self.bar_session || self.reset == TickReset::Continuous)
        {
            // Contiguous (within a session, or always when continuous): the previous bar runs
            // to this one's start.
            self.ends[i] = time;
        }
        self.bars.push(Bar {
            time,
            open: price,
            high: price,
            low: price,
            close: price,
            volume: qty,
        });
        self.ends.push(UtcMillis(time.0.saturating_add(1)));
        self.last.push(time);
        self.counts.push(1);
        self.partial.push(false);
        self.first.push(first);
        let mut levels = self
            .spare
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(LEVELS_RESERVE.min(self.n as usize)));
        levels.clear();
        self.levels.push(levels);
        self.bar_session = key;
    }

    fn drop_front(&mut self, k: usize) {
        let k = k.min(self.bars.len());
        self.bars.drain(..k);
        self.ends.drain(..k);
        self.last.drain(..k);
        self.counts.drain(..k);
        self.partial.drain(..k);
        self.first.drain(..k);
        for mut l in self.levels.drain(..k) {
            if self.spare.len() < self.spare.capacity() {
                l.clear();
                self.spare.push(l);
            }
        }
    }

    /// The session part `t` belongs to, by its start: the ETH session containing it, or the
    /// next one between sessions; after a holiday halt, the halt's end (trades during the halt
    /// belong after it); without a calendar, the UTC day. With where that part ends.
    fn session_key(&mut self, t: UtcMillis) -> (i64, i64) {
        if let Some((lo, close, key)) = self.session
            && lo <= t.0
            && t.0 < close
        {
            return (key, close);
        }
        let (lo, close, key) = match self.cal.and_then(|c| c.next_session(t, SessionKind::Eth)) {
            Some(s) => match s.halt {
                Some((hs, he)) if t >= hs => (hs.0, s.close.0, he.0),
                Some((hs, _)) => (t.0.min(s.open.0), hs.0, s.open.0),
                None => (t.0.min(s.open.0), s.close.0, s.open.0),
            },
            None => {
                let start = t.0.div_euclid(DAY) * DAY;
                (start, start.saturating_add(DAY), start)
            }
        };
        // Widen the cached range down to earlier times already seen for this session.
        let lo = match self.session {
            Some((plo, _, pkey)) if pkey == key => plo.min(lo),
            _ => lo,
        };
        self.session = Some((lo, close, key));
        (key, close)
    }

    /// The same bars, counts, spans and per-price volumes (tests: rebuilt equals live-built).
    pub fn same_bars(&self, other: &TickBars) -> bool {
        self.n == other.n
            && self.bars == other.bars
            && self.ends == other.ends
            && self.last == other.last
            && self.counts == other.counts
            && self.partial == other.partial
            && self.first == other.first
            && self.levels == other.levels
    }
}

/// The largest axis-friendly period not above `avg` ms (at least a second).
fn nice_period(avg: i64) -> i64 {
    const STEPS: [i64; 14] = [
        1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000, 300_000, 600_000, 900_000,
        1_800_000, 3_600_000, 14_400_000,
    ];
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|s| *s <= avg)
        .unwrap_or(STEPS[0])
}

#[cfg(test)]
mod tests;
