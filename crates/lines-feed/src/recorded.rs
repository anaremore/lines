//! A recording or stored session's market events, decoded synchronously, without threads or
//! timing (backtests, decision 0025). The frames go through the recording's own provider
//! session exactly as a replay feeds them (decision 0011): the session's clock is each frame's
//! recorded time, and `start` sees the first recorded frame's time. The same file always gives
//! the same events.

use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use lines_core::{ContractKey, Product, UtcMillis};

use crate::disk::replay::SegmentReplay;
use crate::provider::{Frame, MarketEvent, Outgoing, ProviderSession, StartContext};
use crate::recording::{Direction, Header, Reader};

enum Frames {
    Recording(Box<Reader<BufReader<std::fs::File>>>),
    Stored(Box<SegmentReplay>),
}

impl Frames {
    /// The next inbound frame and its recorded time.
    fn next(&mut self) -> Result<Option<(UtcMillis, Frame)>> {
        match self {
            Frames::Recording(r) => loop {
                match r.next_record()? {
                    None => return Ok(None),
                    Some(rec) if rec.direction == Direction::FromServer => {
                        return Ok(Some((UtcMillis(rec.wall_ms), rec.frame)));
                    }
                    Some(_) => continue,
                }
            },
            // `open` already read the first stored frame ahead: it comes first.
            Frames::Stored(s) => Ok(s
                .next_unpaced()?
                .map(|(us, f)| (UtcMillis(us.div_euclid(1000)), f))),
        }
    }
}

/// Market events from a `.lfr` recording or a stored contract or day.
pub struct RecordedEvents {
    header: Header,
    product: Product,
    contract: Option<ContractKey>,
    session: Box<dyn ProviderSession>,
    frames: Frames,
    started: bool,
    history_ms: i64,
    /// Frames the session refused (counted, as a replay counts them; never fatal).
    decode_errors: u64,
}

