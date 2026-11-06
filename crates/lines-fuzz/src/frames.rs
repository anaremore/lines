//! The byte layout session harnesses read: a sequence of frames plus clock steps.
//!
//! ```text
//! input := cfg:u8 op*
//! op    := tag:u8 len:u16le payload[len]     (len is clamped to the bytes left)
//! tag   := bits 0-1 kind (0 binary, 1 text, 2 http, 3 keepalive tick; payload ignored)
//!          bits 2-7 clock advance in seconds before the op (0..=63)
//! ```
//!
//! Every byte string is a valid input, so the fuzzer never wastes runs on framing.

use bytes::Bytes;
use lines_feed::provider::{Frame, FrameKind};

pub struct Op {
    pub advance_ms: i64,
    /// `None` = call the session's keep-alive.
    pub frame: Option<Frame>,
    /// The payload, also for keep-alive ops (harnesses that aren't sessions use all four
    /// kinds as data).
    pub payload: Bytes,
}

/// Split off the config byte (0 for empty input).
pub fn split(data: &[u8]) -> (u8, &[u8]) {
    match data.split_first() {
        Some((&c, rest)) => (c, rest),
        None => (0, &[]),
    }
}

pub struct Ops<'a> {
    rest: &'a [u8],
}

impl<'a> Ops<'a> {
    pub fn new(ops: &'a [u8]) -> Self {
        Ops { rest: ops }
    }
}

impl Iterator for Ops<'_> {
    type Item = Op;

    fn next(&mut self) -> Option<Op> {
        let (&tag, rest) = self.rest.split_first()?;
        let (len, rest) = match rest {
            [a, b, rest @ ..] => (u16::from_le_bytes([*a, *b]) as usize, rest),
            _ => (0, &[][..]),
        };
        let (payload, rest) = rest.split_at(len.min(rest.len()));
        self.rest = rest;
        let kind = match tag & 3 {
            0 => Some(FrameKind::Binary),
            1 => Some(FrameKind::Text),
            2 => Some(FrameKind::Http),
            _ => None,
        };
        let payload = Bytes::copy_from_slice(payload);
        Some(Op {
            advance_ms: (tag >> 2) as i64 * 1000,
            frame: kind.map(|kind| Frame {
                kind,
                data: payload.clone(),
            }),
            payload,
        })
    }
}

/// Encode ops `(kind, advance seconds, payload)` for seeds (`None` kind = keep-alive tick).
/// Seed payloads must be under 64 KiB.
pub fn encode(cfg: u8, ops: &[(Option<FrameKind>, u8, &[u8])]) -> Vec<u8> {
    let mut v = vec![cfg];
    for &(kind, advance_s, payload) in ops {
        let k = match kind {
            Some(FrameKind::Binary) => 0,
            Some(FrameKind::Text) => 1,
            Some(FrameKind::Http) => 2,
            None => 3,
        };
        let len = u16::try_from(payload.len()).expect("seed frames are under 64 KiB");
        v.push(k | advance_s.min(63) << 2);
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(payload);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_truncation() {
        let enc = encode(
            7,
            &[
                (Some(FrameKind::Text), 2, b"{}"),
                (None, 63, b""),
                (Some(FrameKind::Http), 0, b"x"),
            ],
        );
        let (cfg, ops) = split(&enc);
        assert_eq!(cfg, 7);
        let ops: Vec<_> = Ops::new(ops).collect();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].advance_ms, 2000);
        assert_eq!(ops[0].frame.as_ref().unwrap().data.as_ref(), b"{}");
        assert!(ops[1].frame.is_none());
        assert_eq!(ops[2].frame.as_ref().unwrap().kind, FrameKind::Http);
        // Truncated length and payload still parse.
        assert_eq!(Ops::new(&[1, 5]).count(), 1);
        assert_eq!(
            Ops::new(&[1, 5, 0, b'a'])
                .next()
                .unwrap()
                .frame
                .unwrap()
                .data
                .len(),
            1
        );
        assert_eq!(split(&[]), (0, &[][..]));
    }
}
