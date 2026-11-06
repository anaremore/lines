//! Volume profile (roadmap 5): traded volume per price for a session, a range of sessions, or
//! the bars on screen, with the point of control and the value area.
//!
//! **Data** ([`SessionProfiles`], kept by the feed's store and updated per trade):
//! - one [`SessionProfile`] per ETH session and per RTH session of the product's session
//!   calendar (decision 0017), bounded to the newest [`SessionProfiles::MAX_SESSIONS`] of each;
//! - a per-bar footprint ([`Footprint`]) for the visible-range profile, bounded like the bars.
//!
//! Each trade is added in O(log levels) to the session that contains it (ETH and RTH are
//! separate profiles; a trade outside RTH is in the ETH profile only, one in the daily
//! maintenance break in neither) and to its bar's footprint. Nothing is rebuilt from trades.
//! A trade in a session that is not the newest (a late trade) goes to its own session.
//!
//! **Exact** (decision 0002): volumes are `Qty` integer sums split by aggressor side, prices
//! integer units. Rows group prices exactly as the DOM ladder does
//! (`row = floor(price / (grouping × tick))`, [`crate::ladder::row_of`]), so a profile row and
//! a DOM row with the same grouping hold the same prices.
//!
//! **Point of control and value area** ([`ProfileRows::build`]), on the grouped rows:
//! - POC = the row with the largest volume. *Ties:* the row nearest the middle of the
//!   profile's range (first to last row with volume); if two are equally near, the lower one.
//! - Value area = rows around the POC holding at least `percent`% of the volume
//!   (`100 × va_volume >= percent × total`, integer arithmetic). Grown one row at a time from
//!   the POC: the next row above and the next row below are compared and the larger is added.
//!   *Ties:* both are added. Empty rows inside the range count as rows of volume 0.
//!
//! **Paint path:** [`ProfileRows::build`] fills a caller-owned buffer; it allocates only if the
//! profile spans more rows than the buffer has ever held (callers raise the grouping instead,
//! see [`ProfileRows::fit_grouping`]). Tested in `tests/no_alloc.rs`.
//!
//! **Coverage:** profiles hold only trades the store counted (live or replayed), not history
//! bars, which have no per-price volume. [`SessionProfiles::data_start`] is the first counted
//! trade; a session or range that began earlier is *partial*, and views must say so.

use std::collections::{BTreeMap, VecDeque};

use crate::ladder::{GROUPING_PRESETS, row_of};
use crate::price::Price;
use crate::qty::Qty;
use crate::sessions::{Session, SessionCalendar, SessionKind};
use crate::tape::Aggressor;
use crate::time::UtcMillis;

/// Volume split by aggressor side. Exact.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SideVolume {
    /// A buyer took the offer.
    pub buy: Qty,
    /// A seller hit the bid.
    pub sell: Qty,
    /// Side not reported by the source.
    pub unknown: Qty,
}

impl SideVolume {
    pub fn of(qty: Qty, aggressor: Option<Aggressor>) -> SideVolume {
        let mut v = SideVolume::default();
        v.add(qty, aggressor);
        v
    }

    pub fn add(&mut self, qty: Qty, aggressor: Option<Aggressor>) {
        let slot = match aggressor {
            Some(Aggressor::Buy) => &mut self.buy,
            Some(Aggressor::Sell) => &mut self.sell,
            None => &mut self.unknown,
        };
        *slot = slot.saturating_add(qty);
    }

    pub fn merge(&mut self, o: SideVolume) {
        self.buy = self.buy.saturating_add(o.buy);
        self.sell = self.sell.saturating_add(o.sell);
        self.unknown = self.unknown.saturating_add(o.unknown);
    }

    pub fn total(&self) -> Qty {
        self.buy
            .saturating_add(self.sell)
            .saturating_add(self.unknown)
    }
}

/// Volume per exact price, split by side.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PriceVolume {
    levels: BTreeMap<i64, SideVolume>,
    total: Qty,
}

impl PriceVolume {
    pub fn add(&mut self, price: Price, qty: Qty, aggressor: Option<Aggressor>) {
        self.levels.entry(price.0).or_default().add(qty, aggressor);
        self.total = self.total.saturating_add(qty);
    }

