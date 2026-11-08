//! The workspace as a document: what is saved, restored and named (decision 0016).
//!
//! Workspace → windows → tabs → a [`Layout`] of [`PaneDoc`]s. A pane records its kind, link
//! group, target (a product followed to its front month, or one contract; `None` = the
//! source's default) and its view state. The market data *source* is not part of a workspace:
//! it belongs to the run (`--source`), so one workspace works on mock, replay or live data.
//!
//! File format: JSON, versioned through [`crate::versioned`] (`FORMAT`, currently version 6).
//! Version 2 adds each pane's `state` (shown, hidden, hidden by "chart only", detached) and a
//! detached pane's `window` placement; version 1 files migrate (every pane shown). Version 3
//! adds the `journal` pane kind (roadmap 8c); version 2 files read unchanged, and an older
//! build meeting a version 3 file reports it as newer instead of failing on the kind. Version 4
//! adds the `strategies` pane kind. Version 5 (decision 0028) lets a pane's contract be an
//! equity or a listed option; every version 4 file reads unchanged, and an older build
//! reports a version 5 file as newer instead of misreading an option. Version 6 adds the `chain`
//! pane kind (an underlying's option chain).
//! Product ids are the catalog's permanent ids; `symbol` is written beside them for people
//! reading the file and ignored on load. A pane whose product this build doesn't know is kept
//! as written and shown as unavailable, never dropped.
//!
//! Also here, pure and tested: where a restored window goes ([`restore_placement`]) and what
//! dropping a dragged tab does ([`tab_drop`]).

use std::collections::BTreeMap;

use lines_core::{ContractKey, Delivery, Month, ProductId, Timeframe};
use lines_feed::catalog::{self, Target};
use serde::{Deserialize, Serialize};

use crate::layout::{Axis, Layout, LinkGroup, PaneKind, Preset, Rect, trading_group};
use crate::pane_columns::{ColumnDoc, DomCol, JournalCol, TapeCol};
use crate::profile::ProfileConfig;
use crate::tabstate::PaneState;
use crate::versioned::{Format, Migration};

/// Version 0 never existed for workspaces (the first file format is 1); an unversioned file
/// is not one of ours and is set aside as unreadable, not guessed at.
fn v0_never_written(_: serde_json::Value) -> Result<serde_json::Value, String> {
    Err("no version field (not a Lines workspace file)".into())
}

/// Version 2 added pane states. Every version 1 pane was shown: say so explicitly.
fn v1_to_v2(mut v: serde_json::Value) -> Result<serde_json::Value, String> {
    fn panes(node: &mut serde_json::Value) -> Result<(), String> {
        let obj = node.as_object_mut().ok_or("layout node isn't an object")?;
        if let Some(p) = obj.get_mut("pane") {
            let p = p.as_object_mut().ok_or("pane isn't an object")?;
            p.entry("state").or_insert_with(|| "shown".into());
            return Ok(());
        }
        let split = obj
            .get_mut("split")
            .and_then(|s| s.as_object_mut())
            .ok_or("layout node is neither a pane nor a split")?;
        for side in ["first", "second"] {
            panes(split.get_mut(side).ok_or("split without both sides")?)?;
        }
        Ok(())
    }
    let windows = v
        .get_mut("windows")
        .and_then(|w| w.as_array_mut())
        .ok_or("no windows")?;
    for w in windows {
        let tabs = w
            .get_mut("tabs")
            .and_then(|t| t.as_array_mut())
            .ok_or("window without tabs")?;
        for t in tabs {
            panes(t.get_mut("layout").ok_or("tab without a layout")?)?;
        }
    }
    Ok(v)
}

