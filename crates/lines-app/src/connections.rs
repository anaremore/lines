//! Settings › Connections (decisions 0030 §3–4 and 0035), GPUI-free: the sources the page lists, the
//! state each is in, the settings a source describes, and where they are kept: plain values
//! in `settings.json` (`connections`), secrets in the OS keychain (never in the settings
//! file, workspaces, logs or diagnostics). The page itself is `connections_view`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use lines_feed::extension::{self, Field, FieldKind, SettingsStore};
use lines_feed::store::FeedStatus;
use lines_trading::extension::BrokerExtension;

use crate::sources::SourcePrefs;

/// One source or broker on the page.
#[derive(Clone, Debug)]
pub struct Row {
    /// Its `--source` name (`coinbase`, an extension's), or a broker extension's name.
    pub name: String,
    pub fields: Vec<Field>,
    /// What its plan allows, in a few words.
    pub plan: String,
    /// A broker extension (decision 0020): its own label, state from its venue, no Test.
    pub broker: Option<BrokerRow>,
    /// The economic calendar's refresh (roadmap 8b, `crate::econ_refresh`): not a data
    /// source; no switch, its state is the refresh's, Refresh now in place of Test.
    pub calendar: bool,
}

/// What a broker's card needs beyond a source's.
#[derive(Clone, Debug)]
pub struct BrokerRow {
    pub label: String,
    /// Off until switched on (`BrokerExtension::opt_in`).
    pub opt_in: bool,
}

/// The run's broker extensions (registered at startup), and which of them it opened.
#[derive(Default)]
struct Brokers {
    all: Vec<Arc<dyn BrokerExtension>>,
    started: Vec<String>,
}

fn brokers() -> &'static RwLock<Brokers> {
    static B: OnceLock<RwLock<Brokers>> = OnceLock::new();
    B.get_or_init(Default::default)
}

/// The broker extensions the page lists, and those this run opened.
pub fn set_brokers(all: &[Arc<dyn BrokerExtension>], started: &[Arc<dyn BrokerExtension>]) {
    *brokers().write().unwrap_or_else(|e| e.into_inner()) = Brokers {
        all: all.to_vec(),
        started: started.iter().map(|b| b.name().to_owned()).collect(),
    };
}

/// Whether a broker is switched on: an opt-in one only when switched on, any other unless
/// switched off.
pub fn broker_on(prefs: &BTreeMap<String, SourcePrefs>, name: &str, opt_in: bool) -> bool {
    match prefs.get(name).and_then(|p| p.enabled) {
        Some(on) => on,
        None => !opt_in,
    }
}

/// Switch a source or broker on or off in the saved preferences: the default state saves as
/// nothing (on, or off for an opt-in broker), the other as an explicit choice.
pub fn switch(prefs: &mut BTreeMap<String, SourcePrefs>, name: String, on: bool, opt_in: bool) {
    if on != opt_in {
        prefs.remove(&name);
    } else {
        prefs.insert(name, SourcePrefs { enabled: Some(on) });
    }
}

/// The brokers a run opens: those switched on.
pub fn brokers_on(
    all: &[Arc<dyn BrokerExtension>],
    prefs: &BTreeMap<String, SourcePrefs>,
) -> Vec<Arc<dyn BrokerExtension>> {
    all.iter()
        .filter(|b| broker_on(prefs, b.name(), b.opt_in()))
        .cloned()
        .collect()
}

/// A registered broker extension that is off until switched on.
pub fn broker_opt_in(name: &str) -> bool {
    brokers()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .all
        .iter()
        .any(|b| b.name() == name && b.opt_in())
}

