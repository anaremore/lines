//! Workspace commands and the keys bound to them (decision 0018: every action is a typed
//! command; hotkeys, menus, header controls and the command palette are just ways to issue
//! one). One table ([`BINDINGS`]) gives both the keymap and the hints shown in menus and the
//! palette, so they can't disagree. Pure and tested; the workspace executes the commands.
//!
//! "Ctrl" means Cmd on macOS (GPUI's `secondary` modifier).

use gpui::Keystroke;
use lines_core::Timeframe;

use crate::layout::{Axis, Direction, LinkGroup, PaneKind, Preset, link_letter};
use crate::pane_columns::{Column, ColumnCmd, DomCol, TapeCol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Move keyboard focus to the neighbouring pane.
    Focus(Direction),
    /// Split the focused pane; the new pane (same kind, symbol and link) goes right or below.
    Split(Axis),
    ClosePane,
    /// Change the focused pane's kind, keeping its symbol and link.
    SetKind(PaneKind),
    /// Cycle the focused pane's link group: unlinked → A → B → C → D.
    CycleLink,
    SetLink(LinkGroup),
    /// Replace the active tab's layout with a preset, on the focused pane's symbol.
    Preset(Preset),
    /// Hide the focused DOM or time & sales.
    HidePane,
    /// Hide or show the DOMs (or tapes) that go with the focused pane (`crate::tabstate`).
    Toggle(PaneKind),
    /// The focused chart's "chart only": hide or restore its linked DOM and time & sales.
    ChartOnly,
    /// Show every hidden pane in the active tab.
    ShowHidden,
    /// Put the focused pane in its own window (in that window: re-attach it).
    PopOut,
    /// Return a detached pane to its previous spot (from a main window: all of the tab's).
    Reattach,
    /// Dock a detached pane beside the chart that last had focus.
    DockBesideChart,
    NewTab,
    CloseTab,
    /// Reopen the most recently closed tab.
    ReopenTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    /// Move the active tab into a new window.
    TabToNewWindow,
    ToggleCrosshairSync,
    /// Crosshair sync carries the price too, between charts of the same product.
    ToggleCrosshairSyncPrice,
    /// The focused chart's magnet crosshair (snap to the bar's open/high/low/close).
    ToggleMagnet,
    /// The focused pane's context menu, from the keyboard.
    Menu,
    /// Every command, searchable.
    Palette,
    SaveAs,
    Open,
    /// The focused chart: hide (or show) its study legends and tags; studies stay drawn.
    ToggleStudyLabels,
    /// The focused chart: hide (or show) all its studies, keeping their configuration.
    ToggleStudies,
    /// The focused chart without legends or study tags, and the key-hint bar hidden; again
    /// restores both.
    CleanChart,
    /// Show or hide the key-hint bar (a workspace preference; shown by default).
    ToggleHints,
    /// A column action on the focused DOM or time & sales (`crate::pane_columns`).
    Columns(ColumnCmd),
    /// The focused unavailable pane shows the source's own symbol (one click, also Enter).
    ShowSourceHere,
    /// Every unavailable pane in the tab shows the source's own symbol.
    ShowSourceInTab,
    /// Write a diagnostics zip (logs, versions, redacted settings, feed and frame stats).
    ExportDiagnostics,
    /// The focused chart: hide (or show) all external levels (decision 0022).
    ToggleLevels,
    /// The focused chart: hide (or show) level labels and level legend rows.
    ToggleLevelLabels,
    /// The focused chart: hide (or show) one level source (index into its sources).
    ToggleLevelSource(u8),
    /// The focused chart: hide (or show) one kind of one source (indices).
    ToggleLevelKind(u8, u8),
    /// Show or hide the chart headers' performance stats (feed msgs/s, updates/s, paints/s,
    /// paint time): a workspace preference, off by default. No key: palette and menu only.
    TogglePerfStats,
    /// The focused DOM's price trail on/off (its column comes with it). No key: pane keys
    /// are reserved; palette and the DOM's pane menu.
    DomTrail,
    /// The focused DOM's trail half-life: the next preset (0.5, 1, 1.5, 3, 5 s).
    DomTrailHalfLife,
    /// The focused chart's studies menu (VWAPs, volume profile, levels; templates).
    Studies,
    /// Trading on the focused DOM or chart (roadmap 7): its account and contract.
    Trade(TradeCmd),
    /// Chart trading (roadmap 8): an order at a price from the chart's menu (price units).
    ChartOrder(crate::chart_trade::OrderPick, i64),
    /// The focused chart's compact trading bar: show or hide.
    ToggleChartBar,
    /// The focused chart's fill marks: show or hide.
    ToggleFillMarks,
    /// The app-wide P&L display: the next mode (off, ticks, currency, both).
    CyclePnl,
    /// The app-wide P&L display: this mode.
    SetPnl(crate::pnl::PnlDisplay),
    /// The focused chart's timeframe (roadmap 9). No key: typed on the chart (`5`, `1h` +
    /// Enter), the header chip, the pane menu and the palette.
    Timeframe(Timeframe),
    /// The focused chart's timeframe list.
    TimeframeMenu,
}