impl RecordedEvents {
    pub fn open(path: &Path) -> Result<RecordedEvents> {
        let header = crate::transport::read_header(path)?;
        let source = crate::Source::Replay {
            path: path.to_path_buf(),
            speed: 0.0,
            stop_at: None,
        };
        let provider = crate::provider_for(&source)?;
        let history_ms = crate::FeedConfig::new(source).history_ms;
        let product = crate::catalog::by_symbol(&header.product)
            .ok_or_else(|| anyhow!("recording is for unknown product {:?}", header.product))?;
        if let Some(k) = header.contract
            && k.product != product.id
        {
            bail!(
                "corrupt recording header: contract is not a {} contract",
                product.symbol
            );
        }
        if !provider.supports(&product) {
            bail!("{} does not provide {}", provider.id(), product.symbol);
        }
        let session = provider
            .new_session(&product, header.contract)
            .map_err(|e| anyhow!("{e}"))?;
        let frames = if crate::disk::is_stored(path) {
            Frames::Stored(Box::new(SegmentReplay::open(path, 0.0, None)?))
        } else {
            Frames::Recording(Box::new(
                Reader::open(path).with_context(|| format!("open recording {}", path.display()))?,
            ))
        };
        Ok(RecordedEvents {
            contract: header.contract,
            header,
            product,
            session,
            frames,
            started: false,
            history_ms,
            decode_errors: 0,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn product(&self) -> &Product {
        &self.product
    }

    /// The recorded contract (`None`: an old recording that doesn't say).
    pub fn contract(&self) -> Option<ContractKey> {
        self.contract
    }

    /// Frames the provider session refused so far.
    pub fn decode_errors(&self) -> u64 {
        self.decode_errors
    }

    /// Recorded frames refused so far because their time isn't representable (a `.lfr`
    /// header or offset that overflows; `recording::wall_ms`).
    pub fn unrepresentable(&self) -> u64 {
        match &self.frames {
            Frames::Recording(r) => r.unrepresentable(),
            Frames::Stored(_) => 0,
        }
    }

    /// The next frame's events and its recorded time; `None` at the end. Frames that decode
    /// to nothing are skipped.
    pub fn next_batch(&mut self) -> Result<Option<(UtcMillis, Vec<MarketEvent>)>> {
        let mut out: Vec<Outgoing> = Vec::new();
        loop {
            let Some((t, frame)) = self.frames.next()? else {
                return Ok(None);
            };
            if !self.started {
                self.started = true;
                self.session.start(
                    StartContext {
                        now: t,
                        history_from: None,
                        bar_period_ms: UtcMillis::MINUTE,
                        history_ms: self.history_ms,
                    },
                    &mut out,
                );
            }
            let mut events = Vec::new();
            if self
                .session
                .on_frame(&frame, t, &mut events, &mut out)
                .is_err()
            {
                self.decode_errors += 1;
            }
            out.clear();
            if !events.is_empty() {
                return Ok(Some((t, events)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lines_core::{Month, Price, Qty};

    use super::*;
    use crate::native::{self, Aggressor, NativeEvent};
    use crate::recording::{FORMAT_VERSION, Writer};

    #[test]
    fn a_recording_decodes_in_order_with_recorded_times() {
        let dir = std::env::temp_dir().join(format!("lines-recorded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.lfr");
        let es = ContractKey::monthly(crate::catalog::ES, 2026, Month::new(12).unwrap());
        let start = 1_800_000_000_000;
        let header = Header {
            start_wall_ms: start,
            provider: native::PROVIDER.into(),
            protocol: native::PROTOCOL.into(),
            product: "ES".into(),
            contract: Some(es),
            version: FORMAT_VERSION,
        };
        let mut w = Writer::create(&path, &header).unwrap();
        for i in 0..3i64 {
            let mut buf = Vec::new();
            native::encode(
                &NativeEvent::Trade {
                    time_us: (start + i * 500) * 1000,
                    price: Price(600_000 + i * 25),
                    qty: Qty(1),
                    aggressor: Aggressor::Buy,
                },
                &mut buf,
            );
            w.write(
                Direction::FromServer,
                Duration::from_millis(i as u64 * 500),
                &Frame::binary(buf),
            )
            .unwrap();
        }
        // An outbound frame is never decoded.
        w.write(
            Direction::ToServer,
            Duration::from_millis(1600),
            &Frame::binary(vec![1]),
        )
        .unwrap();
        w.finish().unwrap();

        let mut r = RecordedEvents::open(&path).unwrap();
        assert_eq!(r.product().symbol, "ES");
        assert_eq!(r.contract(), Some(es));
        let mut got = Vec::new();
        while let Some((t, events)) = r.next_batch().unwrap() {
            for e in events {
                if let MarketEvent::Trade { price, .. } = e {
                    got.push((t.0 - start, price.0));
                }
            }
        }
        assert_eq!(got, vec![(0, 600_000), (500, 600_025), (1000, 600_050)]);
        assert_eq!(r.decode_errors(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A frame whose recorded time doesn't fit (a crafted header near `i64::MAX`) is skipped
    /// and counted for the backtest's warning, never given a wrapped time.
    #[test]
    fn frames_with_unrepresentable_times_are_counted() {
        let dir = std::env::temp_dir().join(format!("lines-recorded-ovf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.lfr");
        let header = Header {
            start_wall_ms: i64::MAX - 1_000,
            provider: native::PROVIDER.into(),
            protocol: native::PROTOCOL.into(),
            product: "ES".into(),
            contract: None,
            version: FORMAT_VERSION,
        };
        let mut w = Writer::create(&path, &header).unwrap();
        for ms in [0, 2_000, 3_000, 500] {
            w.write(
                Direction::FromServer,
                Duration::from_millis(ms),
                &Frame::binary(vec![0xff]),
            )
            .unwrap();
        }
        w.finish().unwrap();

        let mut r = RecordedEvents::open(&path).unwrap();
        while r.next_batch().unwrap().is_some() {}
        assert_eq!(r.unrepresentable(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stored session decodes from its first frame: `SegmentReplay::open` reads that frame
    /// ahead, and it once got lost here (a backtest or tick-bar rebuild on a stored session
    /// missed its first trades).
    #[test]
    fn a_stored_session_keeps_its_first_frame() {
        use crate::disk::ContractDir;
        use crate::disk::segment::{FlushPolicy, SegmentHeader, SegmentWriter};

        let dir =
            std::env::temp_dir().join(format!("lines-recorded-stored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let es = ContractKey::monthly(crate::catalog::ES, 2026, Month::new(12).unwrap());
        let product = crate::catalog::get(crate::catalog::ES).unwrap();
        let start = UtcMillis::parse_iso("2026-11-05T15:00:00Z").unwrap().0;
        let cdir = ContractDir::new(&dir, "mock", &product, Some(es));
        let date = crate::disk::partition::trade_date(&product, UtcMillis(start));
        let header = SegmentHeader {
            created_wall_ms: start,
            source: "mock".into(),
            encoding: native::PROTOCOL.into(),
            product: "ES".into(),
            contract: Some(es),
            date,
        };
        let path = cdir.segment_path(date);
        let (mut w, _) = SegmentWriter::open(&path, &header, FlushPolicy::default()).unwrap();
        for i in 0..3i64 {
            let mut buf = Vec::new();
            native::encode(
                &NativeEvent::Trade {
                    time_us: (start + i * 500) * 1000,
                    price: Price(600_000 + i * 25),
                    qty: Qty(1),
                    aggressor: Aggressor::Buy,
                },
                &mut buf,
            );
            w.append((start + i * 500) * 1000, &buf).unwrap();
        }
        w.close().unwrap();

        let mut r = RecordedEvents::open(&path).unwrap();
        let mut got = Vec::new();
        while let Some((t, events)) = r.next_batch().unwrap() {
            for e in events {
                if let MarketEvent::Trade { price, .. } = e {
                    got.push((t.0 - start, price.0));
                }
            }
        }
        assert_eq!(got, vec![(0, 600_000), (500, 600_025), (1000, 600_050)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
