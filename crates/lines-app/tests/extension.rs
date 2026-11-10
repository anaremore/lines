//! The provider extension point (decision 0020): a source registered from outside the public
//! crates runs through the same entry point, connection loop, recording and replay as a
//! built-in one. The dummy here speaks a made-up text protocol over an in-memory transport.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use futures_util::future::BoxFuture;
use lines_app::{DynTransport, Extensions, SourceExtension, Timing};
use lines_core::{Capabilities, ContractKey, Price, Product, ProductId, Qty, UtcMillis};
use lines_feed::catalog;
use lines_feed::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, Provider, ProviderSession, SessionError, StartContext,
};
use lines_feed::transport::Transport;

const NAME: &str = "dummy";
const PROVIDER: &str = "dummy-ext";
const PROTOCOL: &str = "dummy-text-1";

/// Frames the dummy server sent across all connections (proves the transport ran).
static SENT: AtomicU64 = AtomicU64::new(0);
/// Subscribe requests the dummy server received.
static SUBSCRIBES: AtomicU64 = AtomicU64::new(0);

struct DummyProvider;

impl Provider for DummyProvider {
    fn id(&self) -> &'static str {
        PROVIDER
    }
    fn protocol(&self) -> String {
        PROTOCOL.into()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            bbo: true,
            ..Default::default()
        }
    }
    fn supports(&self, product: &Product) -> bool {
        product.id == catalog::ES
    }
    fn new_session(
        &self,
        product: &Product,
        _contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        Ok(Box::new(DummySession {
            tick: product.price.tick,
        }))
    }
}

/// `T <ms> <ticks> <qty>` is a trade, `B <ticks>` a bid; everything else is ignored.
struct DummySession {
    tick: i64,
}

impl ProviderSession for DummySession {
    fn start(&mut self, _ctx: StartContext, out: &mut Vec<Outgoing>) {
        out.push(Outgoing::Frame(Frame::text("subscribe ES")));
    }

    fn on_frame(
        &mut self,
        frame: &Frame,
        _now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        _out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let text = std::str::from_utf8(&frame.data)
            .map_err(|_| SessionError::Protocol("not text".into()))?;
        let f: Vec<&str> = text.split(' ').collect();
        let num = |i: usize| -> Result<i64, SessionError> {
            f.get(i)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| SessionError::Protocol(format!("bad frame {text:?}")))
        };
        match f[0] {
            "hello" => events.push(MarketEvent::Ready),
            "T" => events.push(MarketEvent::Trade {
                time: UtcMillis(num(1)?),
                price: Price(num(2)? * self.tick),
                qty: Qty(num(3)?),
                aggressor: None,
            }),
            "B" => events.push(MarketEvent::Bid(Price(num(1)? * self.tick))),
            _ => {}
        }
        Ok(())
    }
}

/// In-memory server: greets, then waits for the subscribe and sends a trade and a bid every
/// 5 ms.
struct DummyTransport {
    subscribed: bool,
    greeted: bool,
    n: i64,
}