/// Trading actions of the focused pane (`crate::trade_bar::run`). Flatten and Cancel All act
/// on the pane's contract and are never blocked by arming, risk or prop rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeCmd {
    /// Cancel the contract's orders and close its position at market.
    Flatten,
    /// Cancel the contract's working orders.
    CancelAll,
    /// Arm or disarm one-click trading on the DOM.
    ToggleArm,
    /// Trade on the next account.
    NextAccount,
    QtyUp,
    QtyDown,
    /// Cycle the bracket preset (none, 8/16, 12/24, 20/40 ticks).
    NextBracket,
    /// The DOM header's order buttons (`dom_trade::QuickOrder`): gated by arming like ladder
    /// clicks.
    BuyMarket,
    SellMarket,
    JoinBid,
    JoinAsk,
    Reverse,
}

impl TradeCmd {
    pub const ALL: [TradeCmd; 12] = [
        TradeCmd::Flatten,
        TradeCmd::CancelAll,
        TradeCmd::ToggleArm,
        TradeCmd::NextAccount,
        TradeCmd::QtyUp,
        TradeCmd::QtyDown,
        TradeCmd::NextBracket,
        TradeCmd::BuyMarket,
        TradeCmd::SellMarket,
        TradeCmd::JoinBid,
        TradeCmd::JoinAsk,
        TradeCmd::Reverse,
    ];

    /// The order a header button sends, for the order commands.
    pub fn quick(self) -> Option<crate::dom_trade::QuickOrder> {
        use crate::dom_trade::QuickOrder as Q;
        Some(match self {
            TradeCmd::BuyMarket => Q::BuyMarket,
            TradeCmd::SellMarket => Q::SellMarket,
            TradeCmd::JoinBid => Q::JoinBid,
            TradeCmd::JoinAsk => Q::JoinAsk,
            TradeCmd::Reverse => Q::Reverse,
            _ => return None,
        })
    }
}

/// A key binding: modifiers plus GPUI's key name.
pub struct Binding {
    pub cmd: Command,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: &'static str,
}

const fn b(cmd: Command, ctrl: bool, shift: bool, alt: bool, key: &'static str) -> Binding {
    Binding {
        cmd,
        ctrl,
        shift,
        alt,
        key,
    }
}