    pub fn total(&self) -> Qty {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub fn len(&self) -> usize {
        self.levels.len()
    }

    pub fn get(&self, price: Price) -> SideVolume {
        self.levels.get(&price.0).copied().unwrap_or_default()
    }

    /// Every level, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (Price, SideVolume)> + '_ {
        self.levels.iter().map(|(p, v)| (Price(*p), *v))
    }
}

/// One session's profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionProfile {
    pub session: Session,
    pub volume: PriceVolume,
    /// Times of the first and last trade counted in it.
    pub first_trade: UtcMillis,
    pub last_trade: UtcMillis,
}

/// Volume per price per bar (the visible-range profile's source). Bars are keyed by their
/// open time; each bar's levels are kept sorted by price in a small vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Footprint {
    period_ms: i64,
    max_bars: usize,
    bars: BTreeMap<i64, Vec<(i64, SideVolume)>>,
}

impl Footprint {
    pub fn new(period_ms: i64, max_bars: usize) -> Footprint {
        Footprint {
            period_ms: period_ms.max(1),
            max_bars: max_bars.max(1),
            bars: BTreeMap::new(),
        }
    }

    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }

    pub fn add(&mut self, time: UtcMillis, price: Price, qty: Qty, aggressor: Option<Aggressor>) {
        // Saturating: trade times come from untrusted input, and `i64::MIN` has no bucket start.
        let start = time
            .0
            .checked_sub(time.0.rem_euclid(self.period_ms))
            .unwrap_or(i64::MIN);
        let bar = self.bars.entry(start).or_default();
        match bar.binary_search_by_key(&price.0, |(p, _)| *p) {
            Ok(i) => bar[i].1.add(qty, aggressor),
            Err(i) => bar.insert(i, (price.0, SideVolume::of(qty, aggressor))),
        }
        while self.bars.len() > self.max_bars {
            self.bars.pop_first();
        }
    }

    pub fn clear(&mut self) {
        self.bars.clear();
    }

    /// Open time of the oldest bar held.
    pub fn first_bar(&self) -> Option<UtcMillis> {
        self.bars.keys().next().map(|t| UtcMillis(*t))
    }

    /// Levels of the bars opening in `[from, to)`, bar by bar (prices ascending within a bar).
    pub fn range(
        &self,
        from: UtcMillis,
        to: UtcMillis,
    ) -> impl Iterator<Item = (Price, SideVolume)> + '_ {
        let r = (from.0 < to.0).then(|| self.bars.range(from.0..to.0));
        r.into_iter()
            .flatten()
            .flat_map(|(_, levels)| levels.iter().map(|(p, v)| (Price(*p), *v)))
    }
}

/// Session and footprint profiles of one contract, updated per trade.
#[derive(Clone, Debug)]
pub struct SessionProfiles {
    calendar: Option<&'static SessionCalendar>,
    eth: VecDeque<SessionProfile>,
    rth: VecDeque<SessionProfile>,
    footprint: Footprint,
    data_start: Option<UtcMillis>,
    /// Trades outside every ETH session (the daily break, or a calendar gap).
    outside_eth: u64,
    /// Changes on every added trade (views use it to skip unchanged rebuilds).
    version: u64,
}

impl SessionProfiles {
    /// Sessions kept per kind (composite profiles span at most this many).
    pub const MAX_SESSIONS: usize = 30;

    /// `calendar` = the product's session calendar (`None`: footprint only).
    pub fn new(
        calendar: Option<&'static SessionCalendar>,
        period_ms: i64,
        max_bars: usize,
    ) -> SessionProfiles {
        SessionProfiles {
            calendar,
            eth: VecDeque::new(),
            rth: VecDeque::new(),
            footprint: Footprint::new(period_ms, max_bars),
            data_start: None,
            outside_eth: 0,
            version: 0,
        }
    }

