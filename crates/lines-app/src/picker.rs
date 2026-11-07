//! Symbol picker logic (roadmap 4b): fuzzy search, ranking, contract choices, micro/mini
//! swapping and the recents/favourites settings file. Pure and GPUI-free so it is unit-tested;
//! the only I/O is the explicit [`Settings::load`]; saving goes through the app's writer.
//!
//! Ranking: recents first (most recent first), then favourites, then everything else; within
//! favourites and the rest, best fuzzy score first, then catalog order. Products are picked;
//! contracts are derived (decision 0013): row offset 0 follows the front month, Tab steps
//! through the next listed months as fixed contracts.

use std::path::Path;

use lines_core::calendar::{calendar_expiry, calendar_roll_date};
use lines_core::{CivilDate, ContractKey, FixedStr, Product, ProductId};
use lines_feed::catalog::{self, Target};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

use crate::versioned::{self, Format};

/// Recents kept in the settings file.
pub const MAX_RECENTS: usize = 8;
/// Contract months Tab offers after the front month.
pub const LATER_MONTHS: usize = 3;
/// A front month this close to its roll date is flagged.
pub const ROLL_SOON_DAYS: i64 = 5;

// ---- catalog entries ------------------------------------------------------------------

/// One searchable product.
#[derive(Clone, Debug)]
pub struct Entry {
    pub product: Product,
    pub exchange: &'static str,
    /// "SYMBOL description exchange", what the fuzzy matcher scores.
    haystack: String,
}

pub fn entries(products: impl IntoIterator<Item = Product>) -> Vec<Entry> {
    products
        .into_iter()
        .map(|product| {
            let exchange = catalog::exchange(product.id);
            let haystack = format!("{} {} {exchange}", product.symbol, product.description);
            Entry {
                product,
                exchange,
                haystack,
            }
        })
        .collect()
}

/// The other size of the same index: ES ↔ MES, NQ ↔ MNQ (from the products' micro
/// equivalence data). `None` for products without one.
pub fn size_partner(entries: &[Entry], id: ProductId) -> Option<ProductId> {
    let me = entries.iter().find(|e| e.product.id == id)?;
    match me.product.micro {
        // A mini: its micro.
        Some(m) if m.micro != id => Some(m.micro),
        // A micro: the mini that names it.
        Some(_) => entries
            .iter()
            .find(|e| e.product.id != id && e.product.micro.is_some_and(|m| m.micro == id))
            .map(|e| e.product.id),
        None => None,
    }
}

// ---- settings ---------------------------------------------------------------------------