/// Every key binding (the first one for a command is its hint). Focus (Alt+arrows) and tab
/// selection (Ctrl+1..9) are patterns, handled in [`command_for`].
pub const BINDINGS: &[Binding] = &[
    b(Command::Split(Axis::Row), true, true, false, "right"),
    b(Command::Split(Axis::Column), true, true, false, "down"),
    b(Command::ClosePane, true, false, false, "w"),
    b(Command::Toggle(PaneKind::Dom), true, true, false, "d"),
    b(Command::Toggle(PaneKind::Tape), true, true, false, "e"),
    b(Command::ChartOnly, true, true, false, "c"),
    b(Command::ShowHidden, true, true, false, "h"),
    b(Command::PopOut, true, true, false, "o"),
    b(Command::DockBesideChart, true, true, false, "b"),
    b(Command::NewTab, true, false, false, "t"),
    b(Command::CloseTab, true, false, false, "f4"),
    b(Command::CloseTab, true, true, false, "w"),
    b(Command::ReopenTab, true, true, false, "t"),
    b(Command::NextTab, true, false, false, "tab"),
    b(Command::NextTab, true, false, false, "pagedown"),
    b(Command::PrevTab, true, true, false, "tab"),
    b(Command::PrevTab, true, false, false, "pageup"),
    b(Command::TabToNewWindow, true, true, false, "n"),
    b(Command::CycleLink, true, false, false, "l"),
    b(Command::ToggleCrosshairSync, true, true, false, "x"),
    b(Command::ToggleMagnet, true, true, false, "g"),
    b(Command::Menu, false, true, false, "f10"),
    b(Command::Menu, true, false, false, "."),
    b(Command::Palette, true, true, false, "p"),
    b(Command::SaveAs, true, true, false, "s"),
    b(Command::Open, true, false, false, "o"),
    b(Command::ToggleStudyLabels, true, true, false, "l"),
    b(Command::ToggleStudies, true, true, false, "i"),
    b(Command::CleanChart, true, true, false, "m"),
    b(Command::ToggleHints, false, false, false, "f1"),
    b(Command::Studies, true, false, false, "i"),
    // Trading (roadmap 7): always Ctrl+Shift or Ctrl+Alt, so no stray key can trade.
    b(Command::Trade(TradeCmd::Flatten), true, true, false, "f"),
    b(Command::Trade(TradeCmd::CancelAll), true, true, false, "a"),
    b(
        Command::Trade(TradeCmd::ToggleArm),
        true,
        true,
        false,
        "space",
    ),
    b(
        Command::Trade(TradeCmd::NextAccount),
        true,
        true,
        false,
        "u",
    ),
    b(Command::CyclePnl, true, false, true, "p"),
    // Header order buttons: Ctrl+Alt, never a bare or Shift-only key.
    b(Command::Trade(TradeCmd::BuyMarket), true, false, true, "b"),
    b(Command::Trade(TradeCmd::SellMarket), true, false, true, "s"),
    b(Command::Trade(TradeCmd::JoinBid), true, false, true, "j"),
    b(Command::Trade(TradeCmd::JoinAsk), true, false, true, "k"),
    b(Command::Trade(TradeCmd::Reverse), true, false, true, "r"),
    b(Command::ToggleChartBar, true, false, true, "t"),
    b(Command::ToggleFillMarks, true, false, true, "f"),
    b(Command::Trade(TradeCmd::QtyUp), true, false, true, "up"),
    b(Command::Trade(TradeCmd::QtyDown), true, false, true, "down"),
    b(
        Command::Trade(TradeCmd::NextBracket),
        true,
        true,
        false,
        "k",
    ),
];

/// The command bound to a keystroke, if any.
pub fn command_for(k: &Keystroke) -> Option<Command> {
    let m = k.modifiers;
    // GPUI's `secondary`: Cmd on macOS, Ctrl elsewhere.
    let mac = cfg!(target_os = "macos");
    let ctrl = if mac { m.platform } else { m.control };
    // The modifier that isn't "Ctrl" on this platform (Ctrl on macOS, Win elsewhere) is never
    // part of a binding.
    if (mac && m.control) || (!mac && m.platform) {
        return None;
    }
    let key = k.key.as_str();
    if !ctrl && !m.shift && m.alt {
        return Some(Command::Focus(match key {
            "left" => Direction::Left,
            "right" => Direction::Right,
            "up" => Direction::Up,
            "down" => Direction::Down,
            _ => return None,
        }));
    }
    if ctrl && !m.shift && !m.alt && key.len() == 1 && (b'1'..=b'9').contains(&key.as_bytes()[0]) {
        return Some(Command::SelectTab((key.as_bytes()[0] - b'1') as usize));
    }
    BINDINGS
        .iter()
        .find(|b| b.ctrl == ctrl && b.shift == m.shift && b.alt == m.alt && b.key == key)
        .map(|b| b.cmd)
}

/// The hint shown for a command in menus and the palette: its first binding.
pub fn hint(c: Command) -> String {
    let mac = cfg!(target_os = "macos");
    match c {
        Command::Focus(_) => return if mac { "⌥arrows" } else { "Alt+arrows" }.into(),
        Command::SelectTab(i) if i < 9 => {
            return if mac {
                format!("⌘{}", i + 1)
            } else {
                format!("Ctrl+{}", i + 1)
            };
        }
        _ => {}
    }
    let Some(b) = BINDINGS.iter().find(|b| b.cmd == c) else {
        return String::new();
    };
    let key = match b.key {
        "right" => "→".to_owned(),
        "down" => "↓".to_owned(),
        "tab" => "Tab".to_owned(),
        "pagedown" => "PgDn".to_owned(),
        "pageup" => "PgUp".to_owned(),
        "up" => "↑".to_owned(),
        "space" => "Space".to_owned(),
        k => k.to_uppercase(),
    };
    if mac {
        let mut s = String::new();
        if b.alt {
            s.push('⌥');
        }
        if b.shift {
            s.push('⇧');
        }
        if b.ctrl {
            s.push('⌘');
        }
        s + &key
    } else {
        let mut s = String::new();
        if b.ctrl {
            s.push_str("Ctrl+");
        }
        if b.alt {
            s.push_str("Alt+");
        }
        if b.shift {
            s.push_str("Shift+");
        }
        s + &key
    }
}