/// A broker's state: off, on at the next start, or its venue's own words while this run has
/// it open. Cheap (the venue's status is a snapshot, no I/O).
pub fn broker_state(name: &str, on: bool) -> State {
    let b = brokers().read().unwrap_or_else(|e| e.into_inner());
    let in_run = b.started.iter().any(|n| n == name);
    if !on {
        return State::Off { running: in_run };
    }
    if !in_run {
        return State::NextStart;
    }
    let status = b
        .all
        .iter()
        .find(|x| x.name() == name)
        .and_then(|x| x.accounts().status());
    match status {
        Some(v) => State::Venue {
            detail: v.detail,
            connected: v.connected,
        },
        None => State::Venue {
            detail: "started".to_owned(),
            connected: true,
        },
    }
}

/// A registered broker's fields (for the keychain's entry names).
fn broker_fields(name: &str) -> Option<Vec<Field>> {
    brokers()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .all
        .iter()
        .find(|b| b.name() == name)
        .map(|b| b.fields())
}

/// The public sources, in the order a run without `--source` prefers them.
const BUILTIN: [&str; 2] = ["coinbase", "kraken"];

/// Every source the page lists: each registered extension, then Coinbase and Kraken (the
/// developer sources, mock and replay, stay behind their flags).
pub fn rows() -> Vec<Row> {
    let ext = extension::registered();
    let row = |e: &std::sync::Arc<dyn extension::SourceExtension>| Row {
        name: e.name().to_owned(),
        fields: e.fields(),
        plan: e
            .plan()
            .unwrap_or_else(|| "Not reported by the connector".to_owned()),
        broker: None,
        calendar: false,
    };
    // The caller's own extensions, Coinbase, Kraken, then the built-in Databento: the order a
    // run prefers them in.
    let mut out: Vec<Row> = ext
        .iter()
        .filter(|e| e.name() != crate::DATABENTO)
        .map(row)
        .collect();
    out.extend(BUILTIN.iter().map(|n| Row {
        name: (*n).to_owned(),
        fields: Vec::new(),
        plan: "Public data, no account: trades, top of book and depth".to_owned(),
        broker: None,
        calendar: false,
    }));
    out.extend(ext.iter().filter(|e| e.name() == crate::DATABENTO).map(row));
    // Brokers after the data sources.
    let b = brokers().read().unwrap_or_else(|e| e.into_inner());
    out.extend(b.all.iter().map(|e| {
        Row {
            name: e.name().to_owned(),
            fields: e.fields(),
            plan: e
                .plan()
                .unwrap_or_else(|| "Not reported by the connector".to_owned()),
            broker: Some(BrokerRow {
                label: e.label().to_owned(),
                opt_in: e.opt_in(),
            }),
            calendar: false,
        }
    }));
    out.push(calendar_row());
    out
}

/// The economic calendar's card: FRED's release dates, with the user's own API key.
pub fn calendar_row() -> Row {
    Row {
        name: crate::econ_refresh::NAME.to_owned(),
        fields: vec![Field {
            key: crate::econ_refresh::KEY,
            label: "API key",
            kind: FieldKind::Secret,
            required: false,
            env: Some(crate::econ_refresh::ENV),
            keychain: None,
        }],
        plan: "Official release dates (BLS, BEA, FOMC) from FRED, St. Louis Fed; free key"
            .to_owned(),
        broker: None,
        calendar: true,
    }
}

/// A source's state, as the page states it (a word, never color alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Switched off (`running`: still serving panes until Lines restarts).
    Off {
        running: bool,
    },
    /// A required setting has no value (`started`: it is in this run, from before or from
    /// its environment variable, and won't connect without it).
    NotSetUp {
        needs: &'static str,
        started: bool,
    },
    /// On, but not part of this run (turned on, or set up, after it started).
    NextStart,
    /// In this run, and no pane shows its symbols: nothing is connected.
    Idle,
    Connecting,
    /// Serving `feeds` symbols.
    Connected {
        feeds: usize,
    },
    /// The provider refused, with its reason.
    Refused(String),
    /// A connection dropped or was refused and is being tried again, with the last reason.
    Retrying(String),
    /// Its feeds were stopped.
    Stopped,
    /// A broker this run opened, in its venue's words ("connected", "disconnected: ...").
    Venue {
        detail: String,
        connected: bool,
    },
}

