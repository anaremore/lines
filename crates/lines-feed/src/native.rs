//! `lines-events-1`: Lines' own frame encoding of normalized market events, for data that
//! has no vendor wire protocol (e.g. files imported from Sierra Chart, roadmap 3b).
//!
//! A recording with provider id [`PROVIDER`] holds one binary frame per *atomic batch*
//! (e.g. one depth update batch); replay applies each frame to the store under one lock, so
//! the book is never observed mid-batch. All values are already in catalog-instrument units.
//!
//! Frame layout: a sequence of events, little-endian:
//! ```text
//! 0x01 Trade      i64 time_us | i64 price | i64 qty | u8 aggressor (0 unknown, 1 buy, 2 sell)
//! 0x02 BookClear  i64 time_us
//! 0x03 BookLevel  i64 time_us | u8 side (0 bid, 1 ask) | i64 price | i64 qty (0 = remove) | u32 orders
//! 0x04 Bbo        i64 time_us | u8 mask (1 bid, 2 ask) | i64 bid | i64 ask
//! ```
//! Times are microseconds since the Unix epoch (UTC); the store keeps milliseconds.

use lines_core::{Capabilities, Instrument, Price, Qty, Side, UtcMillis};

use crate::catalog;
use crate::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, Provider, ProviderSession, SessionError, StartContext,
};

pub const PROVIDER: &str = "lines";
pub const PROTOCOL: &str = "lines-events-1";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Aggressor {
    Unknown = 0,
    Buy = 1,
    Sell = 2,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NativeEvent {
    Trade {
        time_us: i64,
        price: Price,
        qty: Qty,
        aggressor: Aggressor,
    },
    BookClear {
        time_us: i64,
    },
    BookLevel {
        time_us: i64,
        side: Side,
        price: Price,
        qty: Qty,
        orders: u32,
    },
    Bbo {
        time_us: i64,
        bid: Option<Price>,
        ask: Option<Price>,
    },
}

impl NativeEvent {
    pub fn time_us(&self) -> i64 {
        match *self {
            NativeEvent::Trade { time_us, .. }
            | NativeEvent::BookClear { time_us }
            | NativeEvent::BookLevel { time_us, .. }
            | NativeEvent::Bbo { time_us, .. } => time_us,
        }
    }
}

/// Append one event's encoding to `buf`.
pub fn encode(e: &NativeEvent, buf: &mut Vec<u8>) {
    match *e {
        NativeEvent::Trade {
            time_us,
            price,
            qty,
            aggressor,
        } => {
            buf.push(1);
            buf.extend_from_slice(&time_us.to_le_bytes());
            buf.extend_from_slice(&price.0.to_le_bytes());
            buf.extend_from_slice(&qty.0.to_le_bytes());
            buf.push(aggressor as u8);
        }
        NativeEvent::BookClear { time_us } => {
            buf.push(2);
            buf.extend_from_slice(&time_us.to_le_bytes());
        }
        NativeEvent::BookLevel {
            time_us,
            side,
            price,
            qty,
            orders,
        } => {
            buf.push(3);
            buf.extend_from_slice(&time_us.to_le_bytes());
            buf.push(match side {
                Side::Bid => 0,
                Side::Ask => 1,
            });
            buf.extend_from_slice(&price.0.to_le_bytes());
            buf.extend_from_slice(&qty.0.to_le_bytes());
            buf.extend_from_slice(&orders.to_le_bytes());
        }
        NativeEvent::Bbo { time_us, bid, ask } => {
            buf.push(4);
            buf.extend_from_slice(&time_us.to_le_bytes());
            buf.push(bid.is_some() as u8 | (ask.is_some() as u8) << 1);
            buf.extend_from_slice(&bid.map_or(0, |p| p.0).to_le_bytes());
            buf.extend_from_slice(&ask.map_or(0, |p| p.0).to_le_bytes());
        }
    }
}

struct Cursor<'a> {
    b: &'a [u8],
}

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], SessionError> {
        if self.b.len() < N {
            return Err(SessionError::Protocol(
                "truncated lines-events frame".into(),
            ));
        }
        let (head, rest) = self.b.split_at(N);
        self.b = rest;
        Ok(head.try_into().expect("length checked"))
    }
    fn u8(&mut self) -> Result<u8, SessionError> {
        Ok(self.take::<1>()?[0])
    }
    fn i64(&mut self) -> Result<i64, SessionError> {
        Ok(i64::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, SessionError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
}

/// Decode a whole frame.
pub fn decode(data: &[u8], out: &mut Vec<NativeEvent>) -> Result<(), SessionError> {
    let mut c = Cursor { b: data };
    let bad = |what: &str| SessionError::Protocol(format!("lines-events: {what}"));
    while !c.b.is_empty() {
        let tag = c.u8()?;
        let time_us = c.i64()?;
        out.push(match tag {
            1 => NativeEvent::Trade {
                time_us,
                price: Price(c.i64()?),
                qty: Qty(c.i64()?),
                aggressor: match c.u8()? {
                    0 => Aggressor::Unknown,
                    1 => Aggressor::Buy,
                    2 => Aggressor::Sell,
                    a => return Err(bad(&format!("bad aggressor {a}"))),
                },
            },
            2 => NativeEvent::BookClear { time_us },
            3 => NativeEvent::BookLevel {
                time_us,
                side: match c.u8()? {
                    0 => Side::Bid,
                    1 => Side::Ask,
                    s => return Err(bad(&format!("bad side {s}"))),
                },
                price: Price(c.i64()?),
                qty: Qty(c.i64()?),
                orders: c.u32()?,
            },
            4 => {
                let mask = c.u8()?;
                let (b, a) = (c.i64()?, c.i64()?);
                NativeEvent::Bbo {
                    time_us,
                    bid: (mask & 1 != 0).then_some(Price(b)),
                    ask: (mask & 2 != 0).then_some(Price(a)),
                }
            }
            t => return Err(bad(&format!("unknown event tag {t}"))),
        });
    }
    Ok(())
}

/// Replay-only provider for `lines-events` recordings.
#[derive(Default)]
pub struct NativeProvider;

impl Provider for NativeProvider {
    fn id(&self) -> &'static str {
        PROVIDER
    }
    fn protocol(&self) -> String {
        PROTOCOL.into()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            history: false,
            bbo: true,
            depth: true,
            fractional_qty: true,
            always_open: false,
        }
    }
    fn supports(&self, instrument: &Instrument) -> bool {
        catalog::get(instrument.id).is_some()
    }
    fn new_session(
        &self,
        _instrument: &Instrument,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        Ok(Box::new(NativeSession {
            ready: false,
            scratch: Vec::new(),
        }))
    }
}

