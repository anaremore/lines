//! What a pane's context menu holds, by the menu rules (`docs/design.md`, "Menus";
//! `crate::menu`): about 12 top-level rows; sets as submenus one level deep, each showing its
//! value (Timeframe ›, Show ›, Drawings ›, Pane ›, Link ›); show/hide as checkmarks; never a
//! row per instance; destructive rows last; "All commands…" at the end. An object under the
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
    /// Trade-built bars (ticks, volume, range): their session switch shows.
    pub on_ticks: bool,
    pub tick_continuous: bool,
    /// The chart has level sources: (levels shown, labels shown).
    pub levels: Option<(bool, bool)>,
    pub studies_shown: bool,
    pub strategy_marks_shown: bool,
    pub clean: bool,
    pub order_strip: bool,
    pub magnet: bool,
    pub drawings_shown: bool,
    pub drawing_selected: bool,
    pub symbol: String,
    pub chart_only: bool,
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
}

/// The thing under the pointer at a right click.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuObject {
    /// A working order's tag: its id, its tag's text, and whether it is a protective stop of
    /// an open position (Move to breakeven).
    Order { id: u64, title: String, stop: bool },
    /// A drawing (now the selection): what it is.
    Drawing { title: String },
}

/// A menu a screenshot opens (`--context-menu`, decision 0009): the focused pane's, or the
/// focused chart's first working order's; with one submenu open; at a window point.
#[derive(Clone, Debug, PartialEq)]
pub struct ShotMenu {
    pub order: bool,
    pub sub: Option<String>,
    pub at: (f32, f32),
}

/// `pane` or `order`, then `:Label` to open the submenu of that row, then `@X,Y` (logical px;
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
    let order = match what {
        "pane" => false,
        "order" => true,
        other => return Err(format!("{other}: expected pane or order")),
    };
    Ok(ShotMenu { order, sub, at })
}

fn row(label: &str, cmd: Command) -> MenuItem {
    MenuItem::named(label, cmd, false)
}

fn toggle(label: &str, cmd: Command, on: bool) -> MenuItem {
    MenuItem::named(label, cmd, on)
}

/// An object's menu: its name and value, what can be done to it, the destructive row last.
pub fn object_items(o: &MenuObject) -> Vec<MenuItem> {
    match o {
        MenuObject::Order { id, title, stop } => {
            let mut v = vec![MenuItem::header(title)];
            if *stop {
                // The position's stops, through the modify path (`TradeCmd::Breakeven`).
                v.push(row(
                    "Move to breakeven",
                    Command::Order(OrderCmd::Breakeven),
                ));
            }
            v.push(row("Change size…", Command::Order(OrderCmd::Resize(*id))));
            v.push(MenuItem::sep());
            v.push(row("Cancel order", Command::Order(OrderCmd::Cancel(*id))).destructive());
            v
        }
        MenuObject::Drawing { title } => vec![
            MenuItem::header(title),
            row("Style…", Command::Draw(DrawCmd::Style)),
            MenuItem::sep(),
            row("Delete", Command::Draw(DrawCmd::DeleteSelected)).destructive(),
        ],
    }
}

/// A pane's menu.
pub fn items(m: &PaneMenu) -> Vec<MenuItem> {
    let mut v = Vec::new();
    if let Some(c) = &m.chart {
        v.push(row("Studies…", Command::Studies));
        v.push(timeframe(c));
        v.push(chart_show(c));
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
                    hint: format!("{:.1} s", f64::from(d.half_life_ms) / 1000.0),
                    ..row("Trail half-life (next)", Command::DomTrailHalfLife)
                },
            ],
        ));
        v.push(MenuItem::submenu(
            "P&L",
            &capitalized(m.pnl.label()),
            PnlDisplay::ALL
                .iter()
                .map(|&p| toggle(&capitalized(p.label()), Command::SetPnl(p), p == m.pnl))
                .collect(),
        ));
        v.push(MenuItem::sep());
    }
    if let Some(j) = &m.journal {
        let mut show: Vec<MenuItem> = JournalGroup::ALL
            .iter()
            .map(|&g| toggle(g.label(), Command::JournalGroup(g), g == j.group))
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
    v.push(row("All commands…", Command::Palette));
    v
}