/// Recents and favourites, persisted as JSON in the OS config dir. Product ids are permanent
/// (catalog), so the file survives symbol renames.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub recents: Vec<u32>,
    #[serde(default)]
    pub favourites: Vec<u32>,
    /// Studies for new charts ("Save as default for new charts"; decision 0021: app-wide
    /// scope). Optional: absent means new charts start empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart_defaults: Option<crate::study_menu::StudySet>,
    /// Named study templates, in save order. Optional.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub study_templates: Vec<crate::study_menu::StudyTemplate>,
    /// The trader's guardrails per trading account, keyed by the account's name (or id)
    /// (roadmap 8b). Optional; no editor yet (the Settings window, decision 0021, will edit
    /// it). Read at start: tighter limits apply at once, looser ones from the next trade date.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub guardrails: std::collections::BTreeMap<String, lines_trading::Guardrails>,
    /// How P&L shows in the trading UI, every pane and account: off, ticks, currency or both
    /// (`crate::pnl`). Optional: absent (and the default, both) isn't written.
    #[serde(default, skip_serializing_if = "crate::pnl::PnlDisplay::is_default")]
    pub pnl_display: crate::pnl::PnlDisplay,
    /// Bracket presets and their display unit per account and product (`crate::brackets`;
    /// trade panel v2). Optional: absent means the default presets, in ticks.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bracket_presets: crate::brackets::BracketSettings,
    /// What turns DOM trading off besides the trader (all off by default: the switch is
    /// sticky). Optional.
    #[serde(
        default,
        skip_serializing_if = "crate::dom_trade::ArmPrefs::is_default"
    )]
    pub trading_prefs: crate::dom_trade::ArmPrefs,
    /// Theme, palette and hollow candles (the Settings window; decision 0021). Optional:
    /// absent means the built-in defaults; a CLI flag wins for its run.
    #[serde(
        default,
        skip_serializing_if = "crate::settings_schema::Appearance::is_default"
    )]
    pub appearance: crate::settings_schema::Appearance,
    /// Defaults for panes created from now on (the Settings window). Optional.
    #[serde(
        default,
        skip_serializing_if = "crate::settings_schema::NewPanes::is_default"
    )]
    pub new_panes: crate::settings_schema::NewPanes,
    /// Drawing styles for new drawings per tool ("Save as default for ..." in the style
    /// editor; decision 0026), keyed by the tool's file name (`trend`, `hline`, ...).
    /// Optional: absent means the default style. Entries keep fields a newer build wrote
    /// (and tools this build doesn't have).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub drawing_styles: std::collections::BTreeMap<String, crate::style_doc::SavedStyle>,
    /// Prop rules per trading account, keyed by the account's name (the Settings window's
    /// Risk section). Optional: an account without an entry keeps its rules.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prop_rules: std::collections::BTreeMap<String, crate::settings_schema::PropRulesDoc>,
    /// Keys this version doesn't know (a newer version's), kept as they were so saving
    /// never drops them (decision 0021).
    #[serde(flatten)]
    pub unknown: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Files written before versioning (no `version` field) have exactly version 1's fields.
fn settings_v0_to_v1(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

/// `settings.json`'s format (`crate::versioned`).
pub const SETTINGS_FORMAT: Format = Format {
    what: "settings",
    current: 1,
    migrations: &[settings_v0_to_v1],
};

impl Settings {
    /// Missing file: defaults, no report. A file that can't be used is backed up and reported
    /// (`crate::versioned::load`); the last value says whether saving over `path` is safe.
    pub fn load(path: &Path) -> (Settings, Option<String>, bool) {
        let l = versioned::load::<Settings>(&SETTINGS_FORMAT, path);
        let mut s = l.value.unwrap_or_default();
        s.normalise();
        (s, l.report, l.writable)
    }

    /// The file's bytes (versioned JSON).
    pub fn encode(&self) -> Vec<u8> {
        versioned::encode(&SETTINGS_FORMAT, self).unwrap_or_default()
    }

    /// Write synchronously (tests); the app saves through `crate::persist::Writer`.
    #[cfg(test)]
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        versioned::save(path, &self.encode())
    }

    fn normalise(&mut self) {
        // The format's own key is written by `versioned`, not kept as an unknown one.
        self.unknown.remove("version");
        dedup_keep_first(&mut self.recents);
        self.recents.truncate(MAX_RECENTS);
        dedup_keep_first(&mut self.favourites);
    }

    pub fn is_favourite(&self, id: ProductId) -> bool {
        self.favourites.contains(&id.0)
    }

    fn recent_rank(&self, id: ProductId) -> Option<usize> {
        self.recents.iter().position(|&r| r == id.0)
    }

    /// Most recent first; capped at [`MAX_RECENTS`].
    pub fn push_recent(&mut self, id: ProductId) {
        self.recents.retain(|&r| r != id.0);
        self.recents.insert(0, id.0);
        self.recents.truncate(MAX_RECENTS);
    }

    /// Returns whether `id` is now a favourite.
    pub fn toggle_favourite(&mut self, id: ProductId) -> bool {
        if self.is_favourite(id) {
            self.favourites.retain(|&f| f != id.0);
            false
        } else {
            self.favourites.push(id.0);
            true
        }
    }
}