/// What a command is called in menus and the palette.
pub fn label(c: Command) -> String {
    match c {
        Command::Focus(d) => format!("Focus pane {}", format!("{d:?}").to_lowercase()),
        Command::Split(Axis::Row) => "Split right".into(),
        Command::Split(Axis::Column) => "Split down".into(),
        Command::ClosePane => "Close pane".into(),
        Command::SetKind(k) => format!("Change pane to {}", k.label()),
        Command::CycleLink => "Next link group".into(),
        Command::SetLink(None) => "Unlink pane".into(),
        Command::SetLink(l) => format!("Link pane to group {}", link_letter(l)),
        Command::Preset(p) => format!("Layout: {}", p.label()),
        Command::HidePane => "Hide pane".into(),
        Command::Toggle(PaneKind::Dom) => "Hide/show DOM".into(),
        Command::Toggle(PaneKind::Tape) => "Hide/show time & sales".into(),
        Command::Toggle(PaneKind::Chart) => "Hide/show chart".into(),
        Command::ChartOnly => "Chart only (hide/restore linked DOM and T&S)".into(),
        Command::ShowHidden => "Show hidden panes".into(),
        Command::PopOut => "Pop out pane into its own window".into(),
        Command::Reattach => "Re-attach detached panes".into(),
        Command::DockBesideChart => "Dock beside focused chart".into(),
        Command::NewTab => "New tab".into(),
        Command::CloseTab => "Close tab".into(),
        Command::ReopenTab => "Reopen closed tab".into(),
        Command::NextTab => "Next tab".into(),
        Command::PrevTab => "Previous tab".into(),
        Command::SelectTab(i) => format!("Go to tab {}", i + 1),
        Command::TabToNewWindow => "Move tab to new window".into(),
        Command::ToggleCrosshairSync => "Crosshair sync on/off (within link groups)".into(),
        Command::ToggleCrosshairSyncPrice => {
            "Crosshair sync: price too, same product (on/off)".into()
        }
        Command::ToggleMagnet => "Chart: magnet crosshair, snap to OHLC (on/off)".into(),
        Command::Menu => "Pane menu".into(),
        Command::Palette => "Command palette".into(),
        Command::SaveAs => "Save workspace as…".into(),
        Command::Open => "Open workspace…".into(),
        Command::ExportDiagnostics => "Export diagnostics…".into(),
        Command::ToggleStudyLabels => "Chart: hide/show study labels".into(),
        Command::ToggleStudies => "Chart: hide/show studies".into(),
        Command::CleanChart => "Clean chart (no study labels, no key hints)".into(),
        Command::ToggleHints => "Hide/show key hints bar".into(),
        Command::Columns(ColumnCmd::Toggle(c)) => format!("Show/hide {}", c.label()),
        Command::Columns(ColumnCmd::AutoFit(c)) => format!("Fit {} to content", c.label()),
        Command::Columns(ColumnCmd::AutoFitAll) => "Fit columns to content".into(),
        Command::Columns(ColumnCmd::Reset) => "Reset columns".into(),
        Command::ShowSourceHere => "Show the source's symbol in this pane".into(),
        Command::ShowSourceInTab => "Switch all unavailable panes to the source's symbol".into(),
        Command::Columns(ColumnCmd::Width(c, w)) => format!("Set {} width to {w} px", c.label()),
        Command::ToggleLevels => "Chart: hide/show levels".into(),
        Command::ToggleLevelLabels => "Chart: hide/show level labels".into(),
        Command::ToggleLevelSource(i) => format!("Chart: hide/show level source {}", i + 1),
        Command::ToggleLevelKind(i, k) => {
            format!("Chart: hide/show level kind {} of source {}", k + 1, i + 1)
        }
        Command::TogglePerfStats => "Performance stats on/off (chart headers)".into(),
        Command::DomTrail => "DOM: price trail on/off".into(),
        Command::DomTrailHalfLife => "DOM: price trail half-life (next)".into(),
        Command::Studies => "Chart: studies (VWAP, volume profile, levels, templates)…".into(),
        Command::CyclePnl => "P&L display: next (off, ticks, currency, both)".into(),
        Command::ChartOrder(pick, _) => format!("Chart: {}", pick.label()),
        Command::ToggleChartBar => "Chart: show/hide trading bar".into(),
        Command::ToggleFillMarks => "Chart: show/hide fill marks".into(),
        Command::SetPnl(m) => format!("P&L display: {}", m.label()),
        Command::Timeframe(tf) => format!("Timeframe: {tf}"),
        Command::TimeframeMenu => "Chart: timeframe…".into(),
        Command::Trade(t) => match t {
            TradeCmd::Flatten => "Trade: Flatten (cancel orders, close position)",
            TradeCmd::CancelAll => "Trade: Cancel all orders",
            TradeCmd::ToggleArm => "Trade: Arm/disarm one-click DOM trading",
            TradeCmd::NextAccount => "Trade: Next account",
            TradeCmd::QtyUp => "Trade: Quantity up",
            TradeCmd::QtyDown => "Trade: Quantity down",
            TradeCmd::NextBracket => "Trade: Next bracket preset",
            TradeCmd::BuyMarket => "Trade: Buy market (selected quantity)",
            TradeCmd::SellMarket => "Trade: Sell market (selected quantity)",
            TradeCmd::JoinBid => "Trade: Join bid (buy limit at the best bid)",
            TradeCmd::JoinAsk => "Trade: Join ask (sell limit at the best ask)",
            TradeCmd::Reverse => "Trade: Reverse position (market, twice the size)",
        }
        .into(),
    }
}