/// How a state reads at a glance (paired with its words).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tone {
    Quiet,
    Busy,
    Good,
    Bad,
}

impl State {
    pub fn text(&self) -> String {
        match self {
            State::Off { running: false } => "Off".to_owned(),
            State::Off { running: true } => "Off when Lines restarts".to_owned(),
            State::NotSetUp {
                needs,
                started: false,
            } => format!("Not set up: needs {needs}, so not started"),
            State::NotSetUp {
                needs,
                started: true,
            } => format!("Not set up: needs {needs} to connect"),
            State::NextStart => "On when Lines restarts".to_owned(),
            State::Idle => "Idle: connects when a pane shows its symbols".to_owned(),
            State::Connecting => "Connecting…".to_owned(),
            State::Connected { feeds: 1 } => "Connected: 1 symbol".to_owned(),
            State::Connected { feeds } => format!("Connected: {feeds} symbols"),
            State::Refused(why) => format!("Refused: {why}"),
            State::Retrying(why) => format!("Reconnecting: {why}"),
            State::Stopped => "Stopped".to_owned(),
            State::Venue { detail, .. } => {
                let mut c = detail.chars();
                match c.next() {
                    Some(first) => first.to_uppercase().chain(c).collect(),
                    None => String::new(),
                }
            }
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            State::Off { .. } | State::NextStart | State::Idle | State::Stopped => Tone::Quiet,
            State::Connecting => Tone::Busy,
            State::Connected { .. }
            | State::Venue {
                connected: true, ..
            } => Tone::Good,
            State::Venue {
                connected: false, ..
            } => Tone::Busy,
            State::NotSetUp { .. } | State::Refused(_) | State::Retrying(_) => Tone::Bad,
        }
    }
}

/// A source's feed statuses with its refused login (`Sources::refusal`) first, as a failed
/// feed: the login's reason before any one symbol's.
pub fn with_refusal(mut feeds: Vec<FeedStatus>, refusal: Option<String>) -> Vec<FeedStatus> {
    if let Some(why) = refusal {
        feeds.insert(0, FeedStatus::Failed(why));
    }
    feeds
}

/// A source's state from its switch, whether this run has it, the first required setting
/// with no value, and the state of each of its running feeds.
pub fn state(on: bool, in_run: bool, needs: Option<&'static str>, feeds: &[FeedStatus]) -> State {
    if !on {
        return State::Off { running: in_run };
    }
    // Running feeds say what they are doing, set up or not (a key removed while connected,
    // `--source` given without one).
    if !(in_run && !feeds.is_empty()) {
        if let Some(needs) = needs {
            return State::NotSetUp {
                needs,
                started: in_run,
            };
        }
        if !in_run {
            return State::NextStart;
        }
    }
    // A problem on any of its feeds shows first: never hidden behind another's connection.
    if let Some(why) = feeds.iter().find_map(|s| match s {
        FeedStatus::Failed(why) => Some(why.clone()),
        _ => None,
    }) {
        return State::Refused(why);
    }
    if let Some(why) = feeds.iter().find_map(|s| match s {
        FeedStatus::Reconnecting { reason, .. } => Some(reason.clone()),
        _ => None,
    }) {
        return State::Retrying(why);
    }
    if feeds.is_empty() {
        return State::Idle;
    }
    if feeds.iter().all(|s| *s == FeedStatus::Stopped) {
        return State::Stopped;
    }
    let live = feeds.iter().filter(|s| **s == FeedStatus::Live).count();
    if live > 0 {
        State::Connected { feeds: live }
    } else {
        State::Connecting
    }
}

/// A saved secret as the page shows it: its last 4 characters (all hidden when it is short).
pub fn masked(secret: &str) -> String {
    let n = secret.chars().count();
    if n <= 8 {
        return "••••".to_owned();
    }
    let tail: String = secret.chars().skip(n - 4).collect();
    format!("••••{tail}")
}