    pub fn calendar(&self) -> Option<&'static SessionCalendar> {
        self.calendar
    }

    /// Count one trade (only trades the stitch counts as new, like `VolumeAtPrice`).
    pub fn add(&mut self, time: UtcMillis, price: Price, qty: Qty, aggressor: Option<Aggressor>) {
        self.version = self.version.wrapping_add(1);
        self.data_start = Some(self.data_start.map_or(time, |s| s.min(time)));
        self.footprint.add(time, price, qty, aggressor);
        let Some(cal) = self.calendar else {
            return;
        };
        // Trade times come from untrusted input. The calendar supports 1900..=2999 and must not
        // be asked about times near or past those ends (its session search would leave the
        // range), so such trades are in no session. Real market data is far inside.
        if !session_range().contains(&time.0) {
            self.outside_eth += 1;
            return;
        }
        if !add_to(
            &mut self.eth,
            cal,
            SessionKind::Eth,
            time,
            price,
            qty,
            aggressor,
        ) {
            self.outside_eth += 1;
        }
        add_to(
            &mut self.rth,
            cal,
            SessionKind::Rth,
            time,
            price,
            qty,
            aggressor,
        );
    }

    pub fn clear(&mut self) {
        self.eth.clear();
        self.rth.clear();
        self.footprint.clear();
        self.data_start = None;
        self.outside_eth = 0;
        self.version = self.version.wrapping_add(1);
    }

    /// Sessions of `kind` with trades, oldest first.
    pub fn sessions(&self, kind: SessionKind) -> &VecDeque<SessionProfile> {
        match kind {
            SessionKind::Eth => &self.eth,
            SessionKind::Rth => &self.rth,
        }
    }

    /// The `kind` session to show at `now`: the one containing `now` (`None` if it has no
    /// trades yet), or between sessions the newest one with trades.
    pub fn current(&self, kind: SessionKind, now: UtcMillis) -> Option<&SessionProfile> {
        let newest = self.sessions(kind).back()?;
        match self.calendar.and_then(|c| c.session_at(now, kind)) {
            Some(s) if s.open != newest.session.open => None,
            _ => Some(newest),
        }
    }

    pub fn footprint(&self) -> &Footprint {
        &self.footprint
    }

    /// Time of the first counted trade: data before it is missing, not zero.
    pub fn data_start(&self) -> Option<UtcMillis> {
        self.data_start
    }

    pub fn outside_eth(&self) -> u64 {
        self.outside_eth
    }

    pub fn version(&self) -> u64 {
        self.version
    }
}

/// Times (ms) the session calendar is asked about: well inside its supported 1900..=2999.
fn session_range() -> std::ops::Range<i64> {
    let day = |y, m, d| crate::time::days_from_civil(y, m, d) * 86_400_000;
    day(1900, 1, 8)..day(2999, 11, 30)
}

/// Add a trade to the session of `kind` containing it, creating that session's profile (in
/// time order) if needed. `false` if no session of that kind contains `time`.
fn add_to(
    list: &mut VecDeque<SessionProfile>,
    cal: &SessionCalendar,
    kind: SessionKind,
    time: UtcMillis,
    price: Price,
    qty: Qty,
    aggressor: Option<Aggressor>,
) -> bool {
    // Fast path: the newest session (almost every trade).
    let i = match list.iter().rposition(|p| p.session.contains(time)) {
        Some(i) => i,
        None => {
            let Some(session) = cal.session_at(time, kind) else {
                return false;
            };
            let i = list.partition_point(|p| p.session.open < session.open);
            list.insert(
                i,
                SessionProfile {
                    session,
                    volume: PriceVolume::default(),
                    first_trade: time,
                    last_trade: time,
                },
            );
            if list.len() > SessionProfiles::MAX_SESSIONS {
                list.pop_front();
                if i == 0 {
                    return true; // older than everything kept
                }
                i - 1
            } else {
                i
            }
        }
    };
    let p = &mut list[i];
    p.volume.add(price, qty, aggressor);
    p.first_trade = p.first_trade.min(time);
    p.last_trade = p.last_trade.max(time);
    true
}

/// One grouped row of a profile.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileRow {
    pub volume: SideVolume,
    pub total: Qty,
}

/// A profile grouped into rows, with its point of control and value area. Reused across
/// frames: [`ProfileRows::build`] does not allocate within the buffer's capacity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileRows {
    /// Rows from low to high price; `rows[i]` is row index `base_row + i`.
    pub rows: Vec<ProfileRow>,
    pub base_row: i64,
    /// Price units per row (`grouping × tick`).
    pub row_units: i64,
    pub total: Qty,
    /// Largest row total (scales the bars).
    pub max_row: Qty,
    /// Index into `rows`.
    pub poc: Option<usize>,
    /// Inclusive index range into `rows`.
    pub value_area: Option<(usize, usize)>,
}

