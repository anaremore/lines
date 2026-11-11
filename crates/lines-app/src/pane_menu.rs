//! What a pane's context menu holds, by the menu rules (`docs/design.md`, "Menus";
//! `crate::menu`): about 12 top-level rows; sets as submenus one level deep, each showing its
//! value (Timeframe ›, Show ›, Drawings ›, Pane ›, Link ›); show/hide as checkmarks; never a
//! row per instance; destructive rows last; "Settings…" (at the pane's own section) then "All
//! commands…" at the end. An object under the
//! pointer (an order's tag, a drawing) gets its own menu instead, its name and value as a
//! header. Pure: the workspace and pane windows read the pane's state into [`PaneMenu`].

use lines_core::Timeframe;

use crate::commands::{Command, DrawCmd, OrderCmd, StrategyCmd};
use crate::layout::{LinkGroup, PaneKind, link_letter};
use crate::menu::MenuItem;
use crate::pnl::PnlDisplay;
use crate::wsdoc::JournalGroup;

/// The timeframes a chart's Timeframe › lists; "All timeframes…" opens the full list.
pub const QUICK_TIMEFRAMES: [Timeframe; 9] = [
    Timeframe::Minutes(1),
    Timeframe::Minutes(2),
    Timeframe::Minutes(5),
    Timeframe::Minutes(15),
    Timeframe::Minutes(30),
    Timeframe::Hours(1),
    Timeframe::Day,
    Timeframe::Ticks(233),
    Timeframe::Ticks(1000),
];

/// A chart's state, as its menu shows it.
#[derive(Clone, Debug)]
pub struct ChartMenu {
    pub timeframe: Timeframe,
    /// Trade history's options (`trade_marks::TradesConfig`).
    pub trades: crate::trade_marks::TradesConfig,
    /// Trade-built bars (ticks, volume, range): their session switch shows.
    pub on_ticks: bool,
    pub tick_continuous: bool,
    /// The chart has level sources: (levels shown, labels shown, where its labels go).
    pub levels: Option<(bool, bool, crate::levels_layer::LabelPlace)>,
    pub studies_shown: bool,
    /// The chart's Volume: on the chart (`Some(true)`) or in a pane; `None`: no Volume.
    pub volume: Option<bool>,
    pub study_labels_shown: bool,
    pub strategy_marks_shown: bool,
    pub clean: bool,
    pub order_strip: bool,
    pub magnet: bool,
    pub drawings_shown: bool,
    /// The drawing toolbar is shown (`crate::drawing_toolbar`).
    pub drawing_tools: bool,
    pub drawing_selected: bool,
    pub symbol: String,
    pub chart_only: bool,
    /// The order section a right click on the plot opens with (`None`: no section: not on
    /// the plot, or an option chart, which keeps its trigger flow).
    pub orders: Option<ChartOrders>,
}

/// The chart menu's order section (owner-approved mock, 2026-11-09).
#[derive(Clone, Debug, PartialEq)]
pub enum ChartOrders {
    /// The header ("At 6012.25 · 2 contracts · bracket 8/16") and the two orders.
    Rows {
        header: String,
        /// Each order's label, the order, and its "…with bracket" row's label (`None`: the
        /// order reduces the position, so no bracket goes on it).
        rows: Vec<(String, crate::chart_trade::MenuOrder, Option<String>)>,
        /// The contract and size they are offered with (checked again when one is chosen).
        offer: crate::chart_trade::Offer,
    },
    /// One dim row saying why there are none (trading off, no live price).
    Note(String),
}

#[derive(Clone, Debug)]
pub struct DomMenu {
    pub trail: bool,
    pub half_life_ms: u32,
}

#[derive(Clone, Debug)]
pub struct JournalMenu {
    pub group: JournalGroup,
    pub all_dates: bool,
}

/// A pane's state, as its menu shows it.
#[derive(Clone, Debug)]
pub struct PaneMenu {
    pub kind: PaneKind,
    pub link: LinkGroup,
    pub chart: Option<ChartMenu>,
    pub dom: Option<DomMenu>,
    pub journal: Option<JournalMenu>,
    /// The app's P&L display (a DOM's menu sets it).
    pub pnl: PnlDisplay,
    /// The linked DOMs / time & sales a toggle acts on: shown, hidden, or none (no row).
    pub linked_dom: Option<bool>,
    pub linked_tape: Option<bool>,
    pub hidden_panes: bool,
    pub detached_panes: bool,
    pub closed_tabs: bool,
    /// Recording live feeds (`crate::recording`): on or off; `None` when there is nothing live
    /// to record (no row).
    pub recording: Option<bool>,
}

/// The thing under the pointer at a right click.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuObject {
    /// A working order's tag: its id, its tag's text and P&L, and whether breakeven would move
    /// it ([`offers_breakeven`]).
    Order { id: u64, title: String, stop: bool },
    /// A drawing (now the selection): what it is.
    Drawing { title: String },
    /// The temporary measure: which parts its readout shows.
    Measure {
        show: crate::chart_draw::MeasureShow,
        /// The P&L display setting: off hides ticks and P&L whatever the choice, and the
        /// rows say so (money-relevant: never a checked box that shows nothing).
        pnl: crate::pnl::PnlDisplay,
    },
}

/// A menu a screenshot opens (`--context-menu`, decision 0009): the focused pane's, or the
/// focused chart's first working order's; with one submenu open; at a window point.
#[derive(Clone, Debug, PartialEq)]
pub struct ShotMenu {
    pub order: bool,
    /// The focused chart's temporary measure's menu (`measure`).
    pub measure: bool,
    /// The tab's menu (`tab`).
    pub tab: bool,
    pub sub: Option<String>,
    pub at: (f32, f32),
}