/// Every command the palette offers (it runs on the focused pane, tab and window).
pub fn palette() -> Vec<Command> {
    let mut v = vec![
        Command::Split(Axis::Row),
        Command::Split(Axis::Column),
        Command::ClosePane,
        Command::Toggle(PaneKind::Dom),
        Command::Toggle(PaneKind::Tape),
        Command::ChartOnly,
        Command::HidePane,
        Command::ShowHidden,
        Command::PopOut,
        Command::Reattach,
        Command::NewTab,
        Command::CloseTab,
        Command::ReopenTab,
        Command::NextTab,
        Command::PrevTab,
        Command::TabToNewWindow,
        Command::CycleLink,
    ];
    v.extend([None, Some(0), Some(1), Some(2), Some(3)].map(Command::SetLink));
    v.extend([PaneKind::Chart, PaneKind::Dom, PaneKind::Tape].map(Command::SetKind));
    v.extend(Preset::ALL.map(Command::Preset));
    v.extend([
        Command::ToggleCrosshairSync,
        Command::ToggleCrosshairSyncPrice,
        Command::ToggleMagnet,
        Command::ToggleStudyLabels,
        Command::ToggleStudies,
        Command::CleanChart,
        Command::ToggleHints,
        Command::ToggleLevels,
        Command::ToggleLevelLabels,
        Command::TogglePerfStats,
        Command::DomTrail,
        Command::DomTrailHalfLife,
        Command::Studies,
        Command::TimeframeMenu,
        Command::SaveAs,
        Command::Open,
        Command::Menu,
    ]);
    // Columns of the focused DOM or time & sales.
    v.extend(
        DomCol::ALL
            .map(|c| Command::Columns(ColumnCmd::Toggle(Column::Dom(c))))
            .into_iter()
            .chain(TapeCol::ALL.map(|c| Command::Columns(ColumnCmd::Toggle(Column::Tape(c)))))
            .chain([
                Command::Columns(ColumnCmd::AutoFitAll),
                Command::Columns(ColumnCmd::Reset),
            ]),
    );
    v.extend([Command::ShowSourceHere, Command::ShowSourceInTab]);
    v.extend(TradeCmd::ALL.map(Command::Trade));
    v.push(Command::CyclePnl);
    v.extend([Command::ToggleChartBar, Command::ToggleFillMarks]);
    v.extend(crate::pnl::PnlDisplay::ALL.map(Command::SetPnl));
    v.push(Command::ExportDiagnostics);
    v.extend(Timeframe::ALL.map(Command::Timeframe));
    v
}