fn dedup_keep_first(v: &mut Vec<u32>) {
    let mut seen = Vec::with_capacity(v.len());
    v.retain(|x| {
        let new = !seen.contains(x);
        seen.push(*x);
        new
    });
}

// ---- search & ranking -------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Recent,
    Favourite,
    Other,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Index into the entries slice.
    pub index: usize,
    pub group: Group,
    pub score: u32,
}

/// Reusable matcher state (nucleo keeps scratch buffers between calls).
pub struct Searcher {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Default for Searcher {
    fn default() -> Self {
        Searcher {
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
        }
    }
}

const EXACT_SYMBOL_BONUS: u32 = 1 << 20;
const SYMBOL_PREFIX_BONUS: u32 = 1 << 16;

impl Searcher {
    /// Fill `out` with the entries matching `query`, ranked. An empty query matches all.
    pub fn search(
        &mut self,
        entries: &[Entry],
        query: &str,
        settings: &Settings,
        out: &mut Vec<Hit>,
    ) {
        out.clear();
        let query = query.trim();
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        for (index, e) in entries.iter().enumerate() {
            let score = if query.is_empty() {
                0
            } else {
                let Some(s) =
                    pattern.score(Utf32Str::new(&e.haystack, &mut self.buf), &mut self.matcher)
                else {
                    continue;
                };
                let sym = e.product.symbol;
                s + if sym.eq_ignore_ascii_case(query) {
                    EXACT_SYMBOL_BONUS
                } else if sym.len() >= query.len()
                    && sym.is_char_boundary(query.len())
                    && sym[..query.len()].eq_ignore_ascii_case(query)
                {
                    SYMBOL_PREFIX_BONUS
                } else {
                    0
                }
            };
            let id = e.product.id;
            let group = if settings.recent_rank(id).is_some() {
                Group::Recent
            } else if settings.is_favourite(id) {
                Group::Favourite
            } else {
                Group::Other
            };
            out.push(Hit {
                index,
                group,
                score,
            });
        }
        out.sort_by(|a, b| {
            a.group.cmp(&b.group).then_with(|| match a.group {
                Group::Recent => {
                    let r = |h: &Hit| settings.recent_rank(entries[h.index].product.id);
                    r(a).cmp(&r(b))
                }
                _ => b.score.cmp(&a.score).then(a.index.cmp(&b.index)),
            })
        });
    }
}

// ---- contracts --------------------------------------------------------------------------

/// The front month (calendar rule: no live volume before connecting, decision 0013's
/// fallback) followed by the next [`LATER_MONTHS`] listed contracts. Spot: its one contract.
pub fn contract_choices(product: &Product, today: CivilDate) -> Vec<ContractKey> {
    let Some(front) = lines_core::roll::resolve_front(product, today, None, None, &[]) else {
        return Vec::new();
    };
    let mut out = vec![front.contract];
    if !product.is_spot() {
        let mut k = front.contract;
        for _ in 0..LATER_MONTHS {
            match lines_core::roll::next_contract(product, k) {
                Some(n) => {
                    out.push(n);
                    k = n;
                }
                None => break,
            }
        }
    }
    out
}

/// What a result row shows for one contract choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractRow {
    /// Offset 0 = the product (follows the front month); others = that fixed contract.
    pub target: Target,
    pub contract: ContractKey,
    pub label: FixedStr<24>,
    pub is_front: bool,
    pub expiry: Option<CivilDate>,
    /// Front month only: days until the calendar says the next contract takes over.
    pub days_to_roll: Option<i64>,
    pub roll_soon: bool,
}