/// `pane`, `order` or `tab`, then `:Label` to open the submenu of that row, then `@X,Y` (logical px;
/// default 200,120): `pane:Show@1400,300`.
pub fn parse_shot_menu(s: &str) -> Result<ShotMenu, String> {
    let (rest, at) = match s.split_once('@') {
        Some((rest, at)) => {
            let (x, y) = at
                .split_once(',')
                .ok_or_else(|| format!("{at}: expected X,Y"))?;
            let n = |v: &str| {
                v.trim()
                    .parse::<f32>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .ok_or_else(|| format!("{v}: not a position"))
            };
            (rest, (n(x)?, n(y)?))
        }
        None => (s, (200.0, 120.0)),
    };
    let (what, sub) = match rest.split_once(':') {
        Some((w, sub)) => (w, Some(sub.to_owned())),
        None => (rest, None),
    };
    let (order, tab, measure) = match what {
        "pane" => (false, false, false),
        "order" => (true, false, false),
        "tab" => (false, true, false),
        "measure" => (false, false, true),
        other => return Err(format!("{other}: expected pane, order, tab or measure")),
    };
    Ok(ShotMenu {
        order,
        measure,
        tab,
        sub,
        at,
    })
}

/// Whether an order's menu offers "Move all stops to breakeven": a stop order against an open
/// position (a long's sell stops, a short's buy stops), what `trade_bar::protective` moves.
pub fn offers_breakeven(stop_order: bool, sells: bool, position: Option<i64>) -> bool {
    stop_order && position.is_some_and(|q| q != 0 && sells == (q > 0))
}

fn row(label: &str, cmd: Command) -> MenuItem {
    MenuItem::named(label, cmd, false)
}

/// An on/off switch: a box, checked when on.
fn toggle(label: &str, cmd: Command, on: bool) -> MenuItem {
    MenuItem::named(label, cmd, on).switch()
}

/// One of a set (a timeframe, a link group): a check on the one in force.
fn pick(label: &str, cmd: Command, on: bool) -> MenuItem {
    MenuItem::named(label, cmd, on)
}

/// An object's menu: its name and value, what can be done to it, the destructive row last.
pub fn object_items(o: &MenuObject) -> Vec<MenuItem> {
    match o {
        MenuObject::Order { id, title, stop } => {
            let mut v = vec![MenuItem::title(title), MenuItem::sep()];
            if *stop {
                // The position's stops, through the modify path (`TradeCmd::Breakeven`).
                v.push(row(
                    "Move all stops to breakeven",
                    Command::Order(OrderCmd::Breakeven),
                ));
            }
            v.push(row("Change size…", Command::Order(OrderCmd::Resize(*id))));
            v.push(MenuItem::sep());
            v.push(row("Cancel order", Command::Order(OrderCmd::Cancel(*id))).destructive());
            v
        }
        MenuObject::Measure { show, pnl } => {
            use crate::chart_draw::MeasurePart;
            let mut v = vec![MenuItem::title("Measure"), MenuItem::sep()];
            for p in MeasurePart::ALL {
                let hidden = match p {
                    MeasurePart::Ticks => !pnl.ticks(),
                    MeasurePart::Pnl => !pnl.money(),
                    _ => false,
                };
                let label = if hidden {
                    format!("{} (P&L display: {})", p.label(), pnl.label())
                } else {
                    p.label().to_owned()
                };
                v.push(toggle(
                    &label,
                    Command::Draw(DrawCmd::MeasurePart(p)),
                    show.get(p),
                ));
            }
            v.push(MenuItem::sep());
            v.push(row("Remove measure", Command::Draw(DrawCmd::RemoveMeasure)).destructive());
            v
        }
        MenuObject::Drawing { title } => vec![
            MenuItem::title(title),
            MenuItem::sep(),
            row("Style…", Command::Draw(DrawCmd::Style)),
            MenuItem::sep(),
            row("Delete", Command::Draw(DrawCmd::DeleteSelected)).destructive(),
        ],
    }
}

/// The chart menu's order section, its separator below (main and popped-out windows).
pub fn order_items(o: &ChartOrders) -> Vec<MenuItem> {
    let mut v = Vec::with_capacity(4);
    match o {
        ChartOrders::Rows {
            header,
            rows,
            offer,
        } => {
            v.push(MenuItem::title(header));
            // Each order twice: sent at once, then staged on the chart with its bracket's legs
            // (unsent until Place). A row whose order would reduce the position has no staged
            // twin: no bracket goes on it.
            for (label, o, stage) in rows {
                v.push(row(label, Command::Order(OrderCmd::Place(*o, *offer))));
                if let Some(staged) = stage {
                    v.push(row(staged, Command::Order(OrderCmd::Stage(*o, *offer))));
                }
            }
        }
        ChartOrders::Note(n) => v.push(MenuItem::title(n)),
    }
    v.push(MenuItem::sep());
    v
}

/// Whether a menu has a row that places an order: it opens with nothing highlighted, so a
/// stray Enter never sends one (design.md §6 rule 5).
pub fn places_orders(items: &[MenuItem]) -> bool {
    items.iter().any(|i| {
        matches!(
            i.cmd,
            Some(Command::Order(OrderCmd::Place(..) | OrderCmd::Stage(..)))
        )
    })
}

