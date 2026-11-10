//! A live source that refuses the login (credentials or permissions) marks its store
//! (`ChartStore::logon_refused`), so the app moves panes off it (decision 0030); a symbol the
//! server doesn't have, a recording, or a dropped connection never does.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use lines_core::{Capabilities, ContractKey, Product, ProductId, UtcMillis};
use lines_feed::catalog::{self, Target};
use lines_feed::extension::{BoxFuture, DynTransport, ExtensionSource, SourceExtension};
use lines_feed::provider::{
    Frame, MarketEvent, Outgoing, Provider, ProviderSession, SessionError, StartContext,
};
use lines_feed::store::FeedStatus;
use lines_feed::transport::Transport;
use lines_feed::{FeedConfig, Source, new_store, run_feed};
use tokio::sync::watch;

/// What the made-up server answers the logon with.
#[derive(Clone, Copy)]
enum Answer {
    /// "logon rejected (code 13) permission denied", as a test login is answered.
    Refused,
    /// The symbol isn't there (an entitlement for one product, not the login).
    NoSymbol,
    /// Refused before connecting (a session that knows its login can't work).
    RefusedAtSession,
}

struct Refuser(Answer);

impl Provider for Refuser {
    fn id(&self) -> &'static str {
        "refuser-ext"
    }
    fn protocol(&self) -> String {
        "refuser-1".into()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn supports(&self, product: &Product) -> bool {
        product.id == catalog::ES
    }
    fn new_session(
        &self,
        _product: &Product,
        _contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError> {
        if let Answer::RefusedAtSession = self.0 {
            return Err(SessionError::LogonRejected {
                code: 13,
                text: "permission denied".into(),
            });
        }
        Ok(Box::new(Session(self.0)))
    }
}

struct Session(Answer);

impl ProviderSession for Session {
    fn start(&mut self, _ctx: StartContext, out: &mut Vec<Outgoing>) {
        out.push(Outgoing::Frame(Frame::text("logon")));
    }

    fn on_frame(
        &mut self,
        _frame: &Frame,
        _now: UtcMillis,
        _events: &mut Vec<MarketEvent>,
        _out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        Err(match self.0 {
            Answer::Refused => SessionError::LogonRejected {
                code: 13,
                text: "permission denied".into(),
            },
            Answer::NoSymbol | Answer::RefusedAtSession => {
                SessionError::Unavailable("symbol not found".into())
            }
        })
    }
}

/// Answers every frame with one reply.
struct Server;

impl Transport for Server {
    async fn send(&mut self, _frame: Frame) -> Result<()> {
        Ok(())
    }
    async fn recv(&mut self) -> Result<Option<Frame>> {
        Ok(Some(Frame::text("reply")))
    }
}

struct Ext(Answer);

impl SourceExtension for Ext {
    fn api_version(&self) -> u32 {
        lines_feed::extension::API_VERSION
    }
    fn name(&self) -> &'static str {
        "refuser"
    }
    fn help(&self) -> &'static str {
        "A test-only source that refuses."
    }
    fn provider(&self) -> Arc<dyn Provider> {
        Arc::new(Refuser(self.0))
    }
    fn default_product(&self) -> ProductId {
        catalog::ES
    }
    fn connect(
        &self,
        _product: &Product,
        _contract: Option<ContractKey>,
    ) -> BoxFuture<'static, Result<Box<dyn DynTransport>>> {
        Box::pin(async { Ok(Box::new(Server) as Box<dyn DynTransport>) })
    }
    fn local_store(&self) -> bool {
        false
    }
}

async fn run(answer: Answer) -> Arc<lines_feed::store::ChartStore> {
    let mut config = FeedConfig::new(Source::Extension(ExtensionSource(Arc::new(Ext(answer)))));
    config.target = Some(Target::Product(catalog::ES));
    let store = new_store(&config).unwrap();
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .expect("a refused feed stops");
    store
}

#[tokio::test]
async fn a_refused_login_marks_the_store() {
    let store = run(Answer::Refused).await;
    assert!(
        matches!(&store.read().status, FeedStatus::Failed(m) if m.contains("code 13")),
        "{:?}",
        store.read().status
    );
    assert!(store.logon_refused());
    // Refused when the session is made, before any connection: the same.
    assert!(run(Answer::RefusedAtSession).await.logon_refused());
}

#[tokio::test]
async fn a_missing_symbol_is_not_a_refused_login() {
    let store = run(Answer::NoSymbol).await;
    assert!(matches!(&store.read().status, FeedStatus::Failed(_)));
    assert!(!store.logon_refused());
}
