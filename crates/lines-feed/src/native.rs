//! `lines-events-1`: Lines' own frame encoding of normalized market events, for data that
//! has no vendor wire protocol (e.g. files imported from Sierra Chart, roadmap 3b).
//!
//! A recording with provider id [`PROVIDER`] holds one binary frame per *atomic batch*
//! (e.g. one depth update batch); replay applies each frame to the store under one lock, so
//! the book is never observed mid-batch. All values are already in catalog-product units.
//!
//! Frame layout: a sequence of events, little-endian:
//! ```text
//! 0x01 Trade      i64 time_us | i64 price | i64 qty | u8 aggressor (0 unknown, 1 buy, 2 sell)
//! 0x02 BookClear  i64 time_us
//! 0x03 BookLevel  i64 time_us | u8 side (0 bid, 1 ask) | i64 price | i64 qty (0 = remove) | u32 orders
//! 0x04 Bbo        i64 time_us | u8 mask (1 bid, 2 ask) | i64 bid | i64 ask
//! 0x05 OptionQuote i64 time_us | u16 year | u8 month | u8 day | i64 strike | u8 right (0 call, 1 put)
//!                 | u8 mask (1 bid, 2 ask, 4 last, 8 iv, 16 delta) | i64 bid | i64 ask | i64 last
//!                 | i64 volume | i64 open_interest | i32 iv_bp | i32 delta_e4
//! ```
//! An option quote is on the recording's own product (its underlying, decision 0028); its
//! strike and prices are in that product's price units. Tag 5 appears only in recordings of
//! equities (`.lfr` v4, store segments v2), which builds before it never open.
//! Times are microseconds since the Unix epoch (UTC); the store keeps milliseconds.

use lines_core::{
    Capabilities, CivilDate, ContractKey, OptionKey, OptionQuote, Price, Product, Qty, Right, Side,
    UtcMillis,
};

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
    /// An option on the recording's product. Its quote's `time` isn't encoded: decoding sets
    /// it to `time_us` (in ms), so encode a quote stamped that way to round-trip it.
    OptionQuote {
        time_us: i64,
        key: OptionKey,
        quote: OptionQuote,
    },
}