/// A pane's menu.
pub fn items(m: &PaneMenu) -> Vec<MenuItem> {
    let mut v = Vec::new();
    if let Some(c) = &m.chart {
        if let Some(o) = &c.orders {
            v.extend(order_items(o));
        }
        v.push(row("Studies…", Command::Studies));
        v.push(timeframe(c));
        v.push(chart_show(c));
        v.push(chart_trades(c));
        v.push(MenuItem::sep());
        v.push(toggle(
            "Order strip",
            Command::ToggleOrderStrip,
            c.order_strip,
        ));
        v.push(toggle("Magnet crosshair", Command::ToggleMagnet, c.magnet));
        v.push(MenuItem::sep());
        v.push(drawings(c));
        v.push(MenuItem::sep());
    }
    if let Some(d) = &m.dom {
        v.push(MenuItem::submenu(
            "Show",
            "",
            vec![
                toggle("Price trail", Command::DomTrail, d.trail),
                MenuItem {
                    // A value, not a key: never replaced by caps.
                    hint: format!("{:.1} s", f64::from(d.half_life_ms) / 1000.0),
                    key: None,
                    ..row("Next trail half-life", Command::DomTrailHalfLife)
                },
            ],
        ));
        v.push(MenuItem::submenu(
            "P&L",
            &capitalized(m.pnl.label()),
            PnlDisplay::ALL
                .iter()
                .map(|&p| pick(&capitalized(p.label()), Command::SetPnl(p), p == m.pnl))
                .collect(),
        ));
        v.push(MenuItem::sep());
    }
    if let Some(j) = &m.journal {
        let mut show: Vec<MenuItem> = JournalGroup::ALL
            .iter()
            .map(|&g| pick(g.label(), Command::JournalGroup(g), g == j.group))
            .collect();
        show.push(MenuItem::sep());
        show.push(toggle("All dates", Command::JournalAllDates, j.all_dates));
        v.push(MenuItem::submenu("Show", j.group.label(), show));
        v.push(row("Edit tags and note…", Command::JournalEdit));
        v.push(row("Open the trade's snapshot", Command::JournalOpenChart));
        v.push(MenuItem::of(Command::ExportJournal, false));
        v.push(MenuItem::sep());
    }
    v.push(pane(m));
    v.push(link(m.link));
    v.push(MenuItem::sep());
    v.push(settings_row(m.kind));
    v.push(row("All commands…", Command::Palette));
    v
}

/// The Settings section a pane's "Settings…" opens at: its own kind's settings.
pub fn settings_section(kind: PaneKind) -> crate::settings_schema::Section {
    use crate::settings_schema::Section;
    match kind {
        PaneKind::Chart => Section::Charts,
        PaneKind::Dom | PaneKind::Tape => Section::Dom,
        PaneKind::Positions => Section::Accounts,
        PaneKind::Journal | PaneKind::Strategies | PaneKind::Chain => Section::OrderEntry,
    }
}

/// "Settings…" with Settings' key, opening at `kind`'s section (every pane menu, second to last).
pub fn settings_row(kind: PaneKind) -> MenuItem {
    row("Settings…", Command::SettingsAt(settings_section(kind)))
}

/// A tab's state, as its menu shows it.
#[derive(Clone, Debug)]
pub struct TabMenu {
    pub tabs: usize,
    /// The tab the menu is for is pinned.
    pub pinned: bool,
    /// Other tabs Close others would close (not pinned).
    pub closable_others: usize,
    pub closed_tabs: bool,
    pub crosshair_sync: bool,
    pub crosshair_sync_price: bool,
    pub perf_stats: bool,
    pub latency_hud: bool,
}

/// A tab's menu: tab, layout and workspace actions; Close tab last.
pub fn tab_items(m: &TabMenu) -> Vec<MenuItem> {
    let mut v = vec![
        MenuItem::of(Command::RenameTab, false),
        toggle("Pin tab", Command::TogglePinTab, m.pinned),
        MenuItem::sep(),
        MenuItem::of(Command::NewTab, false),
    ];
    if m.closed_tabs {
        v.push(MenuItem::of(Command::ReopenTab, false));
    }
    if m.tabs > 1 {
        v.push(MenuItem::of(Command::TabToNewWindow, false));
    }
    v.push(MenuItem::sep());
    v.push(MenuItem::submenu(
        "Layout",
        "",
        crate::layout::Preset::ALL
            .iter()
            .map(|&p| MenuItem::of(Command::Preset(p), false))
            .collect(),
    ));
    v.push(MenuItem::submenu(
        "Show",
        "",
        vec![
            toggle(
                "Crosshair sync",
                Command::ToggleCrosshairSync,
                m.crosshair_sync,
            ),
            toggle(
                "Crosshair price sync",
                Command::ToggleCrosshairSyncPrice,
                m.crosshair_sync_price,
            ),
            MenuItem::sep(),
            toggle("Performance stats", Command::TogglePerfStats, m.perf_stats),
            toggle("Latency HUD", Command::ToggleLatencyHud, m.latency_hud),
        ],
    ));
    v.push(MenuItem::submenu(
        "Workspace",
        "",
        vec![
            MenuItem::of(Command::SaveAs, false),
            MenuItem::of(Command::Open, false),
            MenuItem::sep(),
            MenuItem::of(Command::ExportDiagnostics, false),
        ],
    ));
    v.push(MenuItem::sep());
    v.push(row("Settings…", Command::Settings));
    v.push(row("All commands…", Command::Palette));
    v.push(MenuItem::sep());
    if m.closable_others > 0 {
        v.push(MenuItem::of(Command::CloseOtherTabs, false).destructive());
    }
    v.push(MenuItem::of(Command::CloseTab, false).destructive());
    v
}