/// Palette entries matching `query`: every word of it appears in the label (any order, any
/// case). Order is the palette's.
pub fn filter(query: &str, commands: &[Command]) -> Vec<Command> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    commands
        .iter()
        .copied()
        .filter(|c| {
            let l = label(*c).to_lowercase();
            words.iter().all(|w| l.contains(w.as_str()))
        })
        .collect()
}

/// The workspace's key hints for the status line.
pub fn workspace_hints() -> &'static str {
    if cfg!(target_os = "macos") {
        "⇧⌘P: all commands  ·  ⌘T: new tab  ·  right-click: pane menu  ·  F1: hide hints"
    } else {
        "Ctrl+Shift+P: all commands  ·  Ctrl+T: new tab  ·  right-click: pane menu  ·  F1: hide hints"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Option<Command> {
        // `parse` wants a `'static` str; leaking a few test strings is fine.
        command_for(&Keystroke::parse(Box::leak(s.to_owned().into_boxed_str())).unwrap())
    }

    fn ctrl() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        }
    }

    #[test]
    fn bindings() {
        let c = |rest: &str| cmd(&format!("{}-{rest}", ctrl()));
        assert_eq!(cmd("alt-left"), Some(Command::Focus(Direction::Left)));
        assert_eq!(cmd("alt-down"), Some(Command::Focus(Direction::Down)));
        assert_eq!(c("shift-right"), Some(Command::Split(Axis::Row)));
        assert_eq!(c("shift-down"), Some(Command::Split(Axis::Column)));
        assert_eq!(c("w"), Some(Command::ClosePane));
        assert_eq!(c("shift-w"), Some(Command::CloseTab));
        assert_eq!(c("f4"), Some(Command::CloseTab));
        assert_eq!(c("t"), Some(Command::NewTab));
        assert_eq!(c("shift-t"), Some(Command::ReopenTab));
        assert_eq!(c("tab"), Some(Command::NextTab));
        assert_eq!(c("shift-tab"), Some(Command::PrevTab));
        assert_eq!(c("1"), Some(Command::SelectTab(0)));
        assert_eq!(c("9"), Some(Command::SelectTab(8)));
        assert_eq!(c("shift-n"), Some(Command::TabToNewWindow));
        assert_eq!(c("l"), Some(Command::CycleLink));
        assert_eq!(c("shift-x"), Some(Command::ToggleCrosshairSync));
        assert_eq!(c("shift-g"), Some(Command::ToggleMagnet));
        assert_eq!(cmd("shift-f10"), Some(Command::Menu));
        assert_eq!(c("shift-s"), Some(Command::SaveAs));
        assert_eq!(c("o"), Some(Command::Open));
        assert_eq!(c("shift-d"), Some(Command::Toggle(PaneKind::Dom)));
        assert_eq!(c("shift-e"), Some(Command::Toggle(PaneKind::Tape)));
        assert_eq!(c("shift-c"), Some(Command::ChartOnly));
        assert_eq!(c("shift-h"), Some(Command::ShowHidden));
        assert_eq!(c("shift-o"), Some(Command::PopOut));
        assert_eq!(c("shift-b"), Some(Command::DockBesideChart));
        assert_eq!(c("shift-p"), Some(Command::Palette));
        assert_eq!(c("shift-l"), Some(Command::ToggleStudyLabels));
        assert_eq!(c("shift-i"), Some(Command::ToggleStudies));
        assert_eq!(c("shift-m"), Some(Command::CleanChart));
        assert_eq!(cmd("f1"), Some(Command::ToggleHints));
        let t = |k: &str| Some(Command::Trade(k_trade(k)));
        assert_eq!(c("shift-f"), t("flatten"));
        assert_eq!(c("shift-a"), t("cancel"));
        assert_eq!(c("shift-space"), t("arm"));
        assert_eq!(c("shift-u"), t("account"));
        assert_eq!(c("alt-up"), t("up"));
        assert_eq!(c("alt-down"), t("down"));
        assert_eq!(c("shift-k"), t("bracket"));
        // No trading command fires from a bare key, Shift alone or Alt alone.
        for k in [
            "f", "a", "space", "shift-f", "shift-a", "alt-up", "alt-down", "up",
        ] {
            assert!(!matches!(cmd(k), Some(Command::Trade(_))), "{k}");
        }
        // Neighbours that must stay as they are: Ctrl+L links, Ctrl+I (indicators menu),
        // Ctrl+K (picker) and Alt+V (profile) belong to the chart, plain C to the DOM.
        assert_eq!(c("l"), Some(Command::CycleLink));
        for k in ["alt-v", "c", "i", "shift-f1"] {
            assert_eq!(cmd(k), None, "{k}");
        }
        // Ctrl+I opens the chart's studies menu (a command: also the palette and the menu).
        assert_eq!(c("i"), Some(Command::Studies));
        assert_eq!(c("k"), None);
    }

    fn k_trade(k: &str) -> TradeCmd {
        match k {
            "flatten" => TradeCmd::Flatten,
            "cancel" => TradeCmd::CancelAll,
            "arm" => TradeCmd::ToggleArm,
            "account" => TradeCmd::NextAccount,
            "up" => TradeCmd::QtyUp,
            "down" => TradeCmd::QtyDown,
            _ => TradeCmd::NextBracket,
        }
    }

    #[test]
    fn pnl_display_has_a_ctrl_alt_key_and_palette_entries() {
        assert_eq!(cmd(&format!("{}-alt-p", ctrl())), Some(Command::CyclePnl));
        let p = palette();
        assert!(p.contains(&Command::CyclePnl));
        for m in crate::pnl::PnlDisplay::ALL {
            assert!(p.contains(&Command::SetPnl(m)), "{m:?}");
        }
        assert!(
            filter("p&l display off", &p).contains(&Command::SetPnl(crate::pnl::PnlDisplay::Off))
        );
    }

    #[test]
    fn header_order_buttons_have_ctrl_alt_keys() {
        let ca = |k: &str| cmd(&format!("{}-alt-{k}", ctrl()));
        assert_eq!(ca("b"), Some(Command::Trade(TradeCmd::BuyMarket)));
        assert_eq!(ca("s"), Some(Command::Trade(TradeCmd::SellMarket)));
        assert_eq!(ca("j"), Some(Command::Trade(TradeCmd::JoinBid)));
        assert_eq!(ca("k"), Some(Command::Trade(TradeCmd::JoinAsk)));
        assert_eq!(ca("r"), Some(Command::Trade(TradeCmd::Reverse)));
        // Their neighbours stay what they were: Ctrl+Shift+B docks, Ctrl+Shift+S saves.
        assert_eq!(
            cmd(&format!("{}-shift-b", ctrl())),
            Some(Command::DockBesideChart)
        );
        assert_eq!(cmd(&format!("{}-shift-s", ctrl())), Some(Command::SaveAs));
        // No order fires from a bare letter, Shift or Alt alone.
        for k in [
            "b", "s", "r", "j", "k", "shift-b", "alt-b", "alt-s", "alt-r",
        ] {
            assert!(!matches!(cmd(k), Some(Command::Trade(_))), "{k}");
        }
    }

    #[test]
    fn chart_trading_toggles_have_keys_and_palette_entries() {
        let ca = |k: &str| cmd(&format!("{}-alt-{k}", ctrl()));
        assert_eq!(ca("t"), Some(Command::ToggleChartBar));
        assert_eq!(ca("f"), Some(Command::ToggleFillMarks));
        let p = palette();
        assert!(p.contains(&Command::ToggleChartBar) && p.contains(&Command::ToggleFillMarks));
        // Neighbours unchanged: Ctrl+Shift+F flattens, Ctrl+T opens a tab.
        assert_eq!(
            cmd(&format!("{}-shift-f", ctrl())),
            Some(Command::Trade(TradeCmd::Flatten))
        );
        assert_eq!(cmd(&format!("{}-t", ctrl())), Some(Command::NewTab));
    }

    #[test]
    fn every_trading_command_has_a_key_and_a_palette_entry() {
        let p = palette();
        for t in TradeCmd::ALL {
            assert!(!hint(Command::Trade(t)).is_empty(), "{t:?} has no key");
            assert!(p.contains(&Command::Trade(t)), "{t:?} not in the palette");
        }
        assert_eq!(
            filter("flatten", &p),
            vec![Command::Trade(TradeCmd::Flatten)]
        );
    }

    #[test]
    fn no_key_is_bound_twice() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert!(
                    !(a.ctrl == b.ctrl && a.shift == b.shift && a.alt == b.alt && a.key == b.key),
                    "{:?} and {:?} share a key",
                    a.cmd,
                    b.cmd
                );
            }
        }
    }

    #[test]
    fn pane_keys_stay_with_the_panes() {
        // Letters open the picker, C/G drive the DOM, Home resets the chart, Ctrl+K opens the
        // picker, Ctrl+D and Alt+M work inside it, Alt+V/R/B/G drive the chart's volume
        // profile: none of them is a workspace command.
        for k in [
            "a", "c", "g", "shift-g", "home", "left", "escape", "enter", "alt-m", "0",
            // Chart volume profile: Alt+V/R/B/G.
            "alt-v", "alt-r", "alt-b", "alt-g",
        ] {
            assert_eq!(cmd(k), None, "{k}");
        }
        for k in ["k", "d"] {
            assert_eq!(cmd(&format!("{}-{k}", ctrl())), None, "{k}");
        }
        // Ctrl+I is the Studies command (it acts on the focused chart; decision 0018).
        assert_eq!(cmd(&format!("{}-i", ctrl())), Some(Command::Studies));
        assert_eq!(cmd("ctrl-alt-left"), None);
    }

    #[test]
    fn hints_come_from_the_bindings() {
        if cfg!(target_os = "macos") {
            assert_eq!(hint(Command::CloseTab), "⌘F4");
            assert_eq!(hint(Command::Split(Axis::Row)), "⇧⌘→");
        } else {
            assert_eq!(hint(Command::CloseTab), "Ctrl+F4");
            assert_eq!(hint(Command::Split(Axis::Row)), "Ctrl+Shift+→");
            assert_eq!(hint(Command::ReopenTab), "Ctrl+Shift+T");
            assert_eq!(hint(Command::Menu), "Shift+F10");
        }
        assert_eq!(hint(Command::SetLink(Some(1))), "");
    }

    #[test]
    fn every_bound_command_is_in_the_palette() {
        // Discoverability: anything with a hotkey can also be found by name.
        let p = palette();
        for b in BINDINGS {
            // The palette itself, and docking (a popped-out pane window's command), aside.
            let in_palette =
                p.contains(&b.cmd) || matches!(b.cmd, Command::DockBesideChart | Command::Palette);
            assert!(in_palette, "{:?} has a key but isn't in the palette", b.cmd);
        }
    }

    #[test]
    fn palette_filters_by_words() {
        let p = palette();
        assert_eq!(filter("reopen", &p), vec![Command::ReopenTab]);
        assert_eq!(
            filter("hide/show dom", &p),
            vec![Command::Toggle(PaneKind::Dom)]
        );
        assert!(filter("layout", &p).len() >= 5);
        assert_eq!(filter("", &p).len(), p.len());
        assert!(filter("zzz", &p).is_empty());
        assert_eq!(
            filter("reset columns", &p),
            vec![Command::Columns(ColumnCmd::Reset)]
        );
        assert_eq!(
            filter("dom column vol", &p),
            vec![Command::Columns(ColumnCmd::Toggle(Column::Dom(
                DomCol::Volume
            )))]
        );
        assert_eq!(filter("performance", &p), vec![Command::TogglePerfStats]);
        // Timeframes: one entry each, "Timeframe: 1m" … "Timeframe: 1W"; no keys of their own
        // (digits and letters typed on a chart are the chart's).
        assert_eq!(
            filter("timeframe 15m", &p),
            vec![Command::Timeframe(Timeframe::Minutes(15))]
        );
        assert_eq!(
            filter("timeframe 1w", &p),
            vec![Command::Timeframe(Timeframe::Week)]
        );
        assert_eq!(
            p.iter()
                .filter(|c| matches!(c, Command::Timeframe(_)))
                .count(),
            Timeframe::ALL.len()
        );
        assert!(
            BINDINGS
                .iter()
                .all(|b| !matches!(b.cmd, Command::Timeframe(_) | Command::TimeframeMenu))
        );
        // A palette/menu command: no key of its own.
        assert!(BINDINGS.iter().all(|b| b.cmd != Command::TogglePerfStats));
        // The DOM trail: palette and pane menu only, never a key (pane keys are reserved).
        assert_eq!(
            filter("price trail", &p),
            vec![Command::DomTrail, Command::DomTrailHalfLife]
        );
        assert!(
            BINDINGS
                .iter()
                .all(|b| !matches!(b.cmd, Command::DomTrail | Command::DomTrailHalfLife))
        );
    }
}