/// The keychain entry's service name; each secret's account is `<source>.<field>`.
const SERVICE: &str = "lines";

enum Backend {
    Os,
    /// Screenshots and tests: never the user's keychain.
    Memory(Mutex<HashMap<String, String>>),
}

fn backend() -> &'static RwLock<Backend> {
    static B: OnceLock<RwLock<Backend>> = OnceLock::new();
    B.get_or_init(|| RwLock::new(Backend::Os))
}

/// Keep secrets in memory for this process instead of the OS keychain (screenshots, tests),
/// starting with these `(source, field, value)`s.
pub fn use_memory_keychain(seed: &[(&str, &str, &str)]) {
    let map = seed
        .iter()
        .map(|(s, k, v)| (account(s, k), (*v).to_owned()))
        .collect();
    *backend().write().unwrap_or_else(|e| e.into_inner()) = Backend::Memory(Mutex::new(map));
}

/// Whether secrets are in memory for this process (screenshots, tests): reads never block.
pub fn memory_keychain() -> bool {
    matches!(
        &*backend().read().unwrap_or_else(|e| e.into_inner()),
        Backend::Memory(_)
    )
}

/// The keychain entry a registered source's field names for itself (`Field::keychain`).
fn own_entry(source: &str, key: &str) -> Option<&'static str> {
    let fields = match extension::by_name(source) {
        Some(e) => e.fields(),
        None => broker_fields(source)?,
    };
    fields.into_iter().find(|f| f.key == key)?.keychain
}

fn account(source: &str, key: &str) -> String {
    match own_entry(source, key) {
        Some(entry) => entry.to_owned(),
        None => format!("{source}.{key}"),
    }
}

/// Where a secret lives in the OS keychain: (target, service, user). A connector's own entry
/// is the one it reads with the platform's tools, under the account its documentation gives
/// (the source's name: `cmdkey /user:databento`, `security -a databento`, `secret-tool …
/// username databento`), so saving replaces that item rather than adding a second one: on
/// Windows a generic credential whose target is the entry's name; on macOS and Linux an
/// item in the default keychain or collection whose service is that name (keyring takes a
/// target there as a keychain or collection name, so none is given). Lines' own: service
/// `lines`, account `<source>.<key>`.
fn entry_spec(
    own: Option<&'static str>,
    source: &str,
    account: String,
) -> (Option<String>, String, String) {
    match own {
        Some(entry) if cfg!(windows) => (Some(entry.to_owned()), entry.to_owned(), source.into()),
        Some(entry) => (None, entry.to_owned(), source.into()),
        None => (None, SERVICE.into(), account),
    }
}

fn os_entry(source: &str, key: &str) -> Result<keyring::Entry, String> {
    let (target, service, user) = entry_spec(own_entry(source, key), source, account(source, key));
    match target {
        Some(t) => keyring::Entry::new_with_target(&t, &service, &user),
        None => keyring::Entry::new(&service, &user),
    }
    .map_err(|e| e.to_string())
}

/// A source's saved secret (`Ok(None)`: none saved). May block briefly: never call it while
/// drawing.
pub fn secret(source: &str, key: &str) -> Result<Option<String>, String> {
    match &*backend().read().unwrap_or_else(|e| e.into_inner()) {
        Backend::Memory(m) => Ok(m
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&account(source, key))
            .cloned()),
        Backend::Os => match os_entry(source, key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        },
    }
}

/// Save a source's secret (replacing any).
pub fn set_secret(source: &str, key: &str, value: &str) -> Result<(), String> {
    match &*backend().read().unwrap_or_else(|e| e.into_inner()) {
        Backend::Memory(m) => {
            m.lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(account(source, key), value.to_owned());
            Ok(())
        }
        Backend::Os => os_entry(source, key)?
            .set_password(value)
            .map_err(|e| e.to_string()),
    }
}