fn timeframe(c: &ChartMenu) -> MenuItem {
    let tf = c.timeframe;
    let mut sub: Vec<MenuItem> = QUICK_TIMEFRAMES
        .iter()
        .map(|&t| pick(&t.to_string(), Command::Timeframe(t), t == tf))
        .collect();
    if !QUICK_TIMEFRAMES.contains(&tf) {
        sub.push(pick(&tf.to_string(), Command::Timeframe(tf), true));
    }
    sub.push(MenuItem::sep());
    sub.push(row("All timeframes…", Command::TimeframeMenu));
    if c.on_ticks {
        sub.push(toggle(
            "Run across sessions",
            Command::TickBarsContinuous,
            c.tick_continuous,
        ));
    }
    MenuItem::submenu("Timeframe", &tf.to_string(), sub)
}

fn chart_show(c: &ChartMenu) -> MenuItem {
    let mut sub = Vec::new();
    if let Some((on, labels, place)) = c.levels {
        sub.push(toggle("Levels", Command::ToggleLevels, on));
        sub.push(toggle("Level labels", Command::ToggleLevelLabels, labels));
        // Where they go (submenus are one level deep, so these are rows here).
        sub.push(MenuItem::header("Labels at"));
        for p in crate::levels_layer::LabelPlace::ALL {
            sub.push(pick(p.label(), Command::LevelLabelPlace(p), place == p));
        }
        // TODO(level-styles, workspaces thread 4c0a): open the per-type level style panel once it
        // merges. Until then the studies menu, which lists each source's types with their
        // switches.
        sub.push(row("Level types…", Command::Studies));
    }
    sub.push(toggle("Studies", Command::ToggleStudies, c.studies_shown));
    // With a Volume: on the chart (on) or in its pane (off).
    if let Some(overlay) = c.volume {
        sub.push(toggle(
            "Volume on the chart",
            Command::VolumeOverlay(!overlay),
            overlay,
        ));
    }
    sub.push(toggle(
        "Study labels",
        Command::ToggleStudyLabels,
        c.study_labels_shown,
    ));
    sub.push(toggle(
        "Strategy markers",
        Command::Strategy(StrategyCmd::ToggleMarks),
        c.strategy_marks_shown,
    ));
    sub.push(toggle(
        "Drawing tools",
        Command::ToggleDrawingTools,
        c.drawing_tools,
    ));
    sub.push(MenuItem::sep());
    sub.push(toggle("Clean chart", Command::CleanChart, c.clean));
    MenuItem::submenu("Show", "", sub)
}

/// Trade history on the chart (`trade_marks`): its switches, then whose trades and which
/// period (submenus are one level deep, so they are rows here). Its value: the period, or off.
fn chart_trades(c: &ChartMenu) -> MenuItem {
    let t = c.trades;
    let mut sub = vec![
        toggle("Show trades", Command::ToggleTrades, !t.hidden),
        toggle("Entry–exit lines", Command::ToggleTradeLines, !t.no_lines),
        toggle("Labels", Command::ToggleTradeLabels, !t.no_labels),
        row("Hide on all charts", Command::TradesOnAllCharts(false)),
        row("Show on all charts", Command::TradesOnAllCharts(true)),
        MenuItem::header("Account"),
        pick(
            "This pane's account",
            Command::TradeAccounts(false),
            !t.all_accounts,
        ),
        pick("All accounts", Command::TradeAccounts(true), t.all_accounts),
        MenuItem::header("Period"),
    ];
    for p in crate::trade_marks::Period::ALL {
        sub.push(pick(p.label(), Command::TradePeriod(p), t.period == p));
    }
    let value = if t.hidden { "Off" } else { t.period.label() };
    MenuItem::submenu("Trades", value, sub)
}

fn drawings(c: &ChartMenu) -> MenuItem {
    let mut sub = Vec::new();
    if c.drawing_selected {
        sub.push(row("Drawing style…", Command::Draw(DrawCmd::Style)));
        sub.push(MenuItem::sep());
    }
    sub.push(toggle(
        "Show drawings",
        Command::Draw(DrawCmd::ToggleVisible),
        c.drawings_shown,
    ));
    sub.push(MenuItem::sep());
    // Asks first (`DrawCmd::DeleteAll`); undo brings them back.
    sub.push(
        row(
            &format!("Delete all drawings for {}…", c.symbol),
            Command::Draw(DrawCmd::DeleteAll),
        )
        .destructive(),
    );
    MenuItem::submenu(
        "Drawings",
        if c.drawings_shown { "" } else { "Hidden" },
        sub,
    )
}