fn timeframe(c: &ChartMenu) -> MenuItem {
    let tf = c.timeframe;
    let mut sub: Vec<MenuItem> = QUICK_TIMEFRAMES
        .iter()
        .map(|&t| toggle(&t.to_string(), Command::Timeframe(t), t == tf))
        .collect();
    if !QUICK_TIMEFRAMES.contains(&tf) {
        sub.push(toggle(&tf.to_string(), Command::Timeframe(tf), true));
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
    if let Some((on, labels)) = c.levels {
        sub.push(toggle("Levels", Command::ToggleLevels, on));
        sub.push(toggle("Level labels", Command::ToggleLevelLabels, labels));
        // TODO(level-styles, workspaces thread 4c0a): open the per-type level style panel once it
        // merges. Until then the studies menu, which lists each source's types with their
        // switches.
        sub.push(row("Level types…", Command::Studies));
    }
    sub.push(toggle("Studies", Command::ToggleStudies, c.studies_shown));
    sub.push(toggle(
        "Strategy markers",
        Command::Strategy(StrategyCmd::ToggleMarks),
        c.strategy_marks_shown,
    ));
    sub.push(MenuItem::sep());
    sub.push(toggle("Clean chart", Command::CleanChart, c.clean));
    MenuItem::submenu("Show", "", sub)
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
        MenuItem::header("Show"),
    ];
    for k in [PaneKind::Chart, PaneKind::Dom, PaneKind::Tape] {
        sub.push(toggle(k.label(), Command::SetKind(k), k == m.kind));
    }
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
    sub.push(MenuItem::of(Command::NewTab, false));
    if m.closed_tabs {
        sub.push(MenuItem::of(Command::ReopenTab, false));
    }
    sub.push(MenuItem::sep());
    if m.chart.is_none() {
        sub.push(row(&format!("Hide {}", m.kind.label()), Command::HidePane));
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
        let mut r = toggle(&label, Command::SetLink(g), g == l);
        if g.is_none() {
            r.hint = crate::commands::hint(Command::CycleLink);
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
            timeframe: Timeframe::Minutes(5),
            on_ticks: false,
            tick_continuous: false,
            levels: Some((true, true)),
            studies_shown: true,
            strategy_marks_shown: true,
            clean: false,
            order_strip: false,
            magnet: true,
            drawings_shown: true,
            drawing_selected: true,
            symbol: "ES".into(),
            chart_only: false,
        }
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
        }
    }

    const KINDS: [PaneKind; 6] = [
        PaneKind::Chart,
        PaneKind::Dom,
        PaneKind::Tape,
        PaneKind::Journal,
        PaneKind::Strategies,
        PaneKind::Chain,
    ];

    fn labels(items: &[MenuItem]) -> Vec<&str> {
        items
            .iter()
            .map(|i| if i.is_sep() { "---" } else { i.label.as_str() })
            .collect()
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
                "---",
                "Order strip",
                "Magnet crosshair",
                "---",
                "Drawings",
                "---",
                "Pane",
                "Link",
                "---",
                "All commands…"
            ]
        );
        assert_eq!(v[1].hint, "5m", "a submenu shows its value");
        assert_eq!(v[10].hint, "A");
        assert_eq!(v[9].hint, "Chart");
        let show = &v[2].sub;
        assert_eq!(
            labels(show),
            [
                "Levels",
                "Level labels",
                "Level types…",
                "Studies",
                "Strategy markers",
                "---",
                "Clean chart"
            ]
        );
        assert_eq!(
            labels(&v[9].sub),
            [
                "Split right",
                "Split down",
                "Pop out",
                "Show",
                "Chart",
                "DOM",
                "Time & sales",
                "Chart only",
                "Linked DOM",
                "Linked time & sales",
                "Show hidden panes",
                "Re-attach detached panes",
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
                "Move to breakeven",
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
            !labels(&target).contains(&"Move to breakeven"),
            "only a stop"
        );
        let d = object_items(&MenuObject::Drawing {
            title: "Trend line".into(),
        });
        assert_eq!(labels(&d), ["Trend line", "Style…", "---", "Delete"]);
        assert!(d.last().unwrap().destructive);
    }

    #[test]
    fn screenshot_menus_parse() {
        assert_eq!(
            parse_shot_menu("pane:Show@1400,300"),
            Ok(ShotMenu {
                order: false,
                sub: Some("Show".into()),
                at: (1400.0, 300.0)
            })
        );
        assert_eq!(
            parse_shot_menu("order").map(|m| (m.order, m.at)),
            Ok((true, (200.0, 120.0)))
        );
        assert!(parse_shot_menu("tab").is_err());
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