/// Remove a source's secret (nothing saved is not an error).
pub fn remove_secret(source: &str, key: &str) -> Result<(), String> {
    match &*backend().read().unwrap_or_else(|e| e.into_inner()) {
        Backend::Memory(m) => {
            m.lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&account(source, key));
            Ok(())
        }
        Backend::Os => match os_entry(source, key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        },
    }
}

/// Sources' plain settings as saved (`settings.json` `connections`): source → field → value.
pub type Plain = BTreeMap<String, BTreeMap<String, String>>;

fn plain() -> &'static RwLock<Plain> {
    static P: OnceLock<RwLock<Plain>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// What feeds read for plain settings: the saved ones, kept in step with Settings.
pub fn sync_plain(saved: &Plain) {
    *plain().write().unwrap_or_else(|e| e.into_inner()) = saved.clone();
}

/// A plain setting's value: saved, else a choice's first value.
pub fn plain_value(saved: &Plain, source: &str, field: &Field) -> Option<String> {
    saved
        .get(source)
        .and_then(|m| m.get(field.key))
        .cloned()
        .or_else(|| match field.kind {
            FieldKind::Choice(c) => c.first().map(|v| (*v).to_owned()),
            _ => None,
        })
}

/// The app's store for sources' settings (installed at startup).
struct AppStore;

impl SettingsStore for AppStore {
    fn get(&self, source: &str, field: &Field) -> Option<String> {
        match field.kind {
            // On a thread of its own: connectors read settings in `connect`, on a feed's
            // Tokio runtime, and a keychain backend may run (and block on) a runtime of its own
            // (the Secret Service on Linux), which can't start inside another.
            FieldKind::Secret => match std::thread::scope(|sc| {
                sc.spawn(|| secret(source, field.key))
                    .join()
                    .unwrap_or_else(|_| Err("the keychain read panicked".to_owned()))
            }) {
                Ok(v) => v,
                Err(e) => {
                    // The keychain's message, never the secret.
                    tracing::warn!("keychain: {source}.{}: {e}", field.key);
                    None
                }
            },
            _ => plain_value(
                &plain().read().unwrap_or_else(|e| e.into_inner()),
                source,
                field,
            ),
        }
    }
}

/// Start serving sources' settings to their connectors (the app, at startup).
pub fn install(saved: &Plain) {
    sync_plain(saved);
    extension::set_settings_store(Arc::new(AppStore));
}

fn not_started() -> &'static RwLock<HashMap<String, &'static str>> {
    static N: OnceLock<RwLock<HashMap<String, &'static str>>> = OnceLock::new();
    N.get_or_init(Default::default)
}

/// A source on but not started because a required setting had no value, with that
/// setting's label (decided once at startup).
pub fn mark_not_set_up(source: &str, needs: &'static str) {
    not_started()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(source.to_owned(), needs);
}

/// Why a source on in Settings isn't in this run: the required setting it lacked.
pub fn not_set_up(source: &str) -> Option<&'static str> {
    not_started()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(source)
        .copied()
}

/// The first required setting of a registered source with no value (its environment
/// variable counts). Reads the keychain: call it off the UI thread.
pub fn needs(source: &str, fields: &[Field]) -> Option<&'static str> {
    fields
        .iter()
        .find(|f| f.required && extension::field_setting(source, f).is_none())
        .map(|f| f.label)
}

/// An example source for screenshots of the page (`--example-connection`): an API key and a
/// choice, never connected. Public code names no real connector (decision 0020).
pub struct Example;

