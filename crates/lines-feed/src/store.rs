//! State shared between the feed thread (writer) and the UI thread (reader).
//!
//! Rules that keep the UI from ever stalling on the feed:
//! - The feed decodes protobuf *outside* the lock and holds the lock only to apply one
//!   frame's worth of already-decoded updates (microseconds).
//! - Every tick is applied to the bars on the feed side, so nothing is lost if the UI is
//!   slow; the UI only *samples* the latest state.
//! - The UI checks [`ChartStore::revision`] (an atomic, no lock) once per frame and uses
//!   `try_lock`, so a contended frame simply redraws the previous scene.

use std::sync::atomic::{AtomicU64, Ordering};

use lines_core::{BarSeries, Price, PriceFormat, UtcMillis};
use parking_lot::{Mutex, MutexGuard};

#[derive(Clone, Debug, PartialEq)]
pub struct InstrumentInfo {
    /// Symbol the user asked for (e.g. "EP").
    pub requested_symbol: String,
    /// Resolved contract symbol (e.g. "EPZ6").
    pub contract_symbol: String,
    pub title: String,
    /// Session-scoped: changes across reconnects, never persist it.
    pub contract_id: u32,
    pub format: PriceFormat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedStatus {
    Connecting,
    LoggingOn,
    Resolving,
    Live,
    Reconnecting { attempt: u32, reason: String },
    ReplayFinished,
    Failed(String),
    Stopped,
}

impl FeedStatus {
    pub fn label(&self) -> &str {
        match self {
            FeedStatus::Connecting => "connecting",
            FeedStatus::LoggingOn => "logging on",
            FeedStatus::Resolving => "resolving symbol",
            FeedStatus::Live => "live",
            FeedStatus::Reconnecting { .. } => "reconnecting",
            FeedStatus::ReplayFinished => "replay finished",
            FeedStatus::Failed(_) => "failed",
            FeedStatus::Stopped => "stopped",
        }
    }
}

/// Monotonic counters (the UI derives rates from deltas).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedStats {
    pub frames_in: u64,
    pub bytes_in: u64,
    pub quotes_in: u64,
    pub trades_in: u64,
    pub corrections_ignored: u64,
    pub decode_errors: u64,
    pub reconnects: u64,
}

#[derive(Debug)]
pub struct ChartData {
    pub instrument: Option<InstrumentInfo>,
    pub series: BarSeries,
    pub last_trade: Option<Price>,
    pub bid: Option<Price>,
    pub ask: Option<Price>,
    pub last_quote_time: Option<UtcMillis>,
    pub status: FeedStatus,
    pub stats: FeedStats,
}

#[derive(Debug)]
pub struct ChartStore {
    data: Mutex<ChartData>,
    revision: AtomicU64,
}

impl ChartStore {
    pub fn new(period_ms: i64, max_bars: usize) -> Self {
        ChartStore {
            data: Mutex::new(ChartData {
                instrument: None,
                series: BarSeries::new(period_ms, max_bars),
                last_trade: None,
                bid: None,
                ask: None,
                last_quote_time: None,
                status: FeedStatus::Connecting,
                stats: FeedStats::default(),
            }),
            revision: AtomicU64::new(0),
        }
    }

    /// Lock-free change detector for the UI.
    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Feed side: mutate and publish.
    pub fn update<R>(&self, f: impl FnOnce(&mut ChartData) -> R) -> R {
        let r = f(&mut self.data.lock());
        self.revision.fetch_add(1, Ordering::Release);
        r
    }

    /// UI side: never blocks. `None` means the feed holds the lock right now.
    #[inline]
    pub fn try_read(&self) -> Option<MutexGuard<'_, ChartData>> {
        self.data.try_lock()
    }

    /// Blocking read, for tests and non-UI callers.
    pub fn read(&self) -> MutexGuard<'_, ChartData> {
        self.data.lock()
    }

    pub fn set_status(&self, status: FeedStatus) {
        self.update(|d| d.status = status);
    }
}