pub fn protocol_compatible(protocol: &str) -> bool {
    protocol == PROTOCOL
}

struct NativeSession {
    ready: bool,
    scratch: Vec<NativeEvent>,
}

impl ProviderSession for NativeSession {
    fn start(&mut self, _ctx: StartContext, _out: &mut Vec<Outgoing>) {}

    fn on_frame(
        &mut self,
        frame: &Frame,
        _now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        _out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        if frame.kind != FrameKind::Binary {
            return Err(SessionError::Protocol(format!(
                "unexpected {:?} frame in lines-events",
                frame.kind
            )));
        }
        if !self.ready {
            self.ready = true;
            events.push(MarketEvent::Ready);
        }
        self.scratch.clear();
        decode(&frame.data, &mut self.scratch)?;
        for e in &self.scratch {
            match *e {
                NativeEvent::Trade {
                    time_us,
                    price,
                    qty,
                    ..
                } => events.push(MarketEvent::Trade {
                    time: UtcMillis(time_us.div_euclid(1000)),
                    price,
                    qty,
                }),
                NativeEvent::BookClear { .. } => events.push(MarketEvent::BookClear),
                NativeEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                    ..
                } => events.push(MarketEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                }),
                NativeEvent::Bbo { bid, ask, .. } => {
                    events.extend(bid.map(MarketEvent::Bid));
                    events.extend(ask.map(MarketEvent::Ask));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_every_event() {
        let evs = [
            NativeEvent::BookClear { time_us: 1 },
            NativeEvent::BookLevel {
                time_us: 2,
                side: Side::Bid,
                price: Price(2_412_875),
                qty: Qty(3),
                orders: 2,
            },
            NativeEvent::BookLevel {
                time_us: 2,
                side: Side::Ask,
                price: Price(-5),
                qty: Qty(0),
                orders: 0,
            },
            NativeEvent::Trade {
                time_us: 3,
                price: Price(2_412_900),
                qty: Qty(1),
                aggressor: Aggressor::Sell,
            },
            NativeEvent::Bbo {
                time_us: 4,
                bid: None,
                ask: Some(Price(7)),
            },
        ];
        let mut buf = Vec::new();
        for e in &evs {
            encode(e, &mut buf);
        }
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert_eq!(out, evs);
        assert!(
            decode(&buf[..buf.len() - 1], &mut Vec::new()).is_err(),
            "truncation detected"
        );
        assert!(
            decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0], &mut Vec::new()).is_err(),
            "unknown tag"
        );
    }

    #[test]
    fn session_maps_to_market_events() {
        let mut s = NativeProvider
            .new_session(&catalog::get(catalog::NQ).unwrap())
            .unwrap();
        let mut buf = Vec::new();
        encode(&NativeEvent::BookClear { time_us: 0 }, &mut buf);
        encode(
            &NativeEvent::Trade {
                time_us: 1_500_999,
                price: Price(10),
                qty: Qty(2),
                aggressor: Aggressor::Buy,
            },
            &mut buf,
        );
        let mut ev = Vec::new();
        s.on_frame(&Frame::binary(buf), UtcMillis(0), &mut ev, &mut Vec::new())
            .unwrap();
        assert_eq!(
            ev,
            vec![
                MarketEvent::Ready,
                MarketEvent::BookClear,
                MarketEvent::Trade {
                    time: UtcMillis(1_500),
                    price: Price(10),
                    qty: Qty(2)
                }
            ]
        );
    }
}