fn pane(m: &PaneMenu) -> MenuItem {
    let mut sub = vec![
        MenuItem::of(Command::Split(crate::layout::Axis::Row), false),
        MenuItem::of(Command::Split(crate::layout::Axis::Column), false),
        row("Pop out", Command::PopOut),
        MenuItem::sep(),
        // What the pane is (pick one); the top-level Show › is what a chart draws.
        MenuItem::header("Show as"),
    ];
    for k in [
        PaneKind::Chart,
        PaneKind::Dom,
        PaneKind::Tape,
        PaneKind::Positions,
    ] {
        sub.push(pick(k.label(), Command::SetKind(k), k == m.kind));
    }
    sub.push(MenuItem::sep());
    if let Some(c) = &m.chart {
        sub.push(toggle("Chart only", Command::ChartOnly, c.chart_only));
    }
    if m.kind != PaneKind::Dom
        && let Some(on) = m.linked_dom
    {
        sub.push(toggle("Linked DOM", Command::Toggle(PaneKind::Dom), on));
    }
    if m.kind != PaneKind::Tape
        && let Some(on) = m.linked_tape
    {
        sub.push(toggle(
            "Linked time & sales",
            Command::Toggle(PaneKind::Tape),
            on,
        ));
    }
    if m.hidden_panes {
        sub.push(MenuItem::of(Command::ShowHidden, false));
    }
    if m.detached_panes {
        sub.push(MenuItem::of(Command::Reattach, false));
    }
    sub.push(MenuItem::sep());
    // Recording is app-wide (every live feed), reached from any pane: a switch, its command
    // the way out when on.
    if let Some(on) = m.recording {
        let cmd = if on {
            Command::StopRecording
        } else {
            Command::StartRecording
        };
        sub.push(toggle("Recording", cmd, on));
        sub.push(MenuItem::sep());
    }
    sub.push(MenuItem::of(Command::NewTab, false));
    if m.closed_tabs {
        sub.push(MenuItem::of(Command::ReopenTab, false));
    }
    sub.push(MenuItem::sep());
    // Hiding is for a DOM or time & sales (a chart is what they hide around).
    if m.chart.is_none() {
        sub.push(MenuItem::of(Command::HidePane, false));
        sub.push(MenuItem::sep());
    }
    sub.push(MenuItem::of(Command::ClosePane, false).destructive());
    MenuItem::submenu("Pane", m.kind.label(), sub)
}

fn link(l: LinkGroup) -> MenuItem {
    let mut sub = Vec::with_capacity(5);
    for g in [None, Some(0), Some(1), Some(2), Some(3)] {
        let label = match g {
            None => "Unlinked".to_owned(),
            Some(_) => format!("Group {}", link_letter(g)),
        };
        let mut r = pick(&label, Command::SetLink(g), g == l);
        if g.is_none() {
            r.hint = crate::commands::hint(Command::CycleLink);
            r.key = Some(Command::CycleLink);
        }
        sub.push(r);
    }
    let value = if l.is_none() {
        "None".to_owned()
    } else {
        link_letter(l).to_string()
    };
    MenuItem::submenu("Link", &value, sub)
}

