//! Market data as the trading core needs it (paper fills, marks, risk bands), converted from
//! the feed's normalized events. Tests script these directly.

use lines_core::{
    ContractKey, Delivery, Price, ProductId, Qty, Right, Side as BookSide, UtcMillis,
};
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
    /// The last trade price without a trade (seeding a mark when following a running feed):
    /// moves marks, never fills an order.
    Last(Price),
    /// A listed option's own quote (decision 0028): the whole quote, replacing the last. Paper
    /// fills use it (see `paper`). `None`, zero or a crossed side count as missing
    /// ([`quote_sides`]).
    OptionQuote {
        bid: Option<Price>,
        ask: Option<Price>,
        last: Option<Price>,
        time: UtcMillis,
    },
}

/// The usable sides of an option quote: a side at or under zero is no side, and a crossed quote
/// has neither (nothing ever fills on it).
pub fn quote_sides(bid: Option<Price>, ask: Option<Price>) -> (Option<Price>, Option<Price>) {
    let bid = bid.filter(|p| p.0 > 0);
    let ask = ask.filter(|p| p.0 > 0);
    match (bid, ask) {
        (Some(b), Some(a)) if b >= a => (None, None),
        sides => sides,
    }
}

fn option_input(q: &lines_core::OptionQuote) -> MarketInput {
    MarketInput::OptionQuote {
        bid: q.bid,
        ask: q.ask,
        last: q.last,
        time: q.time,
    }
}

/// The option quotes in one feed batch on `product`, each for its own series' contract.
pub fn option_quotes(
    product: ProductId,
    events: &[MarketEvent],
) -> Vec<(ContractKey, MarketInput)> {
    events
        .iter()
        .filter_map(|e| match e {
            MarketEvent::OptionQuote { key, quote } => Some((
                ContractKey {
                    product,
                    delivery: Delivery::Option(*key),
                },
                option_input(quote),
            )),
            _ => None,
        })
        .collect()
}

/// Every series' quote a store holds now (seeding the core when it starts following a running
/// feed: quotes that came before stay current until the series quotes again).
pub fn seed_option_quotes(
    product: ProductId,
    chains: &lines_core::OptionChains,
) -> Vec<(ContractKey, MarketInput)> {
    let mut out = Vec::new();
    for expiry in chains.expiries() {
        let Some(chain) = chains.chain(expiry) else {
            continue;
        };
        for row in &chain.rows {
            for right in [Right::Call, Right::Put] {
                if let Some(q) = row.side(right) {
                    let key = ContractKey::option(product, expiry, row.strike, right);
                    out.push((key, option_input(q)));
                }
            }
        }
    }
    out
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
            // A source dropping its depth empties the book: paper never fills against levels
            // the feed no longer shows.
            MarketEvent::BookClear | MarketEvent::DepthUnavailable { .. } => {
                Some(MarketInput::BookClear)
            }
            MarketEvent::Bid(p) => Some(MarketInput::Bid(p)),
            MarketEvent::Ask(p) => Some(MarketInput::Ask(p)),
            MarketEvent::LastPrice(p) => Some(MarketInput::Last(p)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    #[test]
    fn a_source_dropping_depth_clears_the_paper_book() {
        let reason = "Top of book only".to_owned();
        let inputs = from_events(&[MarketEvent::DepthUnavailable { reason }]);
        assert!(matches!(inputs[..], [MarketInput::BookClear]), "{inputs:?}");
    }
}
