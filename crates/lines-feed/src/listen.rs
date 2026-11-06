//! Listeners on a running feed's normalized events (e.g. the trading core's paper broker,
//! which needs every trade and book change of the contracts it trades).
//!
//! [`listen`] returns a channel that receives each decoded batch of the feed (an atomic batch:
//! book changes included). Batches are only built while someone listens; a dropped receiver is
//! forgotten on the next batch. Like the indicator engine's tap, the feed thread only sends:
//! it never waits on a listener.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex};

use lines_core::UtcMillis;

use crate::connection::EventSink;
use crate::provider::MarketEvent;

type Batch = Arc<[MarketEvent]>;

static LISTENERS: LazyLock<Mutex<HashMap<u64, Vec<Sender<Batch>>>>> =
    LazyLock::new(Default::default);

/// Receive every batch of the feed behind `ChartStore::feed_id()` from now on.
pub fn listen(feed: u64) -> Receiver<Batch> {
    let (tx, rx) = mpsc::channel();
    LISTENERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(feed)
        .or_default()
        .push(tx);
    rx
}

/// The feed driver's side.
pub(crate) struct ListenTap {
    pub feed: u64,
}

impl EventSink for ListenTap {
    fn frame(&mut self, events: &[MarketEvent], _now: UtcMillis) {
        let mut map = LISTENERS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(list) = map.get_mut(&self.feed) else {
            return;
        };
        let batch: Batch = events.into();
        list.retain(|tx| tx.send(batch.clone()).is_ok());
        if list.is_empty() {
            map.remove(&self.feed);
        }
    }
}
