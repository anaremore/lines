//! What every pane shares: it is given a [`PaneFeed`] and a theme, owns its own view state,
//! and redraws only when its own store changed.
//!
//! Each pane polls *its* store's atomic revision on the UI thread every 8 ms and notifies only
//! itself (decision 0001: never per tick). The workspace renders panes as cached views, so a
//! pane whose market didn't move replays its previous frame instead of repainting: N panes
//! don't cost N× when one market ticks.

use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, EventEmitter};
use lines_feed::store::ChartStore;

use crate::feeds::PaneFeed;

/// A pane switched itself to another feed (e.g. the chart's symbol picker). The workspace
/// forwards it to the other panes in the same link group.
#[derive(Clone)]
pub struct FeedChanged(pub PaneFeed);

pub trait Pane: EventEmitter<FeedChanged> + Sized + 'static {
    fn store(&self) -> &Arc<ChartStore>;
    /// The revision this pane last drew.
    fn seen_revision(&mut self) -> &mut u64;
    /// Follow another feed (a linked pane switched). Resets per-market view state.
    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>);
    /// Called on every poll tick (e.g. rate counters); default nothing.
    fn on_tick(&mut self) {}
}

/// Start the pane's revision poll: notify when its store changed, nothing otherwise.
pub fn watch_store<V: Pane>(cx: &mut Context<V>) {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(8))
                .await;
            let alive = this.update(cx, |pane, cx| {
                pane.on_tick();
                let rev = pane.store().revision();
                let seen = pane.seen_revision();
                if rev != *seen {
                    *seen = rev;
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// The pane header's contract text: the active contract is always shown (decision 0013);
/// unknown is labelled, not hidden.
pub fn contract_text(d: &lines_feed::ChartData) -> String {
    match d.contract_label() {
        Some(c) => c.to_string(),
        None => format!("{} (contract unknown)", d.product.symbol),
    }
}