impl Transport for DummyTransport {
    async fn send(&mut self, frame: Frame) -> Result<()> {
        if frame.data.as_ref() == b"subscribe ES" {
            SUBSCRIBES.fetch_add(1, Ordering::Relaxed);
            self.subscribed = true;
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Frame>> {
        if !self.greeted {
            self.greeted = true;
            return Ok(Some(Frame::text("hello")));
        }
        if !self.subscribed {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.n += 1;
        SENT.fetch_add(1, Ordering::Relaxed);
        let ticks = 27_000 + self.n % 7;
        let text = if self.n % 2 == 0 {
            format!("T {} {ticks} {}", lines_feed::now_utc().0, 1 + self.n % 3)
        } else {
            format!("B {}", ticks - 1)
        };
        Ok(Some(Frame {
            kind: FrameKind::Text,
            data: text.into(),
        }))
    }
}

struct DummyExtension;

impl SourceExtension for DummyExtension {
    fn api_version(&self) -> u32 {
        lines_feed::extension::API_VERSION
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn help(&self) -> &'static str {
        "A test-only source on an in-memory transport."
    }
    fn long_help(&self) -> Option<&'static str> {
        Some("Dummy: no credentials needed.")
    }
    fn provider(&self) -> Arc<dyn Provider> {
        Arc::new(DummyProvider)
    }
    fn default_product(&self) -> ProductId {
        catalog::ES
    }
    fn connect(
        &self,
        _product: &Product,
        _contract: Option<ContractKey>,
    ) -> BoxFuture<'static, Result<Box<dyn DynTransport>>> {
        Box::pin(async {
            Ok(Box::new(DummyTransport {
                subscribed: false,
                greeted: false,
                n: 0,
            }) as Box<dyn DynTransport>)
        })
    }
    fn timing(&self) -> Timing {
        Timing {
            stale_after: Duration::from_secs(7),
            ..Timing::default()
        }
    }
}

fn extensions() -> Extensions {
    Extensions::new().with_source(Arc::new(DummyExtension))
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lines-extension-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

#[test]
fn registered_source_runs_headless_records_and_replays() {
    let rec = temp("dummy.lfr");
    let rec_arg = rec.to_str().unwrap();
    lines_app::run_with_args(
        [
            "lines",
            "--source",
            NAME,
            "--headless",
            "1",
            "--no-store",
            "--record",
            rec_arg,
        ],
        extensions(),
    )
    .expect("headless run on the extension source");
    assert!(SUBSCRIBES.load(Ordering::Relaxed) >= 1, "session started");
    assert!(SENT.load(Ordering::Relaxed) > 10, "frames flowed");

    // The recording names the extension's provider, so this build can replay it.
    let h = lines_feed::transport::read_header(&rec).unwrap();
    assert_eq!(
        (h.provider.as_str(), h.protocol.as_str()),
        (PROVIDER, PROTOCOL)
    );
    assert_eq!(h.product, "ES");
    assert!(
        h.contract.is_some(),
        "a product target follows its front month"
    );
    assert_eq!(lines_feed::stale_after(PROVIDER), Duration::from_secs(7));

    // Replay through the entry point, then through the feed directly to count what it saw.
    lines_app::run_with_args(
        [
            "lines",
            "--source",
            "replay",
            "--replay",
            rec_arg,
            "--speed",
            "0",
            "--headless",
            "5",
        ],
        extensions(),
    )
    .expect("headless replay of the extension's recording");
    let replay = || {
        let mut feed = lines_feed::spawn(lines_feed::FeedConfig::new(lines_feed::Source::Replay {
            path: rec.clone(),
            speed: 0.0,
            stop_at: None,
        }))
        .unwrap();
        let t0 = std::time::Instant::now();
        while feed.store.read().status != lines_feed::store::FeedStatus::ReplayFinished {
            assert!(t0.elapsed() < Duration::from_secs(10), "replay finishes");
            std::thread::sleep(Duration::from_millis(5));
        }
        let d = feed.store.read();
        let out = (d.stats.trades_in, d.series.bars().to_vec(), d.provider);
        drop(d);
        feed.shutdown();
        out
    };
    let (trades, bars, provider) = replay();
    assert!(trades > 5, "trades replayed: {trades}");
    assert_eq!(provider, PROVIDER);
    assert_eq!(replay().1, bars, "replay is deterministic");
}

#[test]
fn extension_availability_and_registration_rules() {
    lines_feed::extension::register(Arc::new(DummyExtension)).unwrap();
    let ext = lines_feed::extension::by_name(NAME).unwrap();
    let a = lines_feed::availability::availability(&lines_feed::Source::Extension(
        lines_feed::extension::ExtensionSource(ext),
    ))
    .unwrap();
    assert_eq!(a.provider_id(), PROVIDER);
    assert_eq!(a.check(&catalog::Target::Product(catalog::ES)), Ok(()));
    assert_eq!(
        a.check(&catalog::Target::Product(catalog::NQ)),
        Err("not mapped for dummy-ext".into())
    );

    struct Named(&'static str);
    impl SourceExtension for Named {
        fn api_version(&self) -> u32 {
            lines_feed::extension::API_VERSION
        }
        fn name(&self) -> &'static str {
            self.0
        }
        fn help(&self) -> &'static str {
            ""
        }
        fn provider(&self) -> Arc<dyn Provider> {
            Arc::new(DummyProvider)
        }
        fn default_product(&self) -> ProductId {
            catalog::ES
        }
        fn connect(
            &self,
            _: &Product,
            _: Option<ContractKey>,
        ) -> BoxFuture<'static, Result<Box<dyn DynTransport>>> {
            Box::pin(async { anyhow::bail!("never connects") })
        }
    }
    // Decision 0030 §10: a connector built for another interface version is refused.
    struct Old;
    impl SourceExtension for Old {
        fn api_version(&self) -> u32 {
            1
        }
        fn name(&self) -> &'static str {
            "old-ext"
        }
        fn help(&self) -> &'static str {
            ""
        }
        fn provider(&self) -> Arc<dyn Provider> {
            Arc::new(DummyProvider)
        }
        fn default_product(&self) -> ProductId {
            catalog::ES
        }
        fn connect(
            &self,
            _: &Product,
            _: Option<ContractKey>,
        ) -> BoxFuture<'static, Result<Box<dyn DynTransport>>> {
            Box::pin(async { anyhow::bail!("never connects") })
        }
    }
    let err = lines_feed::extension::register(Arc::new(Old)).unwrap_err();
    assert!(
        err.to_string().contains("built for extension interface 1"),
        "{err}"
    );
    assert!(lines_feed::extension::by_name("old-ext").is_none());
    use lines_feed::extension::register;
    assert!(
        register(Arc::new(Named("kraken"))).is_err(),
        "built-in name"
    );
    assert!(register(Arc::new(Named("Bad Name"))).is_err(), "malformed");
    assert!(
        register(Arc::new(Named("other"))).is_err(),
        "a second extension can't claim the same provider id"
    );
}