impl extension::SourceExtension for Example {
    fn api_version(&self) -> u32 {
        extension::API_VERSION
    }
    fn name(&self) -> &'static str {
        "example"
    }
    fn help(&self) -> &'static str {
        "An example source (screenshots of Settings › Connections)."
    }
    fn provider(&self) -> Arc<dyn lines_feed::provider::Provider> {
        Arc::new(lines_feed::coinbase::CoinbaseProvider)
    }
    fn default_product(&self) -> lines_core::ProductId {
        lines_feed::catalog::BTC_USD
    }
    fn connect(
        &self,
        _: &lines_core::Product,
        _: Option<lines_core::ContractKey>,
    ) -> extension::BoxFuture<'static, anyhow::Result<Box<dyn extension::DynTransport>>> {
        Box::pin(async { anyhow::bail!("the example source never connects") })
    }
    fn fields(&self) -> Vec<Field> {
        vec![
            Field {
                key: "api-key",
                label: "API key",
                kind: FieldKind::Secret,
                required: true,
                env: Some("LINES_EXAMPLE_KEY"),
                keychain: None,
            },
            Field {
                key: "dataset",
                label: "Dataset",
                kind: FieldKind::Choice(&["Futures", "Equities"]),
                required: false,
                env: None,
                keychain: None,
            },
        ]
    }
    fn plan(&self) -> Option<String> {
        Some("Example only: real-time futures with depth".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A login refused this run reads as refused, with the provider's reason, after its panes
    /// moved away (no feed left), and before another feed's failure (one symbol's).
    #[test]
    fn a_refused_login_stays_refused_after_its_panes_moved() {
        let why = "logon rejected (code 13): permission denied";
        let feeds = with_refusal(Vec::new(), Some(why.into()));
        assert_eq!(state(true, true, None, &feeds), State::Refused(why.into()));
        assert_eq!(
            with_refusal(vec![FeedStatus::Live], None),
            vec![FeedStatus::Live]
        );
        let symbol = vec![FeedStatus::Failed("symbol not found".into())];
        assert_eq!(
            state(true, true, None, &with_refusal(symbol, Some(why.into()))),
            State::Refused(why.into())
        );
    }

    #[test]
    fn a_sources_state_reads_from_its_switch_setup_and_feeds() {
        let live = FeedStatus::Live;
        let busy = FeedStatus::Connecting;
        let refused = FeedStatus::Failed("key not entitled to GLBX".into());
        assert_eq!(
            state(false, false, None, &[]),
            State::Off { running: false }
        );
        assert_eq!(
            state(false, true, None, std::slice::from_ref(&live)),
            State::Off { running: true }
        );
        assert_eq!(
            state(true, true, Some("API key"), &[]),
            State::NotSetUp {
                needs: "API key",
                started: true
            }
        );
        assert_eq!(
            state(true, false, Some("API key"), &[]).text(),
            "Not set up: needs API key, so not started"
        );
        // A source that is running says what it is doing, set up or not.
        assert_eq!(
            state(true, true, Some("API key"), std::slice::from_ref(&live)),
            State::Connected { feeds: 1 }
        );
        assert_eq!(state(true, false, None, &[]), State::NextStart);
        assert_eq!(state(true, true, None, &[]), State::Idle);
        assert_eq!(
            state(true, true, None, std::slice::from_ref(&busy)),
            State::Connecting
        );
        // A dropped feed shows with its reason, even beside a live one.
        let retry = FeedStatus::Reconnecting {
            attempt: 2,
            reason: "401 unauthorized".into(),
        };
        assert_eq!(
            state(true, true, None, &[live.clone(), retry]),
            State::Retrying("401 unauthorized".into())
        );
        assert_eq!(
            state(true, true, None, &[FeedStatus::Stopped]),
            State::Stopped
        );
        assert_eq!(
            state(
                true,
                true,
                None,
                &[busy.clone(), live.clone(), live.clone()]
            ),
            State::Connected { feeds: 2 }
        );
        // A refusal is never hidden behind another feed's connection.
        let s = state(true, true, None, &[live, refused]);
        assert_eq!(s, State::Refused("key not entitled to GLBX".into()));
        assert_eq!(s.text(), "Refused: key not entitled to GLBX");
        assert_eq!(s.tone(), Tone::Bad);
        assert_eq!(State::Idle.tone(), Tone::Quiet);
        assert_eq!(State::Connected { feeds: 1 }.text(), "Connected: 1 symbol");
    }

    /// A connector's own keychain entry is where its own reader looks: the target on
    /// Windows, the service (default keychain or collection, no target) elsewhere.
    #[test]
    fn a_connectors_own_entry_is_where_its_reader_looks() {
        // Databento's entry and account, as its README has users store the key.
        let entry = lines_databento::key::CREDENTIAL;
        let name = lines_databento::live::DatabentoExtension::from_env();
        let name = lines_feed::extension::SourceExtension::name(&name);
        let (target, service, user) = entry_spec(Some(entry), name, "x".into());
        assert_eq!(service, entry);
        assert_eq!(
            user, "databento",
            "the account the README gives: one item, replaced"
        );
        if cfg!(windows) {
            assert_eq!(target.as_deref(), Some(entry));
        } else {
            assert_eq!(
                target, None,
                "a target names a keychain or collection there"
            );
        }
        assert_eq!(
            entry_spec(None, "acme", "acme.api-key".into()),
            (None, "lines".to_owned(), "acme.api-key".to_owned())
        );
    }

    struct Venue {
        name: &'static str,
        opt_in: bool,
    }

    impl lines_trading::extension::AccountSource for Venue {
        fn accounts(&self) -> Vec<lines_trading::extension::ExtensionAccount> {
            Vec::new()
        }
        fn revision(&self) -> u64 {
            0
        }
        fn status(&self) -> Option<lines_trading::extension::VenueStatus> {
            Some(lines_trading::extension::VenueStatus {
                connected: false,
                detail: "disconnected: connection refused".into(),
                last_message: None,
                warning: None,
            })
        }
    }

    impl BrokerExtension for Venue {
        fn name(&self) -> &str {
            self.name
        }
        fn label(&self) -> &str {
            "Local venue"
        }
        fn capabilities(&self) -> lines_trading::BrokerCaps {
            lines_trading::BrokerCaps {
                market_orders: false,
                stop_orders: false,
                modify: false,
                server_brackets: false,
                oco: false,
                fractional_qty: false,
                external_fills: true,
            }
        }
        fn accounts(&self) -> Arc<dyn lines_trading::extension::AccountSource> {
            Arc::new(Venue {
                name: self.name,
                opt_in: self.opt_in,
            })
        }
        fn open(&self) -> anyhow::Result<Box<dyn lines_trading::Broker>> {
            anyhow::bail!("never opened here")
        }
        fn plan(&self) -> Option<String> {
            Some("Observe only: order entry disabled".into())
        }
        fn opt_in(&self) -> bool {
            self.opt_in
        }
    }

    /// An opt-in broker is off (never opened) until switched on, and switching it off again
    /// saves nothing; any other broker is on unless switched off.
    #[test]
    fn an_opt_in_broker_is_off_until_switched_on() {
        let opt: Arc<dyn BrokerExtension> = Arc::new(Venue {
            name: "venue-a",
            opt_in: true,
        });
        let plain: Arc<dyn BrokerExtension> = Arc::new(Venue {
            name: "venue-b",
            opt_in: false,
        });
        let all = [opt.clone(), plain.clone()];
        let names = |v: Vec<Arc<dyn BrokerExtension>>| {
            v.iter().map(|b| b.name().to_owned()).collect::<Vec<_>>()
        };
        let mut prefs = BTreeMap::new();
        assert_eq!(names(brokers_on(&all, &prefs)), ["venue-b"]);
        switch(&mut prefs, "venue-a".into(), true, true);
        assert_eq!(prefs["venue-a"].enabled, Some(true));
        assert_eq!(names(brokers_on(&all, &prefs)), ["venue-a", "venue-b"]);
        switch(&mut prefs, "venue-b".into(), false, false);
        assert_eq!(names(brokers_on(&all, &prefs)), ["venue-a"]);
        switch(&mut prefs, "venue-a".into(), false, true);
        switch(&mut prefs, "venue-b".into(), true, false);
        assert!(prefs.is_empty(), "the default states save nothing");
        // A source switched off still saves as before.
        switch(&mut prefs, "coinbase".into(), false, false);
        assert_eq!(prefs["coinbase"].enabled, Some(false));
    }

    /// A broker's card: off, on at the next start, then its venue's own words (no feeds).
    #[test]
    fn a_brokers_state_is_its_switch_then_its_venue() {
        let opt: Arc<dyn BrokerExtension> = Arc::new(Venue {
            name: "venue-c",
            opt_in: true,
        });
        set_brokers(std::slice::from_ref(&opt), &[]);
        assert!(broker_opt_in("venue-c"));
        assert!(!broker_opt_in("coinbase"));
        assert_eq!(
            broker_state("venue-c", false),
            State::Off { running: false }
        );
        assert_eq!(broker_state("venue-c", true), State::NextStart);
        let row = rows().into_iter().find(|r| r.name == "venue-c").unwrap();
        assert_eq!(row.plan, "Observe only: order entry disabled");
        assert_eq!(
            row.broker.as_ref().map(|b| b.label.as_str()),
            Some("Local venue")
        );
        // After the sources; only the calendar's card follows the brokers.
        let all = rows();
        assert_eq!(
            all.iter().rev().nth(1).map(|r| r.name.as_str()),
            Some("venue-c"),
            "after the sources"
        );
        assert!(all.last().is_some_and(|r| r.calendar && r.name == "fred"));
        set_brokers(std::slice::from_ref(&opt), std::slice::from_ref(&opt));
        let s = broker_state("venue-c", true);
        assert_eq!(s.text(), "Disconnected: connection refused");
        assert_eq!(s.tone(), Tone::Busy);
        assert_eq!(broker_state("venue-c", false), State::Off { running: true });
        set_brokers(&[], &[]);
    }

    #[test]
    fn a_secret_shows_only_its_last_4_characters() {
        assert_eq!(masked("db-AbCdEfGh1234"), "••••1234");
        // A short secret shows nothing of itself.
        assert_eq!(masked("abc"), "••••");
        assert_eq!(masked("12345678"), "••••");
        assert_eq!(masked(""), "••••");
    }

    #[test]
    fn secrets_are_saved_replaced_and_removed_and_plain_values_default() {
        use_memory_keychain(&[("acme", "api-key", "first-value-0001")]);
        assert_eq!(
            secret("acme", "api-key").unwrap().as_deref(),
            Some("first-value-0001")
        );
        set_secret("acme", "api-key", "second-value-0002").unwrap();
        assert_eq!(
            secret("acme", "api-key").unwrap().as_deref(),
            Some("second-value-0002")
        );
        remove_secret("acme", "api-key").unwrap();
        assert_eq!(secret("acme", "api-key").unwrap(), None);
        remove_secret("acme", "api-key").unwrap();
        // Plain values: saved, else a choice's first value.
        let dataset = Field {
            key: "dataset",
            label: "Dataset",
            kind: FieldKind::Choice(&["GLBX.MDP3", "XNAS.ITCH"]),
            required: false,
            env: None,
            keychain: None,
        };
        let mut saved = Plain::new();
        assert_eq!(
            plain_value(&saved, "acme", &dataset).as_deref(),
            Some("GLBX.MDP3")
        );
        saved
            .entry("acme".into())
            .or_default()
            .insert("dataset".into(), "XNAS.ITCH".into());
        assert_eq!(
            plain_value(&saved, "acme", &dataset).as_deref(),
            Some("XNAS.ITCH")
        );
        // Coinbase and Kraken are always listed, after any extension.
        // (Brokers, which another test registers, come after them.)
        let names: Vec<String> = rows()
            .into_iter()
            .filter(|r| r.broker.is_none() && !r.calendar)
            .map(|r| r.name)
            .collect();
        assert!(names.ends_with(&["coinbase".to_owned(), "kraken".to_owned()]));
    }
}
