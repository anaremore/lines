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

/// The feed restarted its data from an earlier point (a replay jumped back): its listeners get
/// an empty batch, after every batch from before the jump and before any replayed one, so they
/// can tell the two apart without timestamps.
pub(crate) fn restarted(feed: u64) {
    let mut map = LISTENERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = map.get_mut(&feed) {
        let marker: Batch = Arc::from(Vec::<MarketEvent>::new());
        list.retain(|tx| tx.send(marker.clone()).is_ok());
    }
}

/// The feed driver's side.
pub(crate) struct ListenTap {
    pub feed: u64,
}

impl EventSink for ListenTap {
    fn frame(&mut self, events: &[MarketEvent], _now: UtcMillis) {
        // A frame with no events (a keep-alive, an ack) isn't sent: an empty batch is only
        // ever the restart marker ([`restarted`]).
        if events.is_empty() {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{Price, Qty};

    /// Empty frames never reach listeners; the restart marker (empty) does, in its place
    /// between the batches before it and after it.
    #[test]
    fn only_the_restart_marker_is_empty() {
        let feed = 0xFEED_0001;
        let rx = listen(feed);
        let mut tap = ListenTap { feed };
        let trade = MarketEvent::Trade {
            time: UtcMillis(1),
            price: Price(100),
            qty: Qty(1),
            aggressor: None,
        };
        tap.frame(std::slice::from_ref(&trade), UtcMillis(1));
        tap.frame(&[], UtcMillis(2));
        restarted(feed);
        tap.frame(std::slice::from_ref(&trade), UtcMillis(3));
        let got: Vec<usize> = rx.try_iter().map(|b| b.len()).collect();
        assert_eq!(
            got,
            [1, 0, 1],
            "a batch, the marker, a batch: no empty frame"
        );
    }
}