/// Row for choice `offset` (wrapped into range). `None` if the product has no contract.
pub fn contract_row(product: &Product, today: CivilDate, offset: usize) -> Option<ContractRow> {
    let choices = contract_choices(product, today);
    let i = offset % choices.len().max(1);
    let contract = *choices.get(i)?;
    let is_front = i == 0;
    let days_to_roll = is_front
        .then(|| calendar_roll_date(product, contract))
        .flatten()
        .map(|d| d.days() - today.days());
    Some(ContractRow {
        target: if is_front {
            Target::Product(product.id)
        } else {
            Target::Contract(contract)
        },
        contract,
        label: contract.label(product),
        is_front,
        expiry: calendar_expiry(product, contract),
        days_to_roll,
        roll_soon: days_to_roll.is_some_and(|d| d <= ROLL_SOON_DAYS),
    })
}

// ---- picker state -----------------------------------------------------------------------

/// The open picker: query, ranked hits, selection and the selected row's contract choice.
pub struct Picker {
    pub query: String,
    pub hits: Vec<Hit>,
    pub selected: usize,
    /// Contract choice of the selected row (0 = front month, following rolls).
    pub offset: usize,
    searcher: Searcher,
}

impl Picker {
    pub fn open(query: &str, entries: &[Entry], settings: &Settings) -> Picker {
        let mut p = Picker {
            query: query.to_owned(),
            hits: Vec::with_capacity(entries.len()),
            selected: 0,
            offset: 0,
            searcher: Searcher::default(),
        };
        p.research(entries, settings, None);
        p
    }

    /// Re-run the search. Keeps the selection on `keep` if it is still a hit, else the top.
    fn research(&mut self, entries: &[Entry], settings: &Settings, keep: Option<ProductId>) {
        self.searcher
            .search(entries, &self.query, settings, &mut self.hits);
        let found = keep.and_then(|id| {
            self.hits
                .iter()
                .position(|h| entries[h.index].product.id == id)
        });
        match found {
            Some(i) => self.selected = i,
            None => {
                self.selected = 0;
                self.offset = 0;
            }
        }
    }

