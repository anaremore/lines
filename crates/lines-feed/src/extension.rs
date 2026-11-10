//! Provider extension point (decision 0020).
//!
//! Sources that are not built in (connectors kept outside this repository) register a
//! [`SourceExtension`]: its CLI name and help, a [`Provider`] (id, capabilities, symbol
//! rules, I/O-free sessions per decision 0011), availability rules and a connector that opens
//! the connection's [`DynTransport`]. The feed then drives it exactly like a built-in source:
//! same connection loop, reconnect backoff, recording, local store and indicators.
//!
//! Registered extensions are process-wide ([`register`]) so a recording made with one can be
//! replayed by the same build: [`crate::Source::Replay`] looks the recording's provider up
//! here when it isn't a built-in one.
//!
//! Nothing here names a vendor. The public `lines` app registers one built-in source this way
//! (Databento, decision 0033, from the `lines-databento` crate); official builds add theirs.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail};
pub use futures_util::future::BoxFuture;
use lines_core::{ContractKey, Product, ProductId};
use parking_lot::RwLock;

use crate::provider::{Frame, Provider};
use crate::transport::Transport;

/// The extension interface's version (decision 0030 §10). A connector says which version it
/// was built against ([`SourceExtension::api_version`]); any other is refused when it
/// registers, with a message, never loaded wrong. Bumped when the interface's meaning changes.
pub const API_VERSION: u32 = 2;

/// A setting a source describes for Settings › Connections (decision 0030 §3): the public
/// app draws it and stores its value, so it never has to know the connector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Stable key, unique within the source (`api-key`): the stored value's name.
    pub key: &'static str,
    /// What the row says ("API key").
    pub label: &'static str,
    pub kind: FieldKind,
    /// Without a value the source shows "not set up" and isn't started.
    pub required: bool,
    /// An environment variable that overrides the stored value (tests, headless runs).
    pub env: Option<&'static str>,
    /// For a secret: the OS keychain entry the connector already reads itself (its target on
    /// Windows, its service on macOS and Linux), so a key saved in Settings is the one it
    /// uses. `None`: Lines' own entry (service `lines`, account `<source>.<key>`).
    pub keychain: Option<&'static str>,
}

/// How a [`Field`] is entered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// One of these values (the first is the default).
    Choice(&'static [&'static str]),
    /// `"on"` or `"off"` (absent: off).
    Switch,
    /// Kept in the OS keychain, shown masked (decision 0030 §4); never in the settings file,
    /// workspaces, logs or diagnostics.
    Secret,
}

/// Where sources' settings are kept: the app installs one ([`set_settings_store`]). Plain
/// values live in its settings file, secrets in the OS keychain. A read may block briefly (a
/// keychain): it is made on a feed's thread when the source connects, never on the UI's.
pub trait SettingsStore: Send + Sync {
    fn get(&self, source: &str, field: &Field) -> Option<String>;
}

fn store() -> &'static RwLock<Option<Arc<dyn SettingsStore>>> {
    static S: OnceLock<RwLock<Option<Arc<dyn SettingsStore>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Install where sources' settings come from (the app, at startup).
pub fn set_settings_store(s: Arc<dyn SettingsStore>) {
    *store().write() = Some(s);
}

/// A registered source's setting: its environment variable when that is set and not empty,
/// else the stored value (`None`: no such source or field, or no value). For connectors, in
/// `connect`: a secret read here must never reach a frame, a log or a recording.
pub fn setting(source: &str, key: &str) -> Option<String> {
    let ext = by_name(source)?;
    let field = ext.fields().into_iter().find(|f| f.key == key)?;
    if let Some(v) = field
        .env
        .and_then(|e| std::env::var(e).ok())
        .filter(|v| !v.is_empty())
    {
        return Some(v);
    }
    let s = store().read().clone()?;
    s.get(source, &field).filter(|v| !v.is_empty())
}

/// Names the built-in sources use on the command line; an extension can't take them.
pub const RESERVED_NAMES: &[&str] = &["mock", "cqg", "coinbase", "kraken", "replay", "lines"];

/// One connection's transport, object-safe so an extension can supply its own (e.g. several
/// sockets multiplexed into one frame stream). Every type implementing [`Transport`] is one.
///
/// Same rules as [`Transport`]: `recv` must be cancel-safe; `Ok(None)` ends the connection.
pub trait DynTransport: Send {
    fn send_dyn(&mut self, frame: Frame) -> BoxFuture<'_, Result<()>>;
    fn recv_dyn(&mut self) -> BoxFuture<'_, Result<Option<Frame>>>;
    fn request_http_dyn(&mut self, url: String);
}

impl<T: Transport> DynTransport for T {
    fn send_dyn(&mut self, frame: Frame) -> BoxFuture<'_, Result<()>> {
        Box::pin(Transport::send(self, frame))
    }
    fn recv_dyn(&mut self) -> BoxFuture<'_, Result<Option<Frame>>> {
        Box::pin(Transport::recv(self))
    }
    fn request_http_dyn(&mut self, url: String) {
        Transport::request_http(self, url)
    }
}

/// A boxed extension transport, driven by the generic connection loop.
pub struct BoxedTransport(pub Box<dyn DynTransport>);

impl Transport for BoxedTransport {
    async fn send(&mut self, frame: Frame) -> Result<()> {
        self.0.send_dyn(frame).await
    }
    async fn recv(&mut self) -> Result<Option<Frame>> {
        self.0.recv_dyn().await
    }
    fn request_http(&mut self, url: String) {
        self.0.request_http_dyn(url)
    }
}

