//! Workspace commands and the keys bound to them (decision 0018: every action is a typed
//! command; hotkeys, menus, header controls and the command palette are just ways to issue
//! one). One table ([`BINDINGS`]) gives both the keymap and the hints shown in menus and the
//! palette, so they can't disagree. Pure and tested; the workspace executes the commands.
//!
//! "Ctrl" means Cmd on macOS (GPUI's `secondary` modifier).

use gpui::Keystroke;

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
    /// The focused chart's studies menu (VWAPs, volume profile, levels; templates).
    Studies,
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
        Command::ToggleCrosshairSync => "Crosshair sync on/off".into(),
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
        Command::Studies => "Chart: studies (VWAP, volume profile, levels, templates)…".into(),
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
        Command::ToggleStudyLabels,
        Command::ToggleStudies,
        Command::CleanChart,
        Command::ToggleHints,
        Command::ToggleLevels,
        Command::ToggleLevelLabels,
        Command::TogglePerfStats,
        Command::Studies,
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
    v.push(Command::ExportDiagnostics);
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
        // A palette/menu command: no key of its own.
        assert!(BINDINGS.iter().all(|b| b.cmd != Command::TogglePerfStats));
    }
}