impl ProfileRows {
    pub fn with_capacity(rows: usize) -> ProfileRows {
        ProfileRows {
            rows: Vec::with_capacity(rows),
            ..ProfileRows::default()
        }
    }

    /// Lowest price of row `i`.
    pub fn row_price(&self, i: usize) -> Price {
        Price((self.base_row + i as i64).saturating_mul(self.row_units))
    }

    /// The smallest grouping that is at least `wanted` and fits a profile spanning
    /// `[lo, hi]` into `capacity` rows: `wanted`, else the DOM presets, then ×10 steps of 25.
    pub fn fit_grouping(wanted: u32, tick: i64, lo: Price, hi: Price, capacity: usize) -> u32 {
        let fits = |g: u32| {
            let u = (g as i64 * tick).max(1);
            row_of(hi, u) - row_of(lo, u) < capacity.max(1) as i64
        };
        let wanted = wanted.max(1);
        if fits(wanted) {
            return wanted;
        }
        let mut g = GROUPING_PRESETS
            .iter()
            .copied()
            .find(|&g| g > wanted && fits(g));
        let mut step = 250;
        while g.is_none() {
            if fits(step) {
                g = Some(step);
            }
            step = step.saturating_mul(10);
        }
        g.unwrap_or(u32::MAX)
    }

    /// Group `levels` into rows of `row_units` and compute POC and value area. `levels` is
    /// called twice (extent, then sums); it may yield the same price several times (e.g.
    /// one per session or bar).
    pub fn build<I>(&mut self, row_units: i64, value_area_percent: u8, levels: impl Fn() -> I)
    where
        I: Iterator<Item = (Price, SideVolume)>,
    {
        let u = row_units.max(1);
        self.rows.clear();
        self.row_units = u;
        self.total = Qty::ZERO;
        self.max_row = Qty::ZERO;
        self.poc = None;
        self.value_area = None;
        let mut extent: Option<(i64, i64)> = None;
        for (p, v) in levels() {
            if v.total().0 == 0 {
                continue;
            }
            let r = row_of(p, u);
            extent = Some(extent.map_or((r, r), |(a, b)| (a.min(r), b.max(r))));
        }
        let Some((lo, hi)) = extent else {
            self.base_row = 0;
            return;
        };
        self.base_row = lo;
        self.rows
            .resize((hi - lo + 1) as usize, ProfileRow::default());
        for (p, v) in levels() {
            let r = &mut self.rows[(row_of(p, u) - lo) as usize];
            r.volume.merge(v);
        }
        for r in &mut self.rows {
            r.total = r.volume.total();
            self.total = self.total.saturating_add(r.total);
            self.max_row = self.max_row.max(r.total);
        }
        if self.total.0 <= 0 {
            return;
        }
        // POC: largest row; ties nearest the middle (compared in half rows), then lower.
        let last = self.rows.len() - 1;
        let off_mid = |i: usize| (2 * i as i64 - last as i64).abs();
        let mut poc = 0;
        for i in 1..self.rows.len() {
            let (a, b) = (self.rows[i].total, self.rows[poc].total);
            if a > b || (a == b && off_mid(i) < off_mid(poc)) {
                poc = i;
            }
        }
        self.poc = Some(poc);
        // Value area: grow from the POC, larger neighbour first, both on a tie.
        let pct = i128::from(value_area_percent.clamp(1, 100));
        let target_met = |v: Qty| 100 * i128::from(v.0) >= pct * i128::from(self.total.0);
        let (mut a, mut b) = (poc, poc);
        let mut va = self.rows[poc].total;
        while !target_met(va) && (a > 0 || b < last) {
            let below = (a > 0).then(|| self.rows[a - 1].total);
            let above = (b < last).then(|| self.rows[b + 1].total);
            match (below, above) {
                (Some(x), Some(y)) if x == y => {
                    a -= 1;
                    b += 1;
                    va = va.saturating_add(x).saturating_add(y);
                }
                (Some(x), Some(y)) if x > y => {
                    a -= 1;
                    va = va.saturating_add(x);
                }
                (_, Some(y)) => {
                    b += 1;
                    va = va.saturating_add(y);
                }
                (Some(x), None) => {
                    a -= 1;
                    va = va.saturating_add(x);
                }
                (None, None) => break,
            }
        }
        self.value_area = Some((a, b));
    }
}

#[cfg(test)]
mod tests;