pub fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart() -> ChartMenu {
        ChartMenu {
            trades: Default::default(),
            timeframe: Timeframe::Minutes(5),
            on_ticks: false,
            tick_continuous: false,
            levels: Some((true, true, crate::levels_layer::LabelPlace::Axis)),
            studies_shown: true,
            volume: None,
            study_labels_shown: false,
            strategy_marks_shown: true,
            clean: false,
            order_strip: false,
            magnet: true,
            drawings_shown: true,
            drawing_tools: false,
            drawing_selected: true,
            symbol: "ES".into(),
            orders: None,
            chart_only: false,
        }
    }

    /// The trail half-life row shows its value ("1.5 s"), never caps, whatever its command's
    /// binding (keycaps replace a row's text only for a key).
    #[test]
    fn the_half_life_row_keeps_its_value() {
        let rows = items(&pane_of(PaneKind::Dom));
        let r = rows
            .iter()
            .flat_map(|r| std::iter::once(r).chain(r.sub.iter()))
            .find(|r| r.label == "Next trail half-life")
            .expect("half-life row");
        assert_eq!((r.key, r.hint.as_str()), (None, "1.5 s"));
    }

    fn pane_of(kind: PaneKind) -> PaneMenu {
        PaneMenu {
            kind,
            link: Some(0),
            chart: (kind == PaneKind::Chart).then(chart),
            dom: (kind == PaneKind::Dom).then_some(DomMenu {
                trail: true,
                half_life_ms: 1500,
            }),
            journal: (kind == PaneKind::Journal).then_some(JournalMenu {
                group: JournalGroup::Trades,
                all_dates: false,
            }),
            pnl: PnlDisplay::Both,
            linked_dom: Some(true),
            linked_tape: Some(false),
            hidden_panes: true,
            detached_panes: true,
            closed_tabs: true,
            recording: Some(false),
        }
    }

    /// Pane › Recording: a switch (checked while recording) whose command starts or stops it;
    /// no row when nothing live can be recorded.
    #[test]
    fn the_pane_menu_starts_and_stops_recording() {
        let sub = |m: &PaneMenu| pane(m).sub;
        let find = |items: Vec<MenuItem>| items.into_iter().find(|i| i.label == "Recording");
        let off = find(sub(&pane_of(PaneKind::Chart))).expect("a Recording row");
        assert_eq!(
            (off.cmd, off.checked),
            (Some(Command::StartRecording), false)
        );
        let mut m = pane_of(PaneKind::Dom);
        m.recording = Some(true);
        let on = find(sub(&m)).expect("a Recording row");
        assert_eq!((on.cmd, on.checked), (Some(Command::StopRecording), true));
        m.recording = None;
        assert!(find(sub(&m)).is_none(), "nothing live: no row");
    }

    const KINDS: [PaneKind; 7] = [
        PaneKind::Chart,
        PaneKind::Dom,
        PaneKind::Tape,
        PaneKind::Journal,
        PaneKind::Strategies,
        PaneKind::Chain,
        PaneKind::Positions,
    ];

    fn labels(items: &[MenuItem]) -> Vec<&str> {
        items
            .iter()
            .map(|i| if i.is_sep() { "---" } else { i.label.as_str() })
            .collect()
    }

    /// The order section comes first, above a separator, then the approved chart menu as it
    /// was; a dim note stands alone (no order rows when trading is off or there's no price).
    #[test]
    fn the_order_section_tops_the_chart_menu() {
        use crate::chart_trade::{MenuOrder, NO_PRICE_ROW, TRADING_OFF_ROW};
        use lines_core::Price;
        use lines_trading::Side;
        let buy = MenuOrder {
            side: Side::Buy,
            stop: false,
            price: Price(601_000),
        };
        let sell = MenuOrder {
            side: Side::Sell,
            stop: true,
            price: Price(601_000),
        };
        let k = crate::chart_trade::Offer {
            contract: lines_core::ContractKey::monthly(
                lines_feed::catalog::ES,
                2026,
                lines_core::Month::new(12).unwrap(),
            ),
            qty: lines_core::Qty(2),
            bracket: None,
            account: 0,
        };
        let mut m = pane_of(PaneKind::Chart);
        let base = items(&m);
        let plain = labels(&base);
        m.chart.as_mut().unwrap().orders = Some(ChartOrders::Rows {
            header: "At 6010.00 · 2 contracts".into(),
            rows: vec![
                (
                    "▲ Buy limit 2 @ 6010.00".into(),
                    buy,
                    Some("▲ Buy limit 2 @ 6010.00 with bracket".into()),
                ),
                ("▼ Sell stop 2 @ 6010.00".into(), sell, None),
            ],
            offer: k,
        });
        let v = items(&m);
        assert!(places_orders(&v), "it opens with nothing highlighted");
        assert!(!places_orders(&items(&pane_of(PaneKind::Chart))));
        assert!(v[0].title && v[0].cmd.is_none());
        // Each order sends at once; one that may carry a bracket also stages, right below.
        assert_eq!(v[1].cmd, Some(Command::Order(OrderCmd::Place(buy, k))));
        assert_eq!(v[2].cmd, Some(Command::Order(OrderCmd::Stage(buy, k))));
        assert_eq!(v[2].label, "▲ Buy limit 2 @ 6010.00 with bracket");
        assert_eq!(v[3].cmd, Some(Command::Order(OrderCmd::Place(sell, k))));
        assert!(v[4].is_sep());
        assert_eq!(labels(&v[5..]), plain, "the rest unchanged");
        for note in [TRADING_OFF_ROW, NO_PRICE_ROW] {
            m.chart.as_mut().unwrap().orders = Some(ChartOrders::Note(note.into()));
            let v = items(&m);
            assert!(v[0].title && !v[0].selectable(), "{note}: a state label");
            assert!(v[1].is_sep());
            assert!(
                v.iter()
                    .all(|i| !matches!(i.cmd, Some(Command::Order(OrderCmd::Place(..))))),
                "{note}: no order rows"
            );
        }
    }

    #[test]
    fn the_chart_menu_is_the_approved_one() {
        let v = items(&pane_of(PaneKind::Chart));
        assert_eq!(
            labels(&v),
            [
                "Studies…",
                "Timeframe",
                "Show",
                "Trades",
                "---",
                "Order strip",
                "Magnet crosshair",
                "---",
                "Drawings",
                "---",
                "Pane",
                "Link",
                "---",
                "Settings…",
                "All commands…"
            ]
        );
        assert_eq!(v[1].hint, "5m", "a submenu shows its value");
        assert_eq!(v[11].hint, "A");
        assert_eq!(v[10].hint, "Chart");
        assert_eq!(v[3].hint, "Today", "trades show their period");
        assert_eq!(
            labels(&v[3].sub),
            [
                "Show trades",
                "Entry–exit lines",
                "Labels",
                "Hide on all charts",
                "Show on all charts",
                "Account",
                "This pane's account",
                "All accounts",
                "Period",
                "Today",
                "This week",
                "All dates"
            ]
        );
        let show = &v[2].sub;
        assert_eq!(
            labels(show),
            [
                "Levels",
                "Level labels",
                "Labels at",
                "Axis",
                "Column",
                "Chart",
                "Level types…",
                "Studies",
                "Study labels",
                "Strategy markers",
                "Drawing tools",
                "---",
                "Clean chart"
            ]
        );
        assert_eq!(
            labels(&v[10].sub),
            [
                "Split right",
                "Split down",
                "Pop out",
                "---",
                "Show as",
                "Chart",
                "DOM",
                "Time & sales",
                "Positions",
                "---",
                "Chart only",
                "Linked DOM",
                "Linked time & sales",
                "Show hidden panes",
                "Re-attach detached panes",
                "---",
                "Recording",
                "---",
                "New tab",
                "Reopen closed tab",
                "---",
                "Close pane"
            ]
        );
    }

    #[test]
    fn every_menu_keeps_the_rules() {
        for kind in KINDS {
            let v = items(&pane_of(kind));
            let top = v.iter().filter(|i| !i.is_sep()).count();
            assert!(top <= 12, "{kind:?}: {top} top-level rows");
            assert_eq!(v.last().map(|i| i.label.as_str()), Some("All commands…"));
            // Settings… second to last, at this pane's own section, with Settings' key.
            let s = &v[v.len() - 2];
            assert_eq!(s.label, "Settings…", "{kind:?}");
            assert_eq!(s.cmd, Some(Command::SettingsAt(settings_section(kind))));
            assert_eq!(s.hint, crate::commands::hint(Command::Settings));
            assert!(!s.hint.is_empty());
            for i in &v {
                assert!(i.sub.iter().all(|s| s.sub.is_empty()), "one level deep");
                // Destructive rows come last in their list.
                if let Some(at) = i.sub.iter().position(|s| s.destructive) {
                    assert!(
                        i.sub[at..].iter().all(|s| s.destructive || s.is_sep()),
                        "{kind:?}: {} has a row after its destructive one",
                        i.label
                    );
                }
                // No "Hide X / Show X" labels that flip: a checkmark instead.
                for s in std::iter::once(i).chain(&i.sub) {
                    assert!(!s.label.contains("Hide/show"), "{}", s.label);
                    assert!(!s.label.starts_with("Hide strategy"), "{}", s.label);
                }
            }
        }
    }

    #[test]
    fn there_is_never_a_row_per_level_type() {
        let v = items(&pane_of(PaneKind::Chart));
        let all: Vec<&MenuItem> = v.iter().chain(v.iter().flat_map(|i| &i.sub)).collect();
        // One row opens the panel that lists them.
        assert_eq!(
            all.iter().filter(|i| i.label.starts_with("Level")).count(),
            3,
            "Levels, Level labels, Level types…"
        );
    }

    /// Every pane kind's "Settings…" opens at a section that has settings (never an empty page).
    #[test]
    fn every_pane_opens_settings_at_a_section_with_settings() {
        for kind in KINDS {
            let at = settings_section(kind);
            assert!(
                crate::settings_schema::all()
                    .iter()
                    .any(|s| s.section == at),
                "{kind:?}: {at:?} has no settings"
            );
        }
    }

    /// Every pane menu command is also in the palette (or opens it, or the menu itself): a
    /// menu is never the only way to an action.
    #[test]
    fn every_menu_action_is_in_the_palette() {
        let palette = crate::commands::palette();
        for kind in KINDS {
            for i in items(&pane_of(kind)) {
                for s in std::iter::once(&i).chain(&i.sub) {
                    if let Some(cmd) = s.cmd
                        && cmd != Command::Palette
                    {
                        // Settings at a section is the palette's Settings… (same window).
                        let cmd = match cmd {
                            Command::SettingsAt(_) => Command::Settings,
                            c => c,
                        };
                        assert!(
                            palette.contains(&cmd),
                            "{kind:?}: {cmd:?} not in the palette"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_measure_gets_its_own_menu() {
        use crate::chart_draw::{MeasurePart, MeasureShow};
        use crate::pnl::PnlDisplay;
        let mut show = MeasureShow::default();
        show.toggle(MeasurePart::Time);
        let v = object_items(&MenuObject::Measure {
            show,
            pnl: PnlDisplay::Both,
        });
        assert!(v[0].header && v[0].label == "Measure");
        assert_eq!(
            labels(&v),
            [
                "Measure",
                "---",
                "Ticks",
                "Points",
                "P&L / contract",
                "Bars",
                "Time",
                "---",
                "Remove measure"
            ]
        );
        assert!(v[2].checked && !v[6].checked, "Time off, the rest on");
        assert!(v.last().unwrap().destructive);
        // P&L display off: the rows it hides say so.
        let off = object_items(&MenuObject::Measure {
            show,
            pnl: PnlDisplay::Off,
        });
        assert_eq!(labels(&off)[2], "Ticks (P&L display: off)");
        assert_eq!(labels(&off)[4], "P&L / contract (P&L display: off)");
        assert_eq!(labels(&off)[3], "Points");
        // Ticks only: P&L is the hidden one, and the row names the mode.
        let ticks = object_items(&MenuObject::Measure {
            show,
            pnl: PnlDisplay::Ticks,
        });
        assert_eq!(labels(&ticks)[2], "Ticks");
        assert_eq!(labels(&ticks)[4], "P&L / contract (P&L display: ticks)");
        let money = object_items(&MenuObject::Measure {
            show,
            pnl: PnlDisplay::Currency,
        });
        assert_eq!(labels(&money)[2], "Ticks (P&L display: currency)");
        assert_eq!(labels(&money)[4], "P&L / contract");
    }

    #[test]
    fn an_order_and_a_drawing_get_their_own_menus() {
        let o = MenuObject::Order {
            id: 7,
            title: "Stop 2 @ 7562.00".into(),
            stop: true,
        };
        let v = object_items(&o);
        assert!(v[0].header && v[0].label == "Stop 2 @ 7562.00");
        assert_eq!(
            labels(&v),
            [
                "Stop 2 @ 7562.00",
                "---",
                "Move all stops to breakeven",
                "Change size…",
                "---",
                "Cancel order"
            ]
        );
        assert!(v.last().unwrap().destructive);
        assert_eq!(
            v.last().unwrap().cmd,
            Some(Command::Order(OrderCmd::Cancel(7)))
        );
        let target = object_items(&MenuObject::Order {
            id: 8,
            title: "Target 2 @ 7580.00".into(),
            stop: false,
        });
        assert!(
            !labels(&target).contains(&"Move all stops to breakeven"),
            "only a stop"
        );
        let d = object_items(&MenuObject::Drawing {
            title: "Trend line".into(),
        });
        assert_eq!(labels(&d), ["Trend line", "---", "Style…", "---", "Delete"]);
        assert!(d.last().unwrap().destructive);
    }

    #[test]
    fn switches_show_a_box_and_sets_a_check() {
        let v = items(&pane_of(PaneKind::Chart));
        let strip = v.iter().find(|i| i.label == "Order strip").unwrap();
        assert!(strip.toggle && !strip.checked, "off, and shown as a switch");
        let pane = v.iter().find(|i| i.label == "Pane").unwrap();
        let chart = pane.sub.iter().find(|i| i.label == "Chart").unwrap();
        assert!(!chart.toggle && chart.checked, "one of a set: a check");
        let link = v.iter().find(|i| i.label == "Link").unwrap();
        assert!(link.sub.iter().all(|i| !i.toggle));
        let show = v.iter().find(|i| i.label == "Show").unwrap();
        // Switches, and where the labels go: one of a set, a check on the one in force.
        let at = |i: &&MenuItem| matches!(i.cmd, Some(Command::LevelLabelPlace(_)));
        assert!(
            show.sub
                .iter()
                .filter(|i| i.cmd.is_some() && !at(i))
                .all(|i| i.toggle || i.label.ends_with('…'))
        );
        let places: Vec<&MenuItem> = show.sub.iter().filter(at).collect();
        assert_eq!(places.len(), 3);
        assert!(places.iter().all(|i| !i.toggle));
        assert_eq!(places.iter().filter(|i| i.checked).count(), 1);
    }

    /// With a Volume, Show has its placement as a switch: on the chart (on) or in a pane;
    /// its command moves it the other way. Without one, no row.
    #[test]
    fn volume_placement_is_a_switch_in_show() {
        let show = |volume| {
            let mut c = chart();
            c.volume = volume;
            let v = items(&PaneMenu {
                chart: Some(c),
                ..pane_of(PaneKind::Chart)
            });
            v.into_iter().find(|i| i.label == "Show").unwrap().sub
        };
        assert!(!show(None).iter().any(|i| i.label.starts_with("Volume")));
        for overlay in [false, true] {
            let sub = show(Some(overlay));
            let row = sub
                .iter()
                .find(|i| i.label == "Volume on the chart")
                .unwrap();
            assert!(row.toggle);
            assert_eq!(row.checked, overlay);
            assert_eq!(row.cmd, Some(Command::VolumeOverlay(!overlay)));
        }
    }

    #[test]
    fn the_tab_menu_keeps_the_rules() {
        let m = TabMenu {
            tabs: 2,
            pinned: false,
            closable_others: 1,
            closed_tabs: true,
            crosshair_sync: true,
            crosshair_sync_price: false,
            perf_stats: false,
            latency_hud: false,
        };
        let v = tab_items(&m);
        assert!(v.iter().filter(|i| !i.is_sep()).count() <= 12);
        let last = v.last().unwrap();
        assert!(last.destructive && last.cmd == Some(Command::CloseTab));
        // Rename and Pin first; Close others, destructive, just before Close tab.
        assert_eq!(v[0].cmd, Some(Command::RenameTab));
        assert!(v[1].toggle && v[1].cmd == Some(Command::TogglePinTab) && !v[1].checked);
        let others = &v[v.len() - 2];
        assert!(others.destructive && others.cmd == Some(Command::CloseOtherTabs));
        // Nothing to close: no Close others; a pinned tab's Pin is checked.
        let v = tab_items(&TabMenu {
            closable_others: 0,
            pinned: true,
            ..m
        });
        assert!(!v.iter().any(|i| i.cmd == Some(Command::CloseOtherTabs)));
        assert!(v[1].checked);
        let palette = crate::commands::palette();
        for i in &v {
            for s in std::iter::once(i).chain(&i.sub) {
                if let Some(cmd) = s.cmd
                    && cmd != Command::Palette
                {
                    assert!(palette.contains(&cmd), "{cmd:?} not in the palette");
                }
            }
        }
    }

    #[test]
    fn breakeven_is_offered_for_the_stops_it_moves() {
        assert!(offers_breakeven(true, true, Some(2)), "a long's sell stop");
        assert!(
            offers_breakeven(true, false, Some(-1)),
            "a short's buy stop"
        );
        assert!(!offers_breakeven(true, true, None), "flat: nothing to move");
        assert!(!offers_breakeven(true, true, Some(0)));
        assert!(
            !offers_breakeven(true, false, Some(2)),
            "a buy stop adds to a long"
        );
        assert!(
            !offers_breakeven(false, true, Some(2)),
            "a limit is no stop"
        );
    }

    #[test]
    fn screenshot_menus_parse() {
        assert!(parse_shot_menu("measure@700,300").unwrap().measure);
        assert_eq!(
            parse_shot_menu("pane:Show@1400,300"),
            Ok(ShotMenu {
                order: false,
                measure: false,
                tab: false,
                sub: Some("Show".into()),
                at: (1400.0, 300.0)
            })
        );
        assert_eq!(
            parse_shot_menu("order").map(|m| (m.order, m.at)),
            Ok((true, (200.0, 120.0)))
        );
        assert_eq!(parse_shot_menu("tab").map(|m| m.tab), Ok(true));
        assert!(parse_shot_menu("tabs").is_err());
        assert!(parse_shot_menu("pane@1,").is_err());
        assert!(parse_shot_menu("pane@-1,4").is_err());
    }

    #[test]
    fn a_timeframe_off_the_quick_list_is_listed_and_checked() {
        let mut c = chart();
        c.timeframe = Timeframe::Ticks(500);
        c.on_ticks = true;
        let tf = timeframe(&c);
        assert_eq!(tf.hint, "500t");
        assert!(tf.sub.iter().any(|i| i.checked && i.label == "500t"));
        assert!(
            tf.sub
                .iter()
                .any(|i| i.cmd == Some(Command::TickBarsContinuous))
        );
        assert!(tf.sub.iter().filter(|i| !i.is_sep()).count() <= 13);
    }
}