/// Connection timing for an extension's live connections.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    /// How often the driver calls `ProviderSession::keepalive` (the session decides whether a
    /// heartbeat is due).
    pub keepalive_every: Duration,
    /// Treat the connection as dead if nothing arrives for this long.
    pub idle_timeout: Duration,
    /// Panes show the feed stale after this much silence while the market is open.
    pub stale_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            keepalive_every: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            stale_after: Duration::from_secs(5),
        }
    }
}

/// A market-data source registered from outside this crate (decision 0020).
pub trait SourceExtension: Send + Sync + 'static {
    /// The interface version this connector was built against: return [`API_VERSION`].
    fn api_version(&self) -> u32;
    /// The `--source` value, e.g. `"acme"`. Also the local store's directory for this source.
    /// Lowercase ASCII letters, digits and `-`; not one of [`RESERVED_NAMES`].
    fn name(&self) -> &'static str;
    /// One line describing the source in `--help`.
    fn help(&self) -> &'static str;
    /// Extra `--help` text (e.g. how credentials are supplied), shown after the options.
    fn long_help(&self) -> Option<&'static str> {
        None
    }
    /// The provider: id (stored in recordings), protocol, capabilities, which products it
    /// maps and its I/O-free sessions.
    fn provider(&self) -> Arc<dyn Provider>;
    /// The product shown when no target is given.
    fn default_product(&self) -> ProductId;
    /// Availability beyond [`Provider::supports`]: `Err(reason)` greys the product out in the
    /// UI with that reason. Must never allow what the feed would refuse.
    fn check(&self, product: &Product) -> Result<(), String> {
        let _ = product;
        Ok(())
    }
    /// Whether this extension's provider decodes a recording with this header (replays).
    fn decodes(&self, provider: &str, protocol: &str) -> bool {
        provider == self.provider().id() && protocol == self.provider().protocol()
    }
    /// Open one live connection for `contract` (`None`: the session decides). Runs on the
    /// feed's Tokio runtime; credentials are the extension's business and must never reach
    /// a frame the session sees or the recorder writes.
    fn connect(
        &self,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> BoxFuture<'static, Result<Box<dyn DynTransport>>>;
    fn timing(&self) -> Timing {
        Timing::default()
    }
    /// Keep live data in the local market-data store (decision 0019) unless `--no-store`.
    fn local_store(&self) -> bool {
        true
    }
    /// Whole past trade dates this source can download into the local store ("Download
    /// days…", decision 0030 §8; `crate::download`). `None`: it can't.
    fn past_days(&self) -> Option<Arc<dyn crate::download::PastDays>> {
        None
    }
    /// The settings Settings › Connections shows for this source (decision 0030 §3); read
    /// them with [`setting`].
    fn fields(&self) -> Vec<Field> {
        Vec::new()
    }
    /// What the account's plan allows, in a few words ("live CME futures, depth"), when the
    /// connector knows it. Cheap: no I/O (called while drawing Settings).
    fn plan(&self) -> Option<String> {
        None
    }
}

fn table() -> &'static RwLock<Vec<Arc<dyn SourceExtension>>> {
    static T: OnceLock<RwLock<Vec<Arc<dyn SourceExtension>>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Register an extension for this process. Registering the same name again replaces it (so
/// tests can run the entry point repeatedly); a reserved or malformed name is an error.
pub fn register(ext: Arc<dyn SourceExtension>) -> Result<()> {
    let name = ext.name();
    if ext.api_version() != API_VERSION {
        bail!(
            "extension source {name:?} was built for extension interface {}; this build of \
             Lines has {API_VERSION}: rebuild the connector against it",
            ext.api_version()
        );
    }
    let well_formed = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !well_formed {
        bail!("extension source name {name:?}: use lowercase letters, digits and '-'");
    }
    if RESERVED_NAMES.contains(&name) {
        bail!("extension source name {name:?} is taken by a built-in source");
    }
    if let Some(days) = ext.past_days()
        && days.api_version() != crate::download::API_VERSION
    {
        bail!(
            "extension source {name:?} was built for download interface v{}; this Lines \
             speaks v{}: rebuild it against this version",
            days.api_version(),
            crate::download::API_VERSION
        );
    }
    let id = ext.provider().id();
    let mut t = table().write();
    if let Some(other) = t
        .iter()
        .find(|e| e.name() != name && e.provider().id() == id)
    {
        bail!(
            "extensions {name:?} and {:?} both claim provider id {id:?}",
            other.name()
        );
    }
    t.retain(|e| e.name() != name);
    t.push(ext);
    Ok(())
}

/// Every registered extension, in registration order.
pub fn registered() -> Vec<Arc<dyn SourceExtension>> {
    table().read().clone()
}

/// The extension registered under this `--source` name.
pub fn by_name(name: &str) -> Option<Arc<dyn SourceExtension>> {
    table().read().iter().find(|e| e.name() == name).cloned()
}

/// The extension that decodes recordings with this header.
pub fn for_recording(provider: &str, protocol: &str) -> Option<Arc<dyn SourceExtension>> {
    table()
        .read()
        .iter()
        .find(|e| e.decodes(provider, protocol))
        .cloned()
}

/// The extension whose provider has this id.
pub fn by_provider(id: &str) -> Option<Arc<dyn SourceExtension>> {
    table()
        .read()
        .iter()
        .find(|e| e.provider().id() == id)
        .cloned()
}

/// A handle on a registered extension, for [`crate::Source::Extension`].
#[derive(Clone)]
pub struct ExtensionSource(pub Arc<dyn SourceExtension>);

impl std::fmt::Debug for ExtensionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ExtensionSource")
            .field(&self.0.name())
            .finish()
    }
}

impl std::ops::Deref for ExtensionSource {
    type Target = dyn SourceExtension;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