    pub fn selected_entry<'a>(&self, entries: &'a [Entry]) -> Option<&'a Entry> {
        self.hits.get(self.selected).map(|h| &entries[h.index])
    }

    pub fn push_str(&mut self, s: &str, entries: &[Entry], settings: &Settings) {
        self.query.push_str(s);
        self.research(entries, settings, None);
    }

    pub fn backspace(&mut self, entries: &[Entry], settings: &Settings) {
        if self.query.pop().is_some() {
            self.research(entries, settings, None);
        }
    }

    /// Move the selection, wrapping at either end. Resets the contract choice.
    pub fn move_selection(&mut self, delta: isize) {
        let n = self.hits.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        self.offset = 0;
    }

    /// Tab / Shift+Tab: next or previous contract month of the selected row (wraps).
    pub fn cycle_contract(&mut self, delta: isize, entries: &[Entry], today: CivilDate) {
        let Some(e) = self.selected_entry(entries) else {
            return;
        };
        let n = contract_choices(&e.product, today).len().max(1) as isize;
        self.offset = (self.offset as isize + delta).rem_euclid(n) as usize;
    }

    /// Swap the selection to the other size (ES ↔ MES), keeping the contract month. If the
    /// partner doesn't match the query, the query becomes its symbol.
    pub fn toggle_size(&mut self, entries: &[Entry], settings: &Settings) {
        let Some(id) = self.selected_entry(entries).map(|e| e.product.id) else {
            return;
        };
        let Some(partner) = size_partner(entries, id) else {
            return;
        };
        let offset = self.offset;
        if !self
            .hits
            .iter()
            .any(|h| entries[h.index].product.id == partner)
        {
            self.query = entries
                .iter()
                .find(|e| e.product.id == partner)
                .map_or_else(String::new, |e| e.product.symbol.to_owned());
        }
        self.research(entries, settings, Some(partner));
        self.offset = offset;
    }

    /// Re-rank after settings changed (e.g. a favourite toggled), keeping the selection.
    pub fn refresh(&mut self, entries: &[Entry], settings: &Settings) {
        let keep = self.selected_entry(entries).map(|e| e.product.id);
        let offset = self.offset;
        self.research(entries, settings, keep);
        self.offset = offset;
    }

    /// The selected row's contract choice.
    pub fn selected_row(&self, entries: &[Entry], today: CivilDate) -> Option<ContractRow> {
        let e = self.selected_entry(entries)?;
        contract_row(&e.product, today, self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::Month;
    use lines_feed::catalog::{BTC_USD, ES, ETH_USD, MES, MNQ, NQ, ZN};

    fn all() -> Vec<Entry> {
        entries(catalog::all())
    }

    fn symbols(entries: &[Entry], query: &str, settings: &Settings) -> Vec<&'static str> {
        let mut out = Vec::new();
        Searcher::default().search(entries, query, settings, &mut out);
        out.iter()
            .map(|h| entries[h.index].product.symbol)
            .collect()
    }

    fn day(y: u16, m: u8, d: u8) -> CivilDate {
        CivilDate::new(y, m, d).unwrap()
    }

    #[test]
    fn fuzzy_search_by_symbol_description_and_exchange() {
        let e = all();
        let none = Settings::default();
        let nq = symbols(&e, "nq", &none);
        assert_eq!(
            &nq[..2],
            ["NQ", "MNQ"],
            "exact symbol, then symbol containing it"
        );
        let nas = symbols(&e, "nas", &none);
        assert!(nas.contains(&"NQ") && nas.contains(&"MNQ"), "{nas:?}");
        assert!(!nas.contains(&"ES"));
        assert_eq!(
            symbols(&e, "100", &none),
            ["NQ", "MNQ", "QQQ"],
            "Nasdaq-100 (the futures, then the ETF)"
        );
        assert_eq!(symbols(&e, "mnq", &none)[0], "MNQ");
        assert_eq!(symbols(&e, "MNQ", &none)[0], "MNQ", "case-insensitive");
        assert_eq!(
            symbols(&e, "cbot", &none)[0],
            "ZN",
            "exchange; fuzzy may add weaker hits"
        );
        let coinbase = symbols(&e, "coinbase", &none);
        assert_eq!(coinbase, ["BTC/USD", "ETH/USD"]);
        assert!(symbols(&e, "zzzz", &none).is_empty());
        assert_eq!(
            symbols(&e, "", &none).len(),
            e.len(),
            "empty query lists everything"
        );
        assert_eq!(symbols(&e, "  ", &none).len(), e.len());
    }

    #[test]
    fn recents_then_favourites_then_the_rest() {
        let e = all();
        let mut s = Settings::default();
        s.push_recent(ZN);
        s.push_recent(ES); // most recent
        s.toggle_favourite(BTC_USD);
        s.toggle_favourite(ES); // recent wins over favourite
        let ranked = symbols(&e, "", &s);
        assert_eq!(&ranked[..4], ["ES", "ZN", "BTC/USD", "NQ"]);
        // With a query, the groups still lead, even over better-scoring matches.
        let mut s = Settings::default();
        s.push_recent(MNQ);
        assert_eq!(symbols(&e, "nq", &s)[..2], ["MNQ", "NQ"]);
        s = Settings::default();
        s.toggle_favourite(MNQ);
        assert_eq!(symbols(&e, "nq", &s)[..2], ["MNQ", "NQ"]);
        // Recents that don't match the query are not shown.
        let mut s = Settings::default();
        s.push_recent(ES);
        assert!(!symbols(&e, "nq", &s).contains(&"ES"));
    }

    #[test]
    fn settings_recents_and_favourites() {
        let mut s = Settings::default();
        for id in [ES, NQ, ZN, MES, MNQ, BTC_USD, ETH_USD, ES, NQ, ZN] {
            s.push_recent(id);
        }
        assert_eq!(s.recents.len(), 7, "deduplicated");
        assert_eq!(&s.recents[..3], [ZN.0, NQ.0, ES.0], "most recent first");
        for i in 0..20 {
            s.push_recent(ProductId(100 + i));
        }
        assert_eq!(s.recents.len(), MAX_RECENTS);
        assert!(s.toggle_favourite(ES));
        assert!(s.is_favourite(ES));
        assert!(!s.toggle_favourite(ES));
        assert!(!s.is_favourite(ES));
    }

    #[test]
    fn pnl_display_is_an_optional_setting() {
        use crate::pnl::PnlDisplay;
        // A file from before the setting: the default (both), no version bump needed.
        let old: Settings = serde_json::from_str(r#"{"recents":[1],"favourites":[]}"#).unwrap();
        assert_eq!(old.pnl_display, PnlDisplay::Both);
        // The default isn't written; any other mode is, and reads back.
        let json = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!json.contains("pnl_display"), "{json}");
        let off = Settings {
            pnl_display: PnlDisplay::Off,
            ..Settings::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains(r#""pnl_display":"off""#), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pnl_display, PnlDisplay::Off);
        assert_eq!(SETTINGS_FORMAT.current, 1, "no version bump");
    }

    #[test]
    fn settings_file_roundtrip_missing_corrupt_and_unversioned() {
        let dir = std::env::temp_dir().join(format!("lines-picker-test-{}", std::process::id()));
        let path = dir.join("nested").join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            Settings::load(&path),
            (Settings::default(), None, true),
            "missing = defaults"
        );

        let mut s = Settings::default();
        s.push_recent(NQ);
        s.toggle_favourite(ZN);
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), (s.clone(), None, true));
        assert!(
            String::from_utf8(s.encode())
                .unwrap()
                .contains(r#""version": 1"#)
        );

        // Corrupt: defaults, reported, and the file kept aside (never silently discarded).
        std::fs::write(&path, b"{ not json").unwrap();
        let (loaded, report, writable) = Settings::load(&path);
        assert_eq!(loaded, Settings::default());
        assert!(writable);
        assert!(report.unwrap().contains("couldn't be read"));
        let kept: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("settings.unreadable-"))
            .collect();
        assert_eq!(kept.len(), 1);

        // A file from before versioning migrates (and is kept as settings.v0.json); wrong
        // shapes and duplicates are tolerated.
        std::fs::write(&path, br#"{"recents":[2,2,1],"future_field":true}"#).unwrap();
        let (loaded, report, _) = Settings::load(&path);
        assert_eq!(report, None);
        assert_eq!(loaded.recents, [2, 1]);
        assert!(loaded.favourites.is_empty());
        assert!(path.with_file_name("settings.v0.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn front_month_expiry_and_roll() {
        let es = catalog::get(ES).unwrap();
        let row = contract_row(&es, day(2026, 11, 6), 0).unwrap();
        assert_eq!(row.label.as_str(), "ESZ26");
        assert_eq!(
            row.target,
            Target::Product(ES),
            "offset 0 follows the front month"
        );
        assert!(row.is_front);
        assert_eq!(row.expiry, Some(day(2026, 12, 18)), "third Friday");
        assert_eq!(
            row.days_to_roll,
            Some(34),
            "rolls 8 days before expiry, Dec 10"
        );
        assert!(!row.roll_soon);
        let near = contract_row(&es, day(2026, 12, 7), 0).unwrap();
        assert_eq!((near.days_to_roll, near.roll_soon), (Some(3), true));
        let rolled = contract_row(&es, day(2026, 12, 10), 0).unwrap();
        assert_eq!(
            rolled.label.as_str(),
            "ESH27",
            "on the roll date the next month is front"
        );
    }

    #[test]
    fn tab_cycles_contract_months() {
        let es = catalog::get(ES).unwrap();
        let today = day(2026, 11, 6);
        let labels: Vec<_> = (0..=LATER_MONTHS + 1)
            .map(|o| {
                contract_row(&es, today, o)
                    .unwrap()
                    .label
                    .as_str()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            labels,
            ["ESZ26", "ESH27", "ESM27", "ESU27", "ESZ26"],
            "wraps"
        );
        let h27 = contract_row(&es, today, 1).unwrap();
        assert_eq!(
            h27.target,
            Target::Contract(ContractKey::monthly(ES, 2027, Month::new(3).unwrap()))
        );
        assert_eq!(
            h27.days_to_roll, None,
            "only the front month shows days to roll"
        );

        let e = all();
        let mut p = Picker::open("es", &e, &Settings::default());
        assert_eq!(p.selected_entry(&e).unwrap().product.symbol, "ES");
        p.cycle_contract(1, &e, today);
        p.cycle_contract(1, &e, today);
        assert_eq!(p.selected_row(&e, today).unwrap().label.as_str(), "ESM27");
        p.cycle_contract(-3, &e, today);
        assert_eq!(
            p.selected_row(&e, today).unwrap().label.as_str(),
            "ESU27",
            "Shift+Tab wraps"
        );
        p.move_selection(1);
        assert_eq!(p.offset, 0, "a new row starts at its front month");
    }

    #[test]
    fn spot_has_one_contract_and_no_roll() {
        let btc = catalog::get(BTC_USD).unwrap();
        let today = day(2026, 11, 6);
        assert_eq!(contract_choices(&btc, today).len(), 1);
        let row = contract_row(&btc, today, 3).unwrap();
        assert_eq!(row.target, Target::Product(BTC_USD));
        assert_eq!(
            (row.expiry, row.days_to_roll, row.roll_soon),
            (None, None, false)
        );
    }

    #[test]
    fn micro_mini_toggle() {
        let e = all();
        assert_eq!(size_partner(&e, ES), Some(MES));
        assert_eq!(size_partner(&e, MES), Some(ES));
        assert_eq!(size_partner(&e, NQ), Some(MNQ));
        assert_eq!(size_partner(&e, MNQ), Some(NQ));
        assert_eq!(size_partner(&e, ZN), None);
        assert_eq!(size_partner(&e, BTC_USD), None);

        let s = Settings::default();
        let today = day(2026, 11, 6);
        let mut p = Picker::open("nq", &e, &s);
        p.cycle_contract(1, &e, today);
        p.toggle_size(&e, &s);
        assert_eq!(p.selected_entry(&e).unwrap().product.symbol, "MNQ");
        assert_eq!(
            p.selected_row(&e, today).unwrap().label.as_str(),
            "MNQH27",
            "month kept"
        );
        p.toggle_size(&e, &s);
        assert_eq!(p.selected_entry(&e).unwrap().product.symbol, "NQ");
        // Partner outside the results: the query follows it.
        let mut p = Picker::open("cbot", &e, &s);
        p.toggle_size(&e, &s); // ZN has no partner: nothing happens
        assert_eq!(p.query, "cbot");
    }

    #[test]
    fn typing_and_favourites_keep_a_sensible_selection() {
        let e = all();
        let mut s = Settings::default();
        let mut p = Picker::open("", &e, &s);
        p.push_str("m", &e, &s);
        p.push_str("n", &e, &s);
        assert_eq!(p.query, "mn");
        p.backspace(&e, &s);
        p.backspace(&e, &s);
        p.backspace(&e, &s); // already empty
        assert_eq!(p.hits.len(), e.len());
        p.move_selection(-1);
        assert_eq!(p.selected, e.len() - 1, "Up from the top wraps");
        let id = p.selected_entry(&e).unwrap().product.id;
        s.toggle_favourite(id);
        p.refresh(&e, &s);
        assert_eq!(p.selected, 0, "favourite moved to the top");
        assert_eq!(
            p.selected_entry(&e).unwrap().product.id,
            id,
            "selection follows it"
        );
    }
}
