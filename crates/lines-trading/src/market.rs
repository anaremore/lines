//! Market data as the trading core needs it (paper fills, marks, risk bands), converted from
//! the feed's normalized events. Tests script these directly.

use lines_core::{Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::provider::MarketEvent;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MarketInput {
    Trade {
        time: UtcMillis,
        price: Price,
        qty: Qty,
    },
    /// The new total at a price; 0 removes the level.
    Book {
        side: BookSide,
        price: Price,
        qty: Qty,
    },
    BookClear,
    Bid(Price),
    Ask(Price),
}

/// The inputs in one feed batch (other events are ignored).
pub fn from_events(events: &[MarketEvent]) -> Vec<MarketInput> {
    events
        .iter()
        .filter_map(|e| match *e {
            MarketEvent::Trade {
                time, price, qty, ..
            } => Some(MarketInput::Trade { time, price, qty }),
            MarketEvent::BookLevel {
                side, price, qty, ..
            } => Some(MarketInput::Book { side, price, qty }),
            MarketEvent::BookClear => Some(MarketInput::BookClear),
            MarketEvent::Bid(p) => Some(MarketInput::Bid(p)),
            MarketEvent::Ask(p) => Some(MarketInput::Ask(p)),
            _ => None,
        })
        .collect()
}