impl NativeEvent {
    pub fn time_us(&self) -> i64 {
        match *self {
            NativeEvent::Trade { time_us, .. }
            | NativeEvent::BookClear { time_us }
            | NativeEvent::BookLevel { time_us, .. }
            | NativeEvent::Bbo { time_us, .. }
            | NativeEvent::OptionQuote { time_us, .. } => time_us,
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
        NativeEvent::OptionQuote {
            time_us,
            key,
            quote: q,
        } => {
            buf.push(5);
            buf.extend_from_slice(&time_us.to_le_bytes());
            buf.extend_from_slice(&key.expiry.year.to_le_bytes());
            buf.extend_from_slice(&[key.expiry.month, key.expiry.day]);
            buf.extend_from_slice(&key.strike.0.to_le_bytes());
            buf.push(match key.right {
                Right::Call => 0,
                Right::Put => 1,
            });
            buf.push(
                q.bid.is_some() as u8
                    | (q.ask.is_some() as u8) << 1
                    | (q.last.is_some() as u8) << 2
                    | (q.iv_bp.is_some() as u8) << 3
                    | (q.delta_e4.is_some() as u8) << 4,
            );
            for p in [q.bid, q.ask, q.last] {
                buf.extend_from_slice(&p.map_or(0, |p| p.0).to_le_bytes());
            }
            buf.extend_from_slice(&q.volume.0.to_le_bytes());
            buf.extend_from_slice(&q.open_interest.to_le_bytes());
            buf.extend_from_slice(&q.iv_bp.unwrap_or(0).to_le_bytes());
            buf.extend_from_slice(&q.delta_e4.unwrap_or(0).to_le_bytes());
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
    fn i32(&mut self) -> Result<i32, SessionError> {
        Ok(i32::from_le_bytes(self.take()?))
    }
    fn u16(&mut self) -> Result<u16, SessionError> {
        Ok(u16::from_le_bytes(self.take()?))
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
            5 => {
                let (y, m, d) = (c.u16()?, c.u8()?, c.u8()?);
                let expiry = CivilDate::new(y, m, d).ok_or_else(|| bad("bad option expiry"))?;
                let strike = Price(c.i64()?);
                if strike.0 <= 0 {
                    return Err(bad("bad option strike"));
                }
                let right = match c.u8()? {
                    0 => Right::Call,
                    1 => Right::Put,
                    r => return Err(bad(&format!("bad option right {r}"))),
                };
                let mask = c.u8()?;
                if mask >= 32 {
                    return Err(bad(&format!("bad option quote mask {mask}")));
                }
                let (bid, ask, last) = (c.i64()?, c.i64()?, c.i64()?);
                let (volume, open_interest) = (c.i64()?, c.i64()?);
                let (iv, delta) = (c.i32()?, c.i32()?);
                if bid < 0 || ask < 0 || last < 0 || volume < 0 || open_interest < 0 {
                    return Err(bad("negative option quote value"));
                }
                let on = |bit: u8| mask & bit != 0;
                NativeEvent::OptionQuote {
                    time_us,
                    key: OptionKey {
                        expiry,
                        strike,
                        right,
                    },
                    quote: OptionQuote {
                        bid: on(1).then_some(Price(bid)),
                        ask: on(2).then_some(Price(ask)),
                        last: on(4).then_some(Price(last)),
                        volume: Qty(volume),
                        open_interest,
                        iv_bp: on(8).then_some(iv),
                        delta_e4: on(16).then_some(delta),
                        time: UtcMillis(time_us.div_euclid(1000)),
                    },
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
    fn supports(&self, product: &Product) -> bool {
        catalog::get(product.id).is_some()
    }
    fn new_session(
        &self,
        product: &Product,
        _contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        Ok(Box::new(NativeSession {
            ready: false,
            scratch: Vec::new(),
            options: product.options().is_some(),
        }))
    }
}

pub fn protocol_compatible(protocol: &str) -> bool {
    protocol == PROTOCOL
}

struct NativeSession {
    ready: bool,
    scratch: Vec<NativeEvent>,
    /// The product lists options: option quotes are its chain (else they are rejected).
    options: bool,
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
                    aggressor,
                } => events.push(MarketEvent::Trade {
                    time: UtcMillis(time_us.div_euclid(1000)),
                    price,
                    qty,
                    aggressor: match aggressor {
                        Aggressor::Buy => Some(lines_core::Aggressor::Buy),
                        Aggressor::Sell => Some(lines_core::Aggressor::Sell),
                        Aggressor::Unknown => None,
                    },
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
                NativeEvent::OptionQuote { key, quote, .. } if self.options => {
                    events.push(MarketEvent::OptionQuote { key, quote })
                }
                NativeEvent::OptionQuote { .. } => events.push(MarketEvent::Rejected { count: 1 }),
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
            NativeEvent::OptionQuote {
                time_us: 5_000,
                key: OptionKey {
                    expiry: CivilDate::new(2026, 11, 9).unwrap(),
                    strike: Price(68_400),
                    right: Right::Put,
                },
                quote: OptionQuote {
                    bid: Some(Price(53)),
                    ask: None,
                    last: Some(Price(54)),
                    volume: Qty(18_331),
                    open_interest: 24_560,
                    iv_bp: Some(1_290),
                    delta_e4: Some(-4_900),
                    time: UtcMillis(5),
                },
            },
        ];
        // Malformed option quotes: unknown mask bits, a negative price.
        let opt = |f: fn(&mut Vec<u8>)| {
            let mut b = Vec::new();
            encode(&evs[5], &mut b);
            f(&mut b);
            decode(&b, &mut Vec::new()).is_err()
        };
        // tag(1) time(8) date(4) strike(8) right(1) = 22: the mask byte.
        assert!(opt(|b| b[22] = 32));
        assert!(opt(|b| b[23..31].copy_from_slice(&(-1i64).to_le_bytes())));
        assert!(
            opt(|b| b[13..21].copy_from_slice(&0i64.to_le_bytes())),
            "zero strike"
        );
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
            .new_session(&catalog::get(catalog::NQ).unwrap(), None)
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
                    qty: Qty(2),
                    aggressor: Some(lines_core::Aggressor::Buy),
                }
            ]
        );
    }

    /// An option quote is SPY's chain; on a product without options (NQ) it is rejected.
    #[test]
    fn option_quotes_need_a_product_with_options() {
        let key = OptionKey {
            expiry: CivilDate::new(2026, 11, 9).unwrap(),
            strike: Price(68_400),
            right: Right::Call,
        };
        let quote = OptionQuote {
            bid: Some(Price(80)),
            time: UtcMillis(1),
            ..Default::default()
        };
        let mut buf = Vec::new();
        encode(
            &NativeEvent::OptionQuote {
                time_us: 1_000,
                key,
                quote,
            },
            &mut buf,
        );
        let run = |id| {
            let mut s = NativeProvider
                .new_session(&catalog::get(id).unwrap(), None)
                .unwrap();
            let mut ev = Vec::new();
            s.on_frame(
                &Frame::binary(buf.clone()),
                UtcMillis(0),
                &mut ev,
                &mut Vec::new(),
            )
            .unwrap();
            ev
        };
        assert_eq!(
            run(catalog::SPY),
            vec![MarketEvent::Ready, MarketEvent::OptionQuote { key, quote }]
        );
        assert_eq!(
            run(catalog::NQ),
            vec![MarketEvent::Ready, MarketEvent::Rejected { count: 1 }]
        );
    }
}