/// Version 3 added the `journal` pane kind: every version 2 file is a valid version 3 file.
fn v2_to_v3(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

/// Version 4 added the `strategies` pane kind: every version 3 file is a valid version 4 file
/// (an older build reports a file with a Strategies pane as newer instead of failing on it).
fn v3_to_v4(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

/// Version 5 added equity and option contracts (decision 0028): every version 4 file is a valid
/// version 5 file, its futures and spot contracts read back as the same keys.
fn v4_to_v5(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

/// Version 6 added the `chain` pane kind (decision 0028): every version 5 file is a valid
/// version 6 file (an older build reports a file with an option chain as newer).
fn v5_to_v6(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

const MIGRATIONS: &[Migration] = &[
    v0_never_written,
    v1_to_v2,
    v2_to_v3,
    v3_to_v4,
    v4_to_v5,
    v5_to_v6,
];

pub const FORMAT: Format = Format {
    what: "workspace",
    current: 6,
    migrations: MIGRATIONS,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDoc {
    #[serde(default = "default_name")]
    pub name: String,
    /// Charts mark the time under the pointer of the other charts in their link group
    /// (opt-in; unlinked charts sync with each other).
    #[serde(default)]
    pub crosshair_sync: bool,
    /// With crosshair sync: charts of the same product also mark its price. Optional:
    /// written only when on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub crosshair_sync_price: bool,
    /// The key-hint bar at the bottom of each window is hidden (shown by default, for new
    /// users; written only when hidden).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_hints: bool,
    /// Chart headers show performance stats (msgs/s, updates/s, paints/s, paint time). Off
    /// by default; written only when on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_perf_stats: bool,
    /// Chart headers show the latency HUD (roadmap 6b). Off by default; written only when on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_latency_hud: bool,
    pub windows: Vec<WindowDoc>,
}

fn default_name() -> String {
    "default".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowDoc {
    /// Where the window was; `None` = let the app choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(default)]
    pub active_tab: usize,
    pub tabs: Vec<TabDoc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabDoc {
    /// The focused pane, as an index in the layout's leaf order.
    #[serde(default)]
    pub focused: usize,
    pub layout: Layout<PaneDoc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneDoc {
    pub kind: PaneKind,
    #[serde(default)]
    pub link: LinkGroup,
    /// `None`: the source's default product (or a recording's contract).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dom: Option<DomState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tape: Option<TapeState>,
    /// A journal pane's view (version 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<JournalState>,
    /// Shown, hidden, hidden by its chart's "chart only", or detached (version 2).
    #[serde(default)]
    pub state: PaneState,
    /// A detached pane's window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Placement>,
    /// The pane's trading choices (account, quantity, bracket; roadmap 7). Optional: files
    /// without it trade on the default account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade: Option<crate::trade_bar::TradeDoc>,
}

impl PaneDoc {
    pub fn new(kind: PaneKind, link: LinkGroup, target: Option<TargetDoc>) -> PaneDoc {
        PaneDoc {
            kind,
            link,
            target,
            chart: None,
            dom: None,
            tape: None,
            journal: None,
            state: PaneState::Shown,
            window: None,
            trade: None,
        }
    }
}

/// A product (followed to its front month) or one contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetDoc {
    /// Catalog product id (permanent).
    pub product: u32,
    /// For people reading the file; ignored on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// A fixed contract; absent = follow the front month.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ContractDoc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractDoc {
    Monthly {
        year: u16,
        month: u8,
    },
    Spot,
    /// An equity's shares (version 5).
    Equity,
    /// A listed option (version 5): expiry `YYYYMMDD`, the exact strike in the underlying's
    /// price units, the right.
    Option {
        expiry: u32,
        strike: i64,
        right: lines_trading::types::RightDoc,
    },
}

impl TargetDoc {
    pub fn from_target(t: &Target) -> TargetDoc {
        let (product, contract) = match t {
            Target::Product(p) => (*p, None),
            Target::Contract(k) => (
                k.product,
                Some(match k.delivery {
                    Delivery::Monthly { year, month } => ContractDoc::Monthly {
                        year,
                        month: month.number(),
                    },
                    Delivery::Spot => ContractDoc::Spot,
                    Delivery::Equity => ContractDoc::Equity,
                    // One saved shape for options across files (lines-trading's).
                    Delivery::Option(_) => {
                        let o = lines_trading::types::ContractDoc::from_key(*k)
                            .option
                            .expect("an option key saves an option");
                        ContractDoc::Option {
                            expiry: o.expiry,
                            strike: o.strike,
                            right: o.right,
                        }
                    }
                }),
            ),
        };
        TargetDoc {
            product: product.0,
            symbol: catalog::get(product).map(|p| p.symbol.to_owned()),
            contract,
        }
    }

    /// The target, or why this build can't use it (shown on the pane; the doc is kept).
    pub fn to_target(&self) -> Result<Target, String> {
        let id = ProductId(self.product);
        let product = catalog::get(id).ok_or_else(|| match &self.symbol {
            Some(s) => format!("unknown product {s} (id {})", self.product),
            None => format!("unknown product id {}", self.product),
        })?;
        Ok(match self.contract {
            None => Target::Product(id),
            Some(ContractDoc::Spot) if product.is_spot() => Target::Contract(ContractKey::spot(id)),
            Some(ContractDoc::Monthly { year, month })
                if !product.is_spot() && !product.is_equity() =>
            {
                let m = Month::new(month).ok_or_else(|| format!("bad contract month {month}"))?;
                Target::Contract(ContractKey::monthly(id, year, m))
            }
            Some(ContractDoc::Equity) if product.is_equity() => {
                Target::Contract(ContractKey::equity(id))
            }
            Some(ContractDoc::Option {
                expiry,
                strike,
                right,
            }) => {
                let doc = lines_trading::types::ContractDoc {
                    product: self.product,
                    year: None,
                    month: None,
                    equity: false,
                    option: Some(lines_trading::types::OptionDoc {
                        expiry,
                        strike,
                        right,
                    }),
                };
                match doc.to_key() {
                    Some(k) if product.lists(&k) => Target::Contract(k),
                    _ => return Err(format!("bad option contract for {}", product.symbol)),
                }
            }
            Some(_) => return Err(format!("bad contract for {}", product.symbol)),
        })
    }

    pub fn product(id: ProductId) -> TargetDoc {
        TargetDoc::from_target(&Target::Product(id))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartState {
    /// Time zoom: logical px per bar.
    pub bar_px: f64,
    /// The chart's indicators (roadmap 5c). Optional: files without it load with none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indicators: Vec<lines_indicators::IndicatorConfig>,
    /// Volume profile settings. Optional in the file: absent (older files, or a chart that
    /// never changed them) means the defaults, so the field is written only when it differs.
    #[serde(default, skip_serializing_if = "ProfileConfig::is_default")]
    pub profile: ProfileConfig,
    /// Study labels and studies hidden (kept in the file as flat `hide_*` keys, written only
    /// when set).
    #[serde(flatten)]
    pub visibility: StudyVisibility,
    /// External levels on this chart (decision 0022): hidden sources, kinds and labels.
    /// Optional: absent means every source and kind shown.
    #[serde(
        default,
        skip_serializing_if = "crate::levels_layer::LevelsConfig::is_default"
    )]
    pub levels: crate::levels_layer::LevelsConfig,
    /// Magnet crosshair: the price snaps to the hovered bar's open/high/low/close. Optional:
    /// written only when on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub magnet: bool,
    /// The order strip (chart trading phase 2b) shown on this chart. Optional: written only
    /// when on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub order_strip: bool,
    /// The chart's timeframe (roadmap 9), e.g. `"5m"`, `"1h"`, `"1D"`. Optional: absent means
    /// 1 minute; a value this version doesn't offer also loads as 1 minute.
    #[serde(default, skip_serializing_if = "TimeframeDoc::is_default")]
    pub timeframe: TimeframeDoc,
    /// The footprint chart mode (roadmap 10e). Optional: absent means off.
    #[serde(
        default,
        skip_serializing_if = "crate::footprint::FootprintConfig::is_default"
    )]
    pub footprint: crate::footprint::FootprintConfig,
    /// User styles of the profile, the footprint and level sources (decision 0026; VWAPs and
    /// cumulative delta keep theirs in `indicators`). Optional: absent means today's look.
    #[serde(
        default,
        skip_serializing_if = "crate::study_style::StudyStyles::is_empty"
    )]
    pub styles: crate::study_style::StudyStyles,
    /// The sub-pane's share of the chart's height (`crate::subpane`), when it was moved from
    /// the default. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpane: Option<f32>,
    /// The product's drawings hidden on this chart (roadmap 11; the drawings themselves live
    /// in `drawings/<PRODUCT>.json`, never here). Optional: written only when set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_drawings: bool,
    /// Tick bars run across sessions (`true`) or restart at each session open (`false`)
    /// (roadmap 9b). Optional: written for tick charts and charts set to continuous; absent,
    /// the chart keeps the mode it started with (`--tick-continuous`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_continuous: Option<bool>,
}

/// A chart timeframe in the workspace file, as its label (`"5m"`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimeframeDoc(pub Timeframe);

impl Default for TimeframeDoc {
    fn default() -> Self {
        TimeframeDoc(Timeframe::ONE_MINUTE)
    }
}

impl TimeframeDoc {
    pub fn is_default(&self) -> bool {
        self.0.is_one_minute()
    }
}

impl Serialize for TimeframeDoc {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TimeframeDoc {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Timeframe::parse(&s).map_or_else(TimeframeDoc::default, TimeframeDoc))
    }
}

/// What a chart draws of its studies (VWAP lines and bands, the volume profile). Hiding
/// keeps every study's configuration: showing again restores them exactly.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudyVisibility {
    /// No legends or study tags (the studies stay drawn).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_study_labels: bool,
    /// No studies at all (and no work for them in paint).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_studies: bool,
}

impl StudyVisibility {
    pub fn draws_studies(self) -> bool {
        !self.hide_studies
    }

    pub fn draws_labels(self) -> bool {
        !self.hide_studies && !self.hide_study_labels
    }

    /// Whether a window's footer (key hints + workspace status) is shown: always, unless the
    /// key hints are hidden *and* the status has nothing to say beyond the workspace name
    /// (hidden or popped-out panes, crosshair sync and reports always show; docs/design.md
    /// §3 and §7: state is never hidden by a cosmetic preference).
    pub fn footer_shown(hide_hints: bool, status_has_state: bool) -> bool {
        !hide_hints || status_has_state
    }

    /// The `CleanChart` command: a chart without legends or study tags and the key-hint bar
    /// hidden. Run on a chart that is already clean, it restores both. Studies stay drawn
    /// (and hidden studies stay hidden): clean is about text, not data.
    pub fn clean_chart(self, hints_hidden: bool) -> (StudyVisibility, bool) {
        let clean = self.hide_study_labels && hints_hidden;
        (
            StudyVisibility {
                hide_study_labels: !clean,
                ..self
            },
            !clean,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DomState {
    /// Ticks per row.
    pub grouping: u32,
    /// Column widths and visibility that differ from the defaults (optional: files without
    /// it load with default columns).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<DomCol, ColumnDoc>,
    /// The price trail (optional: written only when on or at a non-default half-life).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail: Option<crate::dom_trail::TrailDoc>,
}

/// A journal pane's view (roadmap 8c).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JournalState {
    /// Stats over every trade date (else the current one).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_dates: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<JournalCol, ColumnDoc>,
    /// What the table lists (phase 2): trades, or P&L by account or by tag. Optional; a
    /// value this build doesn't know reads as the trade list.
    #[serde(default, skip_serializing_if = "JournalGroup::is_trades")]
    pub group: JournalGroup,
}

/// What a journal pane lists.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "serde_json::Value", into = "String")]
pub enum JournalGroup {
    /// Every closed trade.
    #[default]
    Trades,
    /// P&L per account.
    Accounts,
    /// P&L per tag (setup).
    Tags,
}

impl JournalGroup {
    pub const ALL: [JournalGroup; 3] = [
        JournalGroup::Trades,
        JournalGroup::Accounts,
        JournalGroup::Tags,
    ];

    pub fn is_trades(&self) -> bool {
        *self == JournalGroup::Trades
    }

    /// The word on its tab in the pane's header.
    pub fn label(self) -> &'static str {
        match self {
            JournalGroup::Trades => "Trades",
            JournalGroup::Accounts => "By account",
            JournalGroup::Tags => "By tag",
        }
    }
}

/// Anything else (a newer build's value, `null`, a number) is the trade list.
impl From<serde_json::Value> for JournalGroup {
    fn from(v: serde_json::Value) -> JournalGroup {
        match v.as_str() {
            Some("accounts") => JournalGroup::Accounts,
            Some("tags") => JournalGroup::Tags,
            _ => JournalGroup::Trades,
        }
    }
}

impl From<JournalGroup> for String {
    fn from(g: JournalGroup) -> String {
        match g {
            JournalGroup::Trades => "trades",
            JournalGroup::Accounts => "accounts",
            JournalGroup::Tags => "tags",
        }
        .to_owned()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TapeState {
    /// Large-print threshold in the product's quantity units (`Qty`), if set by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large_print: Option<i64>,
    /// Column widths and visibility that differ from the defaults (optional).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<TapeCol, ColumnDoc>,
}

/// A window's place on screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// The monitor's persistent id (as the OS reports it), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default)]
    pub state: WindowState,
    /// Restore bounds in screen coordinates (logical px).
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// Names for saved workspaces: they become file names, so letters, digits, space, `-`, `_`,
/// 1..=40 characters, not a preset's name.
pub fn valid_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("name is empty".into());
    }
    if n.chars().count() > 40 {
        return Err("name is longer than 40 characters".into());
    }
    if let Some(c) = n
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_')))
    {
        return Err(format!(
            "{c:?} isn't allowed (letters, digits, space, - and _)"
        ));
    }
    if is_builtin(n) {
        return Err(format!("{n:?} is a built-in layout"));
    }
    Ok(n.to_owned())
}

/// Built-in workspaces: the presets plus `pair` (the default) and `6c2d` (the benchmark).
pub fn is_builtin(name: &str) -> bool {
    builtin(name).is_some()
}

pub fn builtin(name: &str) -> Option<WorkspaceDoc> {
    if name.eq_ignore_ascii_case("pair") {
        return Some(WorkspaceDoc::pair());
    }
    if name.eq_ignore_ascii_case("6c2d") {
        return Some(WorkspaceDoc::bench());
    }
    Preset::from_name(name)
        .map(|p| WorkspaceDoc::single(name, p.build(|k, l| PaneDoc::new(k, l, None))))
}

impl WorkspaceDoc {
    /// One window, one tab.
    pub fn single(name: &str, layout: Layout<PaneDoc>) -> WorkspaceDoc {
        WorkspaceDoc {
            name: name.to_owned(),
            crosshair_sync: false,
            crosshair_sync_price: false,
            hide_hints: false,
            show_perf_stats: false,
            show_latency_hud: false,
            windows: vec![WindowDoc {
                placement: None,
                active_tab: 0,
                tabs: vec![TabDoc { focused: 0, layout }],
            }],
        }
    }

    /// The first-run default (the owner's everyday setup): NQ and ES side by side, each as
    /// chart + DOM + time & sales in its own link group (NQ = A, ES = B). Crosshair sync is
    /// on, so the two charts compare bar for bar.
    pub fn pair() -> WorkspaceDoc {
        let side = |id: ProductId, link: u8| {
            trading_group(Some(link), &mut |k, l| {
                PaneDoc::new(k, l, Some(TargetDoc::product(id)))
            })
        };
        let mut w = WorkspaceDoc::single(
            "default",
            Layout::split(Axis::Row, 0.5, side(catalog::NQ, 0), side(catalog::ES, 1)),
        );
        w.crosshair_sync = true;
        w
    }

    /// The tracked benchmark (`docs/positioning.md`): 6 charts + 2 DOMs in one tab, on the
    /// source's default product. Two rows of three charts over a strip of two DOMs would be
    /// cramped, so: three columns of two charts, then a column of two DOMs.
    pub fn bench() -> WorkspaceDoc {
        let chart = |l| Layout::Pane(PaneDoc::new(PaneKind::Chart, Some(l), None));
        let dom = |l| Layout::Pane(PaneDoc::new(PaneKind::Dom, Some(l), None));
        let col = |a, b| Layout::split(Axis::Column, 0.5, a, b);
        let charts = Layout::split(
            Axis::Row,
            0.333,
            col(chart(0), chart(1)),
            Layout::split(
                Axis::Row,
                0.5,
                col(chart(2), chart(3)),
                col(chart(0), chart(1)),
            ),
        );
        let mut w = WorkspaceDoc::single(
            "6c2d",
            Layout::split(Axis::Row, 0.78, charts, col(dom(0), dom(1))),
        );
        w.windows[0].tabs[0].focused = 0;
        w
    }

    /// Repair what a hand-edited or older file may get wrong without dropping anything the
    /// user made: indexes clamped, ratios clamped. `Err` if there is nothing to show.
    pub fn normalise(&mut self) -> Result<(), String> {
        // A window without tabs holds no user data beyond its position.
        self.windows.retain(|w| !w.tabs.is_empty());
        if self.windows.is_empty() {
            return Err("the workspace has no windows with tabs".into());
        }
        for w in &mut self.windows {
            w.active_tab = w.active_tab.min(w.tabs.len() - 1);
            for t in &mut w.tabs {
                t.layout.normalise();
                t.focused = t.focused.min(t.layout.leaf_count() - 1);
                let mut fix = |p: &PaneDoc| {
                    let mut p = p.clone();
                    if p.link.is_some_and(|g| g >= crate::layout::LINK_GROUPS) {
                        p.link = None;
                    }
                    // Charts are never hidden (only closed or popped out).
                    if p.kind == PaneKind::Chart && p.state.is_hidden() {
                        p.state = PaneState::Shown;
                    }
                    p
                };
                t.layout = t.layout.map(&mut fix);
                // Something must be on screen: show the first hidden pane, else the first.
                if !t.layout.any(&|p: &PaneDoc| p.state.is_shown()) {
                    let first_hidden = t.layout.leaves().iter().position(|p| p.state.is_hidden());
                    let target = first_hidden.unwrap_or(0);
                    let mut i = 0;
                    t.layout = t.layout.map(&mut |p: &PaneDoc| {
                        let mut p = p.clone();
                        if i == target {
                            p.state = PaneState::Shown;
                            p.window = None;
                        }
                        i += 1;
                        p
                    });
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn pane_count(&self) -> usize {
        self.windows
            .iter()
            .flat_map(|w| &w.tabs)
            .map(|t| t.layout.leaf_count())
            .sum()
    }

    /// Every pane's target replaced (e.g. `--instrument`).
    pub fn retarget_all(&mut self, target: Option<TargetDoc>) {
        for w in &mut self.windows {
            for t in &mut w.tabs {
                t.layout = t.layout.map(&mut |p: &PaneDoc| PaneDoc {
                    target: target.clone(),
                    ..p.clone()
                });
            }
        }
    }
}

// ---- window placement ---------------------------------------------------------------------

/// A monitor as the platform reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayInfo {
    pub uuid: Option<String>,
    pub bounds: Rect,
}

/// The least of a window that must be on some display for it to count as reachable.
const MIN_VISIBLE: (f32, f32) = (120.0, 40.0);

fn overlap(a: Rect, b: Rect) -> (f32, f32) {
    (
        (a.right().min(b.right()) - a.x.max(b.x)).max(0.0),
        (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0.0),
    )
}

/// Where to open a saved window: on its own monitor if that is still connected and the window
/// is still reachable there; else wherever it is reachable; else centred on the first display
/// (the primary), shrunk to fit. Returns the display index and the bounds.
pub fn restore_placement(p: &Placement, displays: &[DisplayInfo]) -> (Option<usize>, Rect) {
    let want = Rect::new(p.x, p.y, p.w.max(200.0), p.h.max(150.0));
    let reachable = |d: &DisplayInfo| {
        let (w, h) = overlap(want, d.bounds);
        w >= MIN_VISIBLE.0.min(want.w) && h >= MIN_VISIBLE.1.min(want.h)
    };
    if let Some(i) = displays
        .iter()
        .position(|d| d.uuid.is_some() && d.uuid == p.display && reachable(d))
    {
        return (Some(i), want);
    }
    if let Some(i) = displays.iter().position(reachable) {
        return (Some(i), want);
    }
    let Some(d) = displays.first() else {
        return (None, want);
    };
    let b = d.bounds;
    let (w, h) = (want.w.min(b.w), want.h.min(b.h));
    (
        Some(0),
        Rect::new(b.x + (b.w - w) * 0.5, b.y + (b.h - h) * 0.5, w, h),
    )
}

// ---- tab drag and drop --------------------------------------------------------------------

/// What releasing a dragged tab does.
#[derive(Clone, Debug, PartialEq)]
pub enum TabDrop<W> {
    /// Released over its own tab bar: move it to this index.
    Reorder(usize),
    /// Released over another Lines window: move the tab there.
    ToWindow(W),
    /// Released away from the tab bar and every Lines window: a new window, its top-left here
    /// (screen coordinates).
    NewWindow { x: f32, y: f32 },
    /// Released inside its own window, near the tab bar: nothing happens.
    Cancel,
}

/// How far below the tab bar (inside its own window) a tab must be dragged to tear off.
pub const TEAR_OFF_PX: f32 = 48.0;

/// Decide a tab drop. `pointer` is in screen coordinates; `own` is the source window's
/// screen bounds and `tab_bar` its tab bar (screen coordinates); `tab_slots` the tab bar's
/// tab rectangles' right edges (screen x) in order, for the reorder index; `others` every
/// other Lines window.
pub fn tab_drop<W: Clone>(
    pointer: (f32, f32),
    own: Rect,
    tab_bar: Rect,
    tab_rights: &[f32],
    others: &[(W, Rect)],
) -> TabDrop<W> {
    let (x, y) = pointer;
    if tab_bar.contains(x, y) {
        let i = tab_rights
            .iter()
            .position(|&r| x < r)
            .unwrap_or(tab_rights.len().saturating_sub(1));
        return TabDrop::Reorder(i);
    }
    if own.contains(x, y) {
        // Over its own window: tear off only when clearly dragged away from the tab bar.
        return if y > tab_bar.bottom() + TEAR_OFF_PX {
            TabDrop::NewWindow { x, y }
        } else {
            TabDrop::Cancel
        };
    }
    if let Some((w, _)) = others.iter().find(|(_, r)| r.contains(x, y)) {
        return TabDrop::ToWindow(w.clone());
    }
    TabDrop::NewWindow { x, y }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::versioned::{Decoded, decode, encode};

    /// Chart behaviours: the magnet crosshair (per chart) and price sync (per workspace) are
    /// optional fields, written only when on (no version bump).
    #[test]
    fn magnet_and_price_sync_are_optional() {
        let old: ChartState = serde_json::from_str(r#"{"bar_px": 7.0}"#).unwrap();
        assert!(!old.magnet);
        assert!(!serde_json::to_string(&old).unwrap().contains("magnet"));
        let on = ChartState {
            magnet: true,
            ..old.clone()
        };
        let json = serde_json::to_string(&on).unwrap();
        assert!(json.contains(r#""magnet":true"#), "{json}");
        assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), on);
        assert!(!old.order_strip && !json.contains("order_strip"), "{json}");
        let strip = ChartState {
            order_strip: true,
            ..old.clone()
        };
        let json = serde_json::to_string(&strip).unwrap();
        assert!(json.contains(r#""order_strip":true"#), "{json}");
        assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), strip);

        let mut w = builtin("pair").unwrap();
        assert!(!w.crosshair_sync_price);
        let text = serde_json::to_string(&w).unwrap();
        assert!(!text.contains("crosshair_sync_price"), "{text}");
        w.crosshair_sync_price = true;
        let back: WorkspaceDoc = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert!(back.crosshair_sync_price);
    }

    /// Roadmap 9: a chart's timeframe is optional in the file (absent = 1 minute), saved as its
    /// label, and an unknown value loads as 1 minute rather than failing the file.
    fn json_has(state: &ChartState, key: &str) -> bool {
        serde_json::to_string(state).unwrap().contains(key)
    }

    #[test]
    fn chart_timeframe_is_optional_and_round_trips() {
        let old: ChartState = serde_json::from_str(r#"{"bar_px": 7.0}"#).unwrap();
        assert_eq!(old.timeframe.0, Timeframe::ONE_MINUTE);
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("timeframe"), "{json}");
        for tf in Timeframe::ALL {
            let state = ChartState {
                timeframe: TimeframeDoc(tf),
                ..old.clone()
            };
            let json = serde_json::to_string(&state).unwrap();
            if !tf.is_one_minute() {
                assert!(json.contains(&format!(r#""timeframe":"{tf}""#)), "{json}");
            }
            assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), state);
        }
        // Drawings hidden on a chart (roadmap 11): optional, written only when set.
        assert!(!json_has(&old, "hide_drawings"));
        let hidden = ChartState {
            hide_drawings: true,
            ..old.clone()
        };
        let json = serde_json::to_string(&hidden).unwrap();
        assert!(json.contains(r#""hide_drawings":true"#), "{json}");
        assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), hidden);
        let future: ChartState =
            serde_json::from_str(r#"{"bar_px": 7.0, "timeframe": "3x"}"#).unwrap();
        assert_eq!(future.timeframe.0, Timeframe::ONE_MINUTE);
    }

    /// Roadmap 9b: a tick chart saves `"500t"` in the same optional field (no version bump).
    /// A build from before tick bars reads it through its own parser, which doesn't know `t`:
    /// the "absent or unknown = 1 minute" path, so an older build opens the chart on 1m.
    #[test]
    fn tick_timeframes_round_trip_and_fall_back_to_1m_in_older_builds() {
        /// `Timeframe::parse` as it was before tick bars (minutes, hours, D, W only).
        fn parse_before_ticks(s: &str) -> Option<Timeframe> {
            let s = s.trim().to_ascii_lowercase();
            let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
            let (num, unit) = s.split_at(digits);
            let n: u16 = if num.is_empty() { 1 } else { num.parse().ok()? };
            let tf = match unit {
                "" | "m" | "min" if !num.is_empty() => Timeframe::Minutes(n),
                "h" | "hr" => Timeframe::Hours(n),
                "d" if n == 1 => Timeframe::Day,
                "w" if n == 1 => Timeframe::Week,
                _ => return None,
            };
            (Timeframe::ALL.contains(&tf) && tf.bar_kind().is_none()).then_some(tf)
        }
        let old: ChartState = serde_json::from_str(r#"{"bar_px": 7.0}"#).unwrap();
        assert!(old.tick_continuous.is_none() && !json_has(&old, "tick_continuous"));
        // Roadmap 9b's other trade-built kinds go the same way (`"500v"`, `"10r"`, `"30s"`).
        for tf in [
            Timeframe::Ticks(500),
            Timeframe::Ticks(233),
            Timeframe::Ticks(777),
            Timeframe::Volume(500),
            Timeframe::Range(10),
            Timeframe::Seconds(30),
        ] {
            let state = ChartState {
                timeframe: TimeframeDoc(tf),
                tick_continuous: Some(tf == Timeframe::Ticks(777)),
                ..old.clone()
            };
            let json = serde_json::to_string(&state).unwrap();
            assert!(json.contains(&format!(r#""timeframe":"{tf}""#)), "{json}");
            assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), state);
            // The older build: its parser, then its "unknown = 1 minute" rule.
            let label = tf.to_string();
            let before =
                parse_before_ticks(&label).map_or_else(TimeframeDoc::default, TimeframeDoc);
            assert_eq!(before.0, Timeframe::ONE_MINUTE, "{label} in an older build");
        }
        for (mode, text) in [(Some(true), "true"), (Some(false), "false")] {
            let json = serde_json::to_string(&ChartState {
                tick_continuous: mode,
                ..old.clone()
            })
            .unwrap();
            assert!(
                json.contains(&format!(r#""tick_continuous":{text}"#)),
                "{json}"
            );
            assert_eq!(
                serde_json::from_str::<ChartState>(&json)
                    .unwrap()
                    .tick_continuous,
                mode
            );
        }
        // Out of range in this build too: 1 minute, the file still loads.
        let bad: ChartState =
            serde_json::from_str(r#"{"bar_px": 7.0, "timeframe": "100001t"}"#).unwrap();
        assert_eq!(bad.timeframe.0, Timeframe::ONE_MINUTE);
    }

    /// Roadmap 5c: a chart's indicators are saved with it; files from before load with none.
    #[test]
    fn chart_indicators_are_optional_and_round_trip() {
        let old: ChartState = serde_json::from_str(r#"{"bar_px": 7.0}"#).unwrap();
        assert!(old.indicators.is_empty());
        let state = ChartState {
            timeframe: Default::default(),
            hide_drawings: false,
            tick_continuous: None,
            bar_px: 7.0,
            visibility: StudyVisibility::default(),
            levels: Default::default(),
            indicators: vec![lines_indicators::IndicatorConfig::Vwap {
                spec: lines_indicators::VwapSpec {
                    reset: lines_indicators::Reset::Rth,
                    bands: lines_indicators::Bands(0b011),
                },
                color: 1,
                display: Default::default(),
                style: Default::default(),
            }],
            profile: ProfileConfig::default(),
            magnet: false,
            order_strip: false,
            footprint: Default::default(),
            styles: Default::default(),
            subpane: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""indicators""#), "{json}");
        assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), state);
        // None saved: the field is left out, so the file stays as before.
        let none = serde_json::to_string(&ChartState {
            timeframe: Default::default(),
            hide_drawings: false,
            tick_continuous: None,
            bar_px: 7.0,
            visibility: StudyVisibility::default(),
            levels: Default::default(),
            indicators: vec![],
            profile: ProfileConfig::default(),
            magnet: false,
            order_strip: false,
            footprint: Default::default(),
            styles: Default::default(),
            subpane: None,
        })
        .unwrap();
        assert!(!none.contains("indicators"), "{none}");
    }

    #[test]
    fn default_is_nq_and_es_side_by_side_each_with_chart_dom_and_tape() {
        let w = WorkspaceDoc::pair();
        assert_eq!(w.windows.len(), 1);
        let tab = &w.windows[0].tabs[0];
        let leaves = tab.layout.leaves();
        let got: Vec<_> = leaves
            .iter()
            .map(|p| {
                (
                    p.kind,
                    p.link,
                    p.target.as_ref().unwrap().symbol.as_deref().unwrap(),
                )
            })
            .collect();
        use PaneKind::*;
        assert_eq!(
            got,
            vec![
                (Chart, Some(0), "NQ"),
                (Dom, Some(0), "NQ"),
                (Tape, Some(0), "NQ"),
                (Chart, Some(1), "ES"),
                (Dom, Some(1), "ES"),
                (Tape, Some(1), "ES"),
            ]
        );
        // Side by side: NQ's panes all left of ES's.
        let placed = tab.layout.place(Rect::new(0.0, 0.0, 1600.0, 900.0));
        assert!(placed[..3].iter().all(|p| p.rect.right() <= 800.0));
        assert!(placed[3..].iter().all(|p| p.rect.x >= 800.0));
        assert!(w.crosshair_sync);
    }

    #[test]
    fn bench_is_six_charts_and_two_doms() {
        let w = WorkspaceDoc::bench();
        let kinds: Vec<_> = w.windows[0].tabs[0]
            .layout
            .leaves()
            .iter()
            .map(|p| p.kind)
            .collect();
        assert_eq!(kinds.iter().filter(|k| **k == PaneKind::Chart).count(), 6);
        assert_eq!(kinds.iter().filter(|k| **k == PaneKind::Dom).count(), 2);
        assert_eq!(w.pane_count(), 8);
    }

    #[test]
    fn round_trips_through_the_versioned_format() {
        let mut w = WorkspaceDoc::pair();
        w.windows[0].placement = Some(Placement {
            display: Some("disp-1".into()),
            state: WindowState::Maximized,
            x: 10.0,
            y: 20.0,
            w: 1200.0,
            h: 800.0,
        });
        w.windows.push(WindowDoc {
            placement: None,
            active_tab: 1,
            tabs: vec![
                TabDoc {
                    focused: 0,
                    layout: Layout::Pane(PaneDoc {
                        chart: Some(ChartState {
                            timeframe: Default::default(),
                            hide_drawings: false,
                            tick_continuous: None,
                            bar_px: 9.5,
                            visibility: StudyVisibility::default(),
                            levels: Default::default(),
                            indicators: vec![],
                            profile: ProfileConfig::default(),
                            magnet: false,
                            order_strip: false,
                            footprint: Default::default(),
                            styles: Default::default(),
                            subpane: None,
                        }),
                        ..PaneDoc::new(PaneKind::Chart, None, None)
                    }),
                },
                TabDoc {
                    focused: 1,
                    layout: Layout::split(
                        Axis::Column,
                        0.3,
                        Layout::Pane(PaneDoc {
                            dom: Some(DomState {
                                grouping: 5,
                                columns: BTreeMap::from([(
                                    DomCol::Bid,
                                    ColumnDoc {
                                        width: Some(88.0),
                                        shown: None,
                                    },
                                )]),
                                trail: None,
                            }),
                            target: Some(TargetDoc::from_target(&Target::Contract(
                                ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap()),
                            ))),
                            ..PaneDoc::new(PaneKind::Dom, Some(3), None)
                        }),
                        Layout::Pane(PaneDoc {
                            tape: Some(TapeState {
                                large_print: Some(25),
                                columns: BTreeMap::from([(
                                    TapeCol::Side,
                                    ColumnDoc {
                                        width: None,
                                        shown: Some(false),
                                    },
                                )]),
                            }),
                            ..PaneDoc::new(PaneKind::Tape, Some(3), None)
                        }),
                    ),
                },
            ],
        });
        let bytes = encode(&FORMAT, &w).unwrap();
        assert_eq!(
            decode::<WorkspaceDoc>(&FORMAT, &bytes),
            Decoded::Ok {
                value: w,
                migrated_from: None
            }
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(r#""version": 6"#), "{text}");
        assert!(text.contains(r#""symbol": "NQ""#), "{text}");
    }

    #[test]
    fn study_visibility_and_hint_bar_round_trip_and_default_to_shown() {
        // Older files (no fields) show everything.
        let old: ChartState = serde_json::from_str(r#"{"bar_px": 7.0}"#).unwrap();
        assert_eq!(old.visibility, StudyVisibility::default());
        assert!(old.visibility.draws_studies() && old.visibility.draws_labels());
        // Set: flat keys, round-trip; unset: not written.
        let hidden = ChartState {
            timeframe: Default::default(),
            hide_drawings: false,
            tick_continuous: None,
            bar_px: 7.0,
            visibility: StudyVisibility {
                hide_study_labels: true,
                hide_studies: true,
            },
            indicators: vec![lines_indicators::IndicatorConfig::Vwap {
                spec: lines_indicators::VwapSpec::default(),
                color: 1,
                display: Default::default(),
                style: Default::default(),
            }],
            profile: ProfileConfig {
                mode: crate::profile::ProfileMode::Session,
                ..ProfileConfig::default()
            },
            levels: crate::levels_layer::LevelsConfig {
                hide_labels: true,
                hidden_sources: vec!["demo".into()],
                ..Default::default()
            },
            magnet: false,
            order_strip: false,
            footprint: Default::default(),
            styles: Default::default(),
            subpane: None,
        };
        let json = serde_json::to_string(&hidden).unwrap();
        assert!(
            json.contains(r#""hide_study_labels":true"#) && json.contains(r#""hide_studies":true"#),
            "{json}"
        );
        // Levels (decision 0022): their own optional field beside the others.
        assert!(
            json.contains(r#""levels":{"hide-labels":true,"hidden-sources":["demo"]}"#),
            "{json}"
        );
        // Hiding keeps every study's configuration.
        assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), hidden);
        let shown = ChartState {
            visibility: StudyVisibility::default(),
            levels: Default::default(),
            ..hidden.clone()
        };
        let json = serde_json::to_string(&shown).unwrap();
        assert!(!json.contains("hide_"), "{json}");
        // Workspace preference: hint bar shown unless saved hidden.
        let mut w = WorkspaceDoc::pair();
        assert!(!w.hide_hints);
        let text = String::from_utf8(encode(&FORMAT, &w).unwrap()).unwrap();
        assert!(!text.contains("hide_hints"), "{text}");
        w.hide_hints = true;
        let bytes = encode(&FORMAT, &w).unwrap();
        let Decoded::Ok { value, .. } = decode::<WorkspaceDoc>(&FORMAT, &bytes) else {
            panic!("round trip")
        };
        assert!(value.hide_hints);
        // Performance stats: off unless saved on, written only when on.
        assert!(!value.show_perf_stats);
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("show_perf_stats")
        );
        let mut w = value;
        w.show_perf_stats = true;
        let bytes = encode(&FORMAT, &w).unwrap();
        let Decoded::Ok { value, .. } = decode::<WorkspaceDoc>(&FORMAT, &bytes) else {
            panic!("round trip")
        };
        assert!(value.show_perf_stats);
        // The latency HUD: the same (off unless saved on, written only when on).
        assert!(!value.show_latency_hud);
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("show_latency_hud")
        );
        let mut w = value;
        w.show_latency_hud = true;
        let bytes = encode(&FORMAT, &w).unwrap();
        let Decoded::Ok { value, .. } = decode::<WorkspaceDoc>(&FORMAT, &bytes) else {
            panic!("round trip")
        };
        assert!(value.show_latency_hud);
    }

    #[test]
    fn toggles_and_clean_chart() {
        let v = StudyVisibility::default();
        // Hidden studies hide their labels too; labels alone leave the studies drawn.
        let labels = StudyVisibility {
            hide_study_labels: true,
            ..v
        };
        assert!(labels.draws_studies() && !labels.draws_labels());
        let studies = StudyVisibility {
            hide_studies: true,
            ..v
        };
        assert!(!studies.draws_studies() && !studies.draws_labels());
        // Clean chart: labels and hint bar off; again: both back.
        let (c, hints_hidden) = v.clean_chart(false);
        assert_eq!((c, hints_hidden), (labels, true));
        assert_eq!(c.clean_chart(hints_hidden), (v, false));
        // Partly clean (labels hidden, bar shown): cleans fully rather than restoring.
        assert_eq!(labels.clean_chart(false), (labels, true));
        // Hidden studies stay hidden either way.
        let (c, _) = studies.clean_chart(false);
        assert!(c.hide_studies && c.hide_study_labels);
        assert!(c.clean_chart(true).0.hide_studies);
        // The footer goes only when hints are hidden and there is no state to show.
        assert!(StudyVisibility::footer_shown(false, false));
        assert!(!StudyVisibility::footer_shown(true, false));
        assert!(StudyVisibility::footer_shown(true, true));
    }

    #[test]
    fn chart_volume_profile_is_saved_per_pane_only_when_set() {
        use crate::profile::{ProfileMode, ProfileSession};
        let chart = |profile| PaneDoc {
            chart: Some(ChartState {
                timeframe: Default::default(),
                hide_drawings: false,
                tick_continuous: None,
                bar_px: 7.0,
                visibility: StudyVisibility::default(),
                levels: Default::default(),
                indicators: vec![],
                profile,
                magnet: false,
                order_strip: false,
                footprint: Default::default(),
                styles: Default::default(),
                subpane: None,
            }),
            ..PaneDoc::new(PaneKind::Chart, None, None)
        };
        // Defaults are not written: files of charts that never touched the profile are
        // unchanged (and older readers never see the field).
        let plain = serde_json::to_string(&chart(ProfileConfig::default())).unwrap();
        assert!(!plain.contains("profile"), "{plain}");
        let set = ProfileConfig {
            mode: ProfileMode::Composite,
            session: ProfileSession::Rth,
            split: true,
            ..ProfileConfig::default()
        };
        let json = serde_json::to_string(&chart(set)).unwrap();
        assert!(
            json.contains(r#""profile":{"mode":"composite","session":"rth""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<PaneDoc>(&json).unwrap(), chart(set));
        // Beside VWAP: two independent optional fields; each loads without the other.
        let both = ChartState {
            timeframe: Default::default(),
            hide_drawings: false,
            tick_continuous: None,
            bar_px: 7.0,
            visibility: StudyVisibility::default(),
            levels: Default::default(),
            indicators: vec![lines_indicators::IndicatorConfig::Vwap {
                spec: lines_indicators::VwapSpec::default(),
                color: 0,
                display: Default::default(),
                style: Default::default(),
            }],
            profile: set,
            magnet: false,
            order_strip: false,
            footprint: Default::default(),
            styles: Default::default(),
            subpane: None,
        };
        let json = serde_json::to_string(&both).unwrap();
        assert_eq!(serde_json::from_str::<ChartState>(&json).unwrap(), both);
        let only_vwap: ChartState =
            serde_json::from_str(&json.replace(r#","profile":"#, r#","ignored":"#)).unwrap();
        assert_eq!(
            (only_vwap.indicators, only_vwap.profile),
            (both.indicators.clone(), ProfileConfig::default())
        );
        // A partial profile object fills in defaults.
        let partial: PaneDoc = serde_json::from_str(
            r#"{"kind": "chart", "chart": {"bar_px": 7.0, "profile": {"mode": "session"}}}"#,
        )
        .unwrap();
        assert_eq!(
            partial.chart.unwrap().profile,
            ProfileConfig {
                mode: ProfileMode::Session,
                ..ProfileConfig::default()
            }
        );
    }

    #[test]
    fn a_version_1_file_migrates_to_version_2() {
        // Pinned: exactly what version 1 wrote. It must keep migrating to the current format.
        let v1 = br#"{
          "version": 1,
          "name": "desk",
          "crosshair_sync": true,
          "windows": [{
            "placement": {"display": "abc", "state": "windowed", "x": 0, "y": 0, "w": 900, "h": 600},
            "active_tab": 0,
            "tabs": [{
              "focused": 1,
              "layout": {"split": {"axis": "row", "ratio": 0.6,
                "first": {"pane": {"kind": "chart", "link": 0, "target": {"product": 2, "symbol": "NQ"}, "chart": {"bar_px": 6.0}}},
                "second": {"pane": {"kind": "dom", "link": 0, "target": {"product": 1, "contract": {"monthly": {"year": 2026, "month": 12}}}, "dom": {"grouping": 2}}}}}
            }]
          }]
        }"#;
        let Decoded::Ok {
            value,
            migrated_from,
        } = decode::<WorkspaceDoc>(&FORMAT, v1)
        else {
            panic!("v1 no longer reads")
        };
        assert_eq!(migrated_from, Some(1));
        assert_eq!(value.name, "desk");
        // (and on through version 3, unchanged)
        assert!(
            value.windows[0].tabs[0]
                .layout
                .leaves()
                .iter()
                .all(|p| p.state == PaneState::Shown && p.window.is_none()),
            "every v1 pane was shown"
        );
        let leaves = value.windows[0].tabs[0].layout.leaves();
        assert_eq!(
            leaves[0].chart,
            Some(ChartState {
                timeframe: Default::default(),
                hide_drawings: false,
                tick_continuous: None,
                bar_px: 6.0,
                visibility: StudyVisibility::default(),
                levels: Default::default(),
                indicators: vec![],
                profile: ProfileConfig::default(),
                magnet: false,
                order_strip: false,
                footprint: Default::default(),
                styles: Default::default(),
                subpane: None,
            })
        );
        assert_eq!(
            leaves[1].target.as_ref().unwrap().to_target(),
            Ok(Target::Contract(ContractKey::monthly(
                catalog::ES,
                2026,
                Month::new(12).unwrap()
            )))
        );
    }

    #[test]
    fn a_version_2_file_still_reads() {
        // Pinned: exactly what version 2 wrote. It migrates to version 3 unchanged.
        let v2 = br#"{
          "version": 2,
          "name": "desk",
          "windows": [{
            "tabs": [{
              "focused": 0,
              "layout": {"split": {"axis": "row", "ratio": 0.68,
                "first": {"pane": {"kind": "chart", "link": 1, "state": "shown"}},
                "second": {"split": {"axis": "column", "ratio": 0.64,
                  "first": {"pane": {"kind": "dom", "link": 1, "state": "detached",
                    "window": {"display": "right", "state": "windowed", "x": 2000, "y": 80, "w": 320, "h": 900}}},
                  "second": {"pane": {"kind": "tape", "link": 1, "state": "chart_only"}}}}}}
            }, {
              "layout": {"pane": {"kind": "dom", "state": "hidden"}}
            }]
          }]
        }"#;
        let Decoded::Ok {
            value,
            migrated_from: Some(2),
        } = decode::<WorkspaceDoc>(&FORMAT, v2)
        else {
            panic!("v2 no longer reads")
        };
        let states: Vec<_> = value.windows[0]
            .tabs
            .iter()
            .flat_map(|t| t.layout.leaves())
            .map(|p| p.state)
            .collect();
        assert_eq!(
            states,
            vec![
                PaneState::Shown,
                PaneState::Detached,
                PaneState::ChartOnly,
                PaneState::Hidden
            ]
        );
        let dom = value.windows[0].tabs[0].layout.leaves()[1];
        assert_eq!(dom.window.as_ref().unwrap().x, 2000.0);
    }

    #[test]
    fn a_version_3_file_as_written_today_still_reads() {
        // Pinned: if this stops parsing, the format changed and needs a version bump plus a
        // migration (crate::versioned), not an edit here.
        let v3 = br#"{
          "version": 3,
          "name": "desk",
          "windows": [{
            "tabs": [{
              "focused": 0,
              "layout": {"split": {"axis": "row", "ratio": 0.6,
                "first": {"pane": {"kind": "chart", "link": 0, "state": "shown"}},
                "second": {"pane": {"kind": "journal", "state": "shown",
                  "journal": {"all_dates": true, "columns": {"tags": {"shown": false}}}}}}}
            }]
          }]
        }"#;
        let Decoded::Ok {
            value,
            migrated_from: Some(3),
        } = decode::<WorkspaceDoc>(&FORMAT, v3)
        else {
            panic!("v3 no longer reads")
        };
        let leaves = value.windows[0].tabs[0].layout.leaves();
        assert_eq!(leaves[1].kind, PaneKind::Journal);
        let j = leaves[1].journal.as_ref().unwrap();
        assert!(j.all_dates);
        assert_eq!(j.columns[&JournalCol::Tags].shown, Some(false));
        assert_eq!(j.group, JournalGroup::Trades, "no group: the trade list");
    }

    /// The journal's group (phase 2) is an optional field: written only when not the trade
    /// list, read back, and a value from a newer build reads as the trade list (never a
    /// corrupt workspace).
    #[test]
    fn journal_group_is_optional_and_lenient() {
        for g in JournalGroup::ALL {
            let s = JournalState {
                group: g,
                ..Default::default()
            };
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json.contains("group"), !g.is_trades(), "{json}");
            assert_eq!(serde_json::from_str::<JournalState>(&json).unwrap(), s);
        }
        for odd in [r#""hours""#, "null", "7", "{}"] {
            let newer: JournalState =
                serde_json::from_str(&format!(r#"{{"group": {odd}, "all_dates": true}}"#)).unwrap();
            assert_eq!(newer.group, JournalGroup::Trades, "{odd}");
            assert!(newer.all_dates);
        }
    }

    #[test]
    fn the_migration_rejects_shapes_it_cannot_follow() {
        for bad in [
            &br#"{"version": 1}"#[..],
            br#"{"version": 1, "windows": [{"tabs": [{"layout": {"tree": 1}}]}]}"#,
            br#"{"version": 1, "windows": [{"tabs": [{"layout": {"split": {"first": {"pane": {"kind": "dom"}}}}}]}]}"#,
        ] {
            assert!(
                matches!(decode::<WorkspaceDoc>(&FORMAT, bad), Decoded::Corrupt { .. }),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn panes_without_column_fields_load_with_default_columns() {
        // Version 2 as written before column settings existed: no `columns` anywhere.
        let v2 = br#"{"version": 2, "windows": [{"tabs": [{"layout": {"split": {"axis": "row", "ratio": 0.5,
            "first": {"pane": {"kind": "dom", "state": "shown", "dom": {"grouping": 2}}},
            "second": {"pane": {"kind": "tape", "state": "shown", "tape": {"large_print": 20}}}}}}]}]}"#;
        let Decoded::Ok {
            value,
            migrated_from: Some(2),
        } = decode::<WorkspaceDoc>(&FORMAT, v2)
        else {
            panic!("reads")
        };
        let leaves = value.windows[0].tabs[0].layout.leaves();
        let dom = leaves[0].dom.as_ref().unwrap();
        assert_eq!((dom.grouping, dom.columns.is_empty()), (2, true));
        let tape = leaves[1].tape.as_ref().unwrap();
        assert_eq!(
            (tape.large_print, tape.columns.is_empty()),
            (Some(20), true)
        );
        // And defaults write no `columns` field: files stay as they were.
        let text = String::from_utf8(encode(&FORMAT, &value).unwrap()).unwrap();
        assert!(!text.contains("columns"), "{text}");
    }

    #[test]
    fn column_settings_round_trip_per_pane() {
        let mut dom = crate::pane_columns::Columns::new(crate::pane_columns::Table::Dom);
        use crate::pane_columns::{Column, ColumnCmd};
        dom.apply(ColumnCmd::Width(Column::Dom(DomCol::Price), 110))
            .unwrap();
        dom.apply(ColumnCmd::Toggle(Column::Dom(DomCol::Volume)))
            .unwrap();
        dom.apply(ColumnCmd::Toggle(Column::Dom(DomCol::Orders)))
            .unwrap();
        let state = DomState {
            grouping: 1,
            columns: dom.save(&DomCol::ALL),
            trail: None,
        };
        let w = WorkspaceDoc::single(
            "cols",
            Layout::Pane(PaneDoc {
                dom: Some(state),
                ..PaneDoc::new(PaneKind::Dom, None, None)
            }),
        );
        let bytes = encode(&FORMAT, &w).unwrap();
        let Decoded::Ok { value, .. } = decode::<WorkspaceDoc>(&FORMAT, &bytes) else {
            panic!()
        };
        assert_eq!(value, w);
        let mut back = crate::pane_columns::Columns::new(crate::pane_columns::Table::Dom);
        let leaves = value.windows[0].tabs[0].layout.leaves();
        back.load(&DomCol::ALL, &leaves[0].dom.as_ref().unwrap().columns);
        assert_eq!(back.set, dom.set);
    }

    #[test]
    fn a_version_4_file_as_written_today_still_reads() {
        // Pinned, as version 3 above: version 4 added the `strategies` pane kind.
        let v4 = br#"{
          "version": 4,
          "name": "desk",
          "windows": [{
            "tabs": [{
              "focused": 0,
              "layout": {"split": {"axis": "row", "ratio": 0.6,
                "first": {"pane": {"kind": "dom", "link": 0, "state": "shown"}},
                "second": {"pane": {"kind": "strategies", "state": "shown"}}}}
            }]
          }]
        }"#;
        let Decoded::Ok {
            value,
            migrated_from: Some(4),
        } = decode::<WorkspaceDoc>(&FORMAT, v4)
        else {
            panic!("v4 no longer reads")
        };
        let leaves = value.windows[0].tabs[0].layout.leaves();
        assert_eq!(leaves[1].kind, PaneKind::Strategies);
    }

    /// Version 6 (decision 0028): the `chain` pane kind, pinned; a version 5 file reads
    /// unchanged.
    #[test]
    fn a_version_6_file_with_an_option_chain_reads() {
        let v6 = br#"{
          "version": 6,
          "name": "options",
          "windows": [{
            "tabs": [{
              "focused": 0,
              "layout": {"split": {"axis": "row", "ratio": 0.4,
                "first": {"pane": {"kind": "chart", "link": 0, "state": "shown",
                  "target": {"product": 2001, "symbol": "SPY"}}},
                "second": {"pane": {"kind": "chain", "link": 0, "state": "shown",
                  "target": {"product": 2001, "symbol": "SPY"}}}}}
            }]
          }]
        }"#;
        let Decoded::Ok {
            value,
            migrated_from: None,
        } = decode::<WorkspaceDoc>(&FORMAT, v6)
        else {
            panic!("v6 no longer reads")
        };
        let leaves = value.windows[0].tabs[0].layout.leaves();
        assert_eq!(leaves[1].kind, PaneKind::Chain);
        let v5 = br#"{"version": 5, "name": "a", "windows": [{"tabs": [{"focused": 0,
            "layout": {"pane": {"kind": "dom", "link": 0, "state": "shown"}}}]}]}"#;
        assert!(matches!(
            decode::<WorkspaceDoc>(&FORMAT, v5),
            Decoded::Ok {
                migrated_from: Some(5),
                ..
            }
        ));
    }

    /// Version 5 (decision 0028): a version 4 file's futures and spot contracts read back as
    /// the same keys; equity and option contracts round-trip; the written file says 5.
    #[test]
    fn version_5_keeps_futures_and_adds_equities_and_options() {
        let v4 = br#"{
          "version": 4,
          "name": "desk",
          "windows": [{
            "tabs": [{
              "focused": 0,
              "layout": {"split": {"axis": "row", "ratio": 0.5,
                "first": {"pane": {"kind": "chart", "link": 0, "state": "shown",
                  "target": {"product": 1, "symbol": "ES", "contract": {"monthly": {"year": 2026, "month": 12}}}}},
                "second": {"pane": {"kind": "dom", "link": 0, "state": "shown",
                  "target": {"product": 1001, "symbol": "BTC/USD", "contract": "spot"}}}}}
            }]
          }]
        }"#;
        let Decoded::Ok { value, .. } = decode::<WorkspaceDoc>(&FORMAT, v4) else {
            panic!("v4 no longer reads")
        };
        let leaves = value.windows[0].tabs[0].layout.leaves();
        let target = |i: usize| leaves[i].target.as_ref().unwrap().to_target().unwrap();
        assert_eq!(
            target(0),
            Target::Contract(ContractKey::monthly(
                catalog::ES,
                2026,
                Month::new(12).unwrap()
            ))
        );
        assert_eq!(
            target(1),
            Target::Contract(ContractKey::spot(catalog::BTC_USD))
        );
        let text = String::from_utf8(encode(&FORMAT, &value).unwrap()).unwrap();
        assert!(text.contains(r#""version": 6"#), "{text}");
        assert!(
            text.contains(r#""monthly""#),
            "the futures shape is unchanged"
        );
        // Equities and options round-trip (exact strike, expiry, right).
        let spy = catalog::SPY;
        let opt = ContractKey::option(
            spy,
            lines_core::CivilDate::new(2026, 12, 18).unwrap(),
            lines_core::Price(45_050),
            lines_core::Right::Put,
        );
        for k in [ContractKey::equity(spy), opt] {
            let doc = TargetDoc::from_target(&Target::Contract(k));
            let json = serde_json::to_string(&doc).unwrap();
            let back: TargetDoc = serde_json::from_str(&json).unwrap();
            assert_eq!(back.to_target().unwrap(), Target::Contract(k), "{json}");
        }
        let json = serde_json::to_string(&TargetDoc::from_target(&Target::Contract(opt))).unwrap();
        assert!(
            json.contains(r#""option":{"expiry":20261218,"strike":45050,"right":"put"}"#),
            "{json}"
        );
        // A kind the product doesn't list is refused (kept, shown unavailable).
        let bad = TargetDoc {
            product: catalog::ES.0,
            symbol: None,
            contract: Some(ContractDoc::Equity),
        };
        assert!(bad.to_target().is_err());
    }

    #[test]
    fn unversioned_and_newer_files_are_not_guessed_at() {
        assert!(matches!(
            decode::<WorkspaceDoc>(&FORMAT, br#"{"windows": []}"#),
            Decoded::Corrupt { .. }
        ));
        assert_eq!(
            decode::<WorkspaceDoc>(&FORMAT, br#"{"version": 7, "windows": 7}"#),
            Decoded::Newer { found: 7 }
        );
    }

    #[test]
    fn unknown_products_are_kept_and_explained() {
        let t = TargetDoc {
            product: 9999,
            symbol: Some("ZZ".into()),
            contract: None,
        };
        assert_eq!(t.to_target(), Err("unknown product ZZ (id 9999)".into()));
        // A spot "contract" on a future, or a month 13: explained, not guessed.
        let bad = TargetDoc {
            contract: Some(ContractDoc::Spot),
            ..TargetDoc::product(catalog::ES)
        };
        assert!(bad.to_target().is_err());
        let bad = TargetDoc {
            contract: Some(ContractDoc::Monthly {
                year: 2026,
                month: 13,
            }),
            ..TargetDoc::product(catalog::ES)
        };
        assert_eq!(bad.to_target(), Err("bad contract month 13".into()));
        let spot = TargetDoc::from_target(&Target::Contract(ContractKey::spot(catalog::BTC_USD)));
        assert_eq!(
            spot.to_target(),
            Ok(Target::Contract(ContractKey::spot(catalog::BTC_USD)))
        );
    }

    #[test]
    fn normalise_repairs_indexes_without_dropping_panes() {
        let mut w = WorkspaceDoc::pair();
        w.windows[0].active_tab = 9;
        w.windows[0].tabs[0].focused = 99;
        w.windows.push(WindowDoc {
            placement: None,
            active_tab: 0,
            tabs: vec![],
        });
        let before = w.pane_count();
        w.normalise().unwrap();
        assert_eq!(w.windows.len(), 1);
        assert_eq!(w.windows[0].active_tab, 0);
        assert_eq!(w.windows[0].tabs[0].focused, 5);
        assert_eq!(w.pane_count(), before);
        let mut empty = WorkspaceDoc {
            windows: vec![],
            ..WorkspaceDoc::pair()
        };
        assert!(empty.normalise().is_err());
    }

    #[test]
    fn normalise_keeps_a_pane_on_screen_and_charts_unhidden() {
        let mut w = WorkspaceDoc::pair();
        let all = |w: &mut WorkspaceDoc, s: PaneState| {
            let t = &mut w.windows[0].tabs[0];
            t.layout = t.layout.map(&mut |p: &PaneDoc| PaneDoc {
                state: if p.kind == PaneKind::Chart {
                    PaneState::Detached
                } else {
                    s
                },
                ..p.clone()
            });
        };
        all(&mut w, PaneState::Hidden);
        w.normalise().unwrap();
        let states: Vec<_> = w.windows[0].tabs[0]
            .layout
            .leaves()
            .iter()
            .map(|p| p.state)
            .collect();
        assert_eq!(
            states[1],
            PaneState::Shown,
            "the first hidden pane comes back"
        );
        assert_eq!(states.iter().filter(|s| s.is_shown()).count(), 1);
        // A chart saved as hidden (hand-edited) is shown.
        let t = &mut w.windows[0].tabs[0];
        t.layout = t.layout.map(&mut |p: &PaneDoc| PaneDoc {
            state: if p.kind == PaneKind::Chart {
                PaneState::ChartOnly
            } else {
                p.state
            },
            ..p.clone()
        });
        w.normalise().unwrap();
        assert!(
            w.windows[0].tabs[0]
                .layout
                .leaves()
                .iter()
                .filter(|p| p.kind == PaneKind::Chart)
                .all(|p| p.state.is_shown())
        );
    }

    #[test]
    fn hidden_and_detached_panes_survive_a_restart() {
        // What autosave writes and the next start reads back: states and the detached window.
        let mut w = WorkspaceDoc::pair();
        let t = &mut w.windows[0].tabs[0];
        let mut i = 0;
        t.layout = t.layout.map(&mut |p: &PaneDoc| {
            i += 1;
            let mut p = p.clone();
            match i {
                2 => {
                    p.state = PaneState::Detached;
                    p.window = Some(Placement {
                        display: Some("right".into()),
                        state: WindowState::Windowed,
                        x: 2000.0,
                        y: 100.0,
                        w: 320.0,
                        h: 800.0,
                    });
                }
                3 => p.state = PaneState::Hidden,
                5 | 6 => p.state = PaneState::ChartOnly,
                _ => {}
            }
            p
        });
        let bytes = encode(&FORMAT, &w).unwrap();
        let Decoded::Ok { mut value, .. } = decode::<WorkspaceDoc>(&FORMAT, &bytes) else {
            panic!()
        };
        value.normalise().unwrap();
        assert_eq!(value, w);
    }

    #[test]
    fn names_are_safe_file_names() {
        assert_eq!(valid_name("  NQ ES desk "), Ok("NQ ES desk".into()));
        assert!(valid_name("").is_err());
        assert!(valid_name("../x").is_err());
        assert!(valid_name("a:b").is_err());
        assert!(valid_name(&"x".repeat(41)).is_err());
        assert!(valid_name("2x2").is_err(), "built-in");
        assert!(valid_name("Pair").is_err(), "built-in");
        for b in ["pair", "6c2d", "1x2", "2x2", "1+2", "trading", "chart"] {
            assert!(builtin(b).is_some(), "{b}");
        }
    }

    fn display(uuid: &str, x: f32, w: f32) -> DisplayInfo {
        DisplayInfo {
            uuid: Some(uuid.into()),
            bounds: Rect::new(x, 0.0, w, 1080.0),
        }
    }

    fn placed(display: &str, x: f32, y: f32, w: f32, h: f32) -> Placement {
        Placement {
            display: Some(display.into()),
            state: WindowState::Windowed,
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn windows_return_to_their_monitor_or_somewhere_reachable() {
        let two = [
            display("left", 0.0, 1920.0),
            display("right", 1920.0, 2560.0),
        ];
        // Its monitor is connected: exactly where it was.
        let p = placed("right", 2000.0, 100.0, 1200.0, 800.0);
        assert_eq!(
            restore_placement(&p, &two),
            (Some(1), Rect::new(2000.0, 100.0, 1200.0, 800.0))
        );
        // Right monitor unplugged: centred on the primary, shrunk to fit.
        let one = [display("left", 0.0, 1920.0)];
        let big = placed("right", 2000.0, 100.0, 2400.0, 1300.0);
        assert_eq!(
            restore_placement(&big, &one),
            (Some(0), Rect::new(0.0, 0.0, 1920.0, 1080.0))
        );
        let p = placed("right", 2000.0, 100.0, 1000.0, 600.0);
        assert_eq!(
            restore_placement(&p, &one),
            (Some(0), Rect::new(460.0, 240.0, 1000.0, 600.0))
        );
        // Unknown id but still reachable where it is: kept.
        let p = placed("gone", 100.0, 100.0, 800.0, 600.0);
        assert_eq!(restore_placement(&p, &two).0, Some(0));
        // Barely on screen (a sliver): moved back.
        let p = placed("left", 1900.0 - 1920.0 * 0.0 + 15.0, 100.0, 800.0, 600.0);
        assert_eq!(restore_placement(&p, &one).1.x, 560.0);
        // No displays reported: as saved.
        assert_eq!(restore_placement(&p, &[]).0, None);
    }

    #[test]
    fn dropping_a_tab() {
        let own = Rect::new(0.0, 0.0, 1000.0, 700.0);
        let bar = Rect::new(0.0, 30.0, 1000.0, 26.0);
        let tabs = [120.0, 240.0, 360.0];
        let others = [("b", Rect::new(1100.0, 0.0, 800.0, 600.0))];
        assert_eq!(
            tab_drop((50.0, 40.0), own, bar, &tabs, &others),
            TabDrop::Reorder(0)
        );
        assert_eq!(
            tab_drop((250.0, 40.0), own, bar, &tabs, &others),
            TabDrop::Reorder(2)
        );
        assert_eq!(
            tab_drop((900.0, 40.0), own, bar, &tabs, &others),
            TabDrop::Reorder(2),
            "past the last tab: last"
        );
        assert_eq!(
            tab_drop((300.0, 70.0), own, bar, &tabs, &others),
            TabDrop::Cancel
        );
        assert_eq!(
            tab_drop((300.0, 400.0), own, bar, &tabs, &others),
            TabDrop::NewWindow { x: 300.0, y: 400.0 }
        );
        assert_eq!(
            tab_drop((1200.0, 300.0), own, bar, &tabs, &others),
            TabDrop::ToWindow("b")
        );
        assert_eq!(
            tab_drop((1050.0, 300.0), own, bar, &tabs, &others),
            TabDrop::NewWindow {
                x: 1050.0,
                y: 300.0
            }
        );
    }
}
