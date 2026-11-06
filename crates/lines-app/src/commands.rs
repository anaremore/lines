//! Workspace commands and the keys bound to them (decision 0018: every action is a typed
//! command; hotkeys and the context menu are just ways to issue one). Pure and tested; the
//! workspace executes them.
//!
//! "Ctrl" means Cmd on macOS (GPUI's `secondary` modifier).

use gpui::Keystroke;

use crate::layout::{Axis, Direction, LinkGroup, PaneKind, Preset};

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
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    /// Move the active tab into a new window.
    TabToNewWindow,
    ToggleCrosshairSync,
    /// The focused pane's context menu, from the keyboard.
    Menu,
    SaveAs,
    Open,
}

/// The command bound to a keystroke, if any.
pub fn command_for(k: &Keystroke) -> Option<Command> {
    let m = k.modifiers;
    // GPUI's `secondary`: Cmd on macOS, Ctrl elsewhere.
    let ctrl = if cfg!(target_os = "macos") {
        m.platform
    } else {
        m.control
    };
    // The modifier that isn't "Ctrl" on this platform (Ctrl on macOS, Win elsewhere) is never
    // part of a binding.
    if (cfg!(target_os = "macos") && m.control) || (!cfg!(target_os = "macos") && m.platform) {
        return None;
    }
    let key = k.key.as_str();
    let dir = |key: &str| match key {
        "left" => Some(Direction::Left),
        "right" => Some(Direction::Right),
        "up" => Some(Direction::Up),
        "down" => Some(Direction::Down),
        _ => None,
    };
    Some(match (ctrl, m.shift, m.alt, key) {
        (false, false, true, k) => Command::Focus(dir(k)?),
        (true, true, false, "right") => Command::Split(Axis::Row),
        (true, true, false, "down") => Command::Split(Axis::Column),
        (true, false, false, "w") => Command::ClosePane,
        (true, true, false, "w") => Command::CloseTab,
        (true, false, false, "t") => Command::NewTab,
        (true, false, false, "tab" | "pagedown") => Command::NextTab,
        (true, true, false, "tab") | (true, false, false, "pageup") => Command::PrevTab,
        (true, false, false, d) if d.len() == 1 && (b'1'..=b'9').contains(&d.as_bytes()[0]) => {
            Command::SelectTab((d.as_bytes()[0] - b'1') as usize)
        }
        (true, true, false, "n") => Command::TabToNewWindow,
        (true, false, false, "l") => Command::CycleLink,
        (true, true, false, "x") => Command::ToggleCrosshairSync,
        (false, true, false, "f10") | (true, false, false, ".") => Command::Menu,
        (true, true, false, "s") => Command::SaveAs,
        (true, false, false, "o") => Command::Open,
        _ => return None,
    })
}

/// The hint shown for a command in menus (the binding's main key).
pub fn hint(c: Command) -> &'static str {
    let mac = cfg!(target_os = "macos");
    match c {
        Command::Split(Axis::Row) if mac => "⇧⌘→",
        Command::Split(Axis::Row) => "Ctrl+Shift+→",
        Command::Split(Axis::Column) if mac => "⇧⌘↓",
        Command::Split(Axis::Column) => "Ctrl+Shift+↓",
        Command::ClosePane if mac => "⌘W",
        Command::ClosePane => "Ctrl+W",
        Command::CloseTab if mac => "⇧⌘W",
        Command::CloseTab => "Ctrl+Shift+W",
        Command::NewTab if mac => "⌘T",
        Command::NewTab => "Ctrl+T",
        Command::TabToNewWindow if mac => "⇧⌘N",
        Command::TabToNewWindow => "Ctrl+Shift+N",
        Command::CycleLink if mac => "⌘L",
        Command::CycleLink => "Ctrl+L",
        Command::ToggleCrosshairSync if mac => "⇧⌘X",
        Command::ToggleCrosshairSync => "Ctrl+Shift+X",
        Command::SaveAs if mac => "⇧⌘S",
        Command::SaveAs => "Ctrl+Shift+S",
        Command::Open if mac => "⌘O",
        Command::Open => "Ctrl+O",
        _ => "",
    }
}

/// The workspace's key hints for the status line.
pub fn workspace_hints() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌥arrows: focus  ·  ⇧⌘→/↓: split  ·  ⌘W: close  ·  ⌘T: tab  ·  ⌘L: link  ·  ⇧F10 or right-click: menu"
    } else {
        "Alt+arrows: focus  ·  Ctrl+Shift+→/↓: split  ·  Ctrl+W: close  ·  Ctrl+T: tab  ·  Ctrl+L: link  ·  Shift+F10 or right-click: menu"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Option<Command> {
        // `parse` wants a `'static` str; leaking a few test strings is fine.
        command_for(&Keystroke::parse(Box::leak(s.to_owned().into_boxed_str())).unwrap())
    }

    #[test]
    fn bindings() {
        let ctrl = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        let c = |rest: &str| cmd(&format!("{ctrl}-{rest}"));
        assert_eq!(cmd("alt-left"), Some(Command::Focus(Direction::Left)));
        assert_eq!(cmd("alt-down"), Some(Command::Focus(Direction::Down)));
        assert_eq!(c("shift-right"), Some(Command::Split(Axis::Row)));
        assert_eq!(c("shift-down"), Some(Command::Split(Axis::Column)));
        assert_eq!(c("w"), Some(Command::ClosePane));
        assert_eq!(c("shift-w"), Some(Command::CloseTab));
        assert_eq!(c("t"), Some(Command::NewTab));
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
    }

    #[test]
    fn pane_keys_stay_with_the_panes() {
        // Letters open the picker, C/G drive the DOM, Home resets the chart, Ctrl+K opens the
        // picker, Ctrl+D and Alt+M work inside it: none of them is a workspace command.
        for k in [
            "a", "c", "g", "shift-g", "home", "left", "escape", "enter", "alt-m", "0",
        ] {
            assert_eq!(cmd(k), None, "{k}");
        }
        let ctrl = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        for k in ["k", "d"] {
            assert_eq!(cmd(&format!("{ctrl}-{k}")), None, "{k}");
        }
        assert_eq!(cmd("ctrl-alt-left"), None);
    }
}
