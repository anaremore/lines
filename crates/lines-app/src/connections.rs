//! Settings › Connections (decision 0030 §3–4), GPUI-free: the sources the page lists, the
//! state each is in, the settings a source describes, and where they are kept: plain values
//! in `settings.json` (`connections`), secrets in the OS keychain (never in the settings
//! file, workspaces, logs or diagnostics). The page itself is `connections_view`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use lines_feed::extension::{self, Field, FieldKind, SettingsStore};
use lines_feed::store::FeedStatus;

/// One source on the page.
#[derive(Clone, Debug)]
pub struct Row {
    /// Its `--source` name (`coinbase`, an extension's).
    pub name: String,
    pub fields: Vec<Field>,
    /// What its plan allows, in a few words.
    pub plan: String,
}

/// The public sources, in the order a run without `--source` prefers them.
const BUILTIN: [&str; 2] = ["coinbase", "kraken"];

/// Every source the page lists: each registered extension, then Coinbase and Kraken (the
/// developer sources, mock and replay, stay behind their flags).
pub fn rows() -> Vec<Row> {
    let mut out: Vec<Row> = extension::registered()
        .iter()
        .map(|e| Row {
            name: e.name().to_owned(),
            fields: e.fields(),
            plan: e
                .plan()
                .unwrap_or_else(|| "Not reported by the connector".to_owned()),
        })
        .collect();
    out.extend(BUILTIN.iter().map(|n| Row {
        name: (*n).to_owned(),
        fields: Vec::new(),
        plan: "Public data, no account: trades, top of book and depth".to_owned(),
    }));
    out
}

/// A source's state, as the page states it (a word, never color alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Switched off (`running`: still serving panes until Lines restarts).
    Off {
        running: bool,
    },
    /// A required setting has no value.
    NotSetUp {
        needs: &'static str,
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
            State::NotSetUp { needs } => format!("Not set up: needs {needs}, so not started"),
            State::NextStart => "On when Lines restarts".to_owned(),
            State::Idle => "Idle: connects when a pane shows its symbols".to_owned(),
            State::Connecting => "Connecting…".to_owned(),
            State::Connected { feeds: 1 } => "Connected: 1 symbol".to_owned(),
            State::Connected { feeds } => format!("Connected: {feeds} symbols"),
            State::Refused(why) => format!("Refused: {why}"),
            State::Retrying(why) => format!("Reconnecting: {why}"),
            State::Stopped => "Stopped".to_owned(),
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            State::Off { .. } | State::NextStart | State::Idle | State::Stopped => Tone::Quiet,
            State::Connecting => Tone::Busy,
            State::Connected { .. } => Tone::Good,
            State::NotSetUp { .. } | State::Refused(_) | State::Retrying(_) => Tone::Bad,
        }
    }
}

/// A source's state from its switch, whether this run has it, the first required setting
/// with no value, and the state of each of its running feeds.
pub fn state(on: bool, in_run: bool, needs: Option<&'static str>, feeds: &[FeedStatus]) -> State {
    if !on {
        return State::Off { running: in_run };
    }
    if let Some(needs) = needs {
        return State::NotSetUp { needs };
    }
    if !in_run {
        return State::NextStart;
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

fn account(source: &str, key: &str) -> String {
    format!("{source}.{key}")
}

fn os_entry(source: &str, key: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, &account(source, key)).map_err(|e| e.to_string())
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

/// The first required setting of a registered source with no value (its environment
/// variable counts). Reads the keychain: call it off the UI thread.
pub fn needs(source: &str, fields: &[Field]) -> Option<&'static str> {
    fields
        .iter()
        .find(|f| f.required && extension::setting(source, f.key).is_none())
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
            },
            Field {
                key: "dataset",
                label: "Dataset",
                kind: FieldKind::Choice(&["Futures", "Equities"]),
                required: false,
                env: None,
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
            State::NotSetUp { needs: "API key" }
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
        let names: Vec<String> = rows().into_iter().map(|r| r.name).collect();
        assert!(names.ends_with(&["coinbase".to_owned(), "kraken".to_owned()]));
    }
}
