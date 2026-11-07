//! A detached pane in its own OS window (roadmap 5b).
//!
//! The pane is not moved: it stays in its tab (same link group, settings, feed and event
//! subscriptions), whose layout keeps its leaf with every ratio around it. The tab simply
//! doesn't place it while it is detached; this window renders it instead. So re-attaching is
//! exact (previous size and position), a detached DOM keeps following its chart's link group,
//! and the workspace file records it as a `detached` leaf plus this window's placement.
//!
//! Re-attach: the "re-attach" header control, Ctrl+Shift+O, the context menu, or closing the
//! window (a pane is never lost by closing its window). "Dock beside chart" (Ctrl+Shift+B)
//! moves it next to the chart that last had focus instead.

use anyhow::Result;
use gpui::{
    AnyElement, App, AppContext, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, StyleRefinement, Styled,
    Subscription, TitlebarOptions, Window, WindowHandle, WindowOptions, div, point, px,
};

use crate::commands::{self, Command};
use crate::menu::{Menu, MenuItem, MenuKey};
use crate::theme::Theme;
use crate::workspace::{
    self, PaneHandle, PaneId, Workspace, dock_beside_chart, header_controls, reattach_pane,
};
use crate::wsdoc::{Placement, WindowState};

pub struct PaneWindow {
    pub id: PaneId,
    handle: PaneHandle,
    theme: Theme,
    focus: FocusHandle,
    /// Where this window is, kept current for saving (read without entering the window).
    placement: Option<Placement>,
    /// The size it was opened at (screenshots: a hidden window reports no size).
    opened: (f32, f32),
    menu: Option<Menu>,
    _subs: Vec<Subscription>,
}

/// This pane window's commands: re-attach (also Pop out, which toggles), dock beside chart,
/// and the menu. Everything else belongs to main windows.
fn command(cmd: Command) -> Option<Command> {
    match cmd {
        Command::PopOut | Command::Reattach => Some(Command::Reattach),
        Command::DockBesideChart | Command::Menu | Command::Settings => Some(cmd),
        _ => None,
    }
}

impl PaneWindow {
    /// A new look (the Settings window's appearance): this window and its pane.
    pub(crate) fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        self.handle.set_theme(theme, cx);
        cx.notify();
    }

    fn new(
        id: PaneId,
        handle: PaneHandle,
        opened: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let env = cx.global::<Workspace>().env.clone();
        let mut subs = Vec::new();
        if env.track_visibility {
            subs.push(cx.observe_window_visibility(window, |this, vis, _, cx| {
                this.handle.set_visible(vis.is_visible(), cx);
            }));
        }
        subs.push(cx.observe_window_bounds(window, |this, window, cx| {
            this.placement = workspace::placement(window, cx);
            cx.notify();
        }));
        handle.set_visible(true, cx);
        handle.focus(window, cx);
        PaneWindow {
            id,
            handle,
            theme: env.theme.get(),
            focus: cx.focus_handle(),
            placement: workspace::placement(window, cx),
            opened,
            menu: None,
            _subs: subs,
        }
    }

    fn run(&mut self, cmd: Command, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let id = self.id;
        match command(cmd) {
            Some(Command::Reattach) => cx.defer(move |cx| reattach_pane(id, cx)),
            Some(Command::DockBesideChart) => cx.defer(move |cx| dock_beside_chart(id, cx)),
            Some(Command::Menu) => self.open_menu(point(px(24.0), px(24.0)), cx),
            Some(Command::Settings) => cx.defer(|cx| crate::settings_view::open(None, cx)),
            _ => {}
        }
        cx.notify();
    }

    fn open_menu(&mut self, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        // Any other popover closes (`crate::popover`).
        crate::popover::opened(cx, crate::popover::Kind::Menu, |this: &mut Self, cx| {
            if this.menu.take().is_some() {
                cx.notify();
            }
        });
        self.menu = Some(Menu::new(
            at,
            vec![
                MenuItem::named("Re-attach to its previous spot", Command::Reattach, false),
                MenuItem::of(Command::DockBesideChart, false),
            ],
        ));
        cx.notify();
    }

    fn on_key_capture(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.menu.as_mut() {
            match menu.key(ev.keystroke.key.as_str()) {
                MenuKey::Close => self.menu = None,
                MenuKey::Run(cmd) => self.run(cmd, window, cx),
                MenuKey::Moved | MenuKey::Ignored => {}
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(cmd) = commands::command_for(&ev.keystroke).and_then(command) {
            // A held key repeats only commands that may repeat: never an order or arming.
            if !ev.is_held || commands::repeats(cmd) {
                self.run(cmd, window, cx);
            }
            cx.stop_propagation();
        }
    }
}

impl Render for PaneWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        // Controls only while the pointer is over this window (no permanent chrome).
        let controls = window.is_window_hovered().then(|| {
            let kind = self.handle.kind();
            header_controls(
                &t,
                kind,
                true,
                false,
                |this: &mut Self, cmd, _, window, cx| this.run(cmd, window, cx),
                self.id,
                cx,
            )
        });
        let menu: Option<AnyElement> = self.menu.as_ref().map(|m| {
            m.render(
                &t,
                window.viewport_size(),
                |this: &mut Self| &mut this.menu,
                |this, cmd, window, cx| this.run(cmd, window, cx),
                cx,
            )
        });
        div()
            .size_full()
            .relative()
            .bg(t.bg)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_capture))
            .child(
                div()
                    .size_full()
                    .relative()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                            this.open_menu(ev.position, cx)
                        }),
                    )
                    .child(
                        self.handle
                            .view()
                            .cached(StyleRefinement::default().size_full()),
                    )
                    .children(controls),
            )
            .children(menu)
    }
}

/// Open pane `id` (detached in its tab) in its own window, at `place` if given (restored or
/// where it was dropped). Screenshots and the benchmark open it hidden at that size.
pub fn open(id: PaneId, handle: PaneHandle, place: Option<&Placement>, cx: &mut App) -> Result<()> {
    if find(id, cx).is_some() {
        return Ok(());
    }
    let env = cx.global::<Workspace>().env.clone();
    let opened = place.map_or((360.0, 640.0), |p| (p.w, p.h));
    let options = if env.track_visibility {
        let (bounds, display) = workspace::window_options(place, cx);
        WindowOptions {
            window_bounds: bounds,
            display_id: display,
            titlebar: Some(TitlebarOptions {
                title: Some(format!("Lines · {}", handle.kind().label()).into()),
                ..Default::default()
            }),
            icon: crate::window_icon(),
            ..Default::default()
        }
    } else {
        workspace::hidden_window("Lines (pane)", opened)
    };
    let h = cx.open_window(options, move |window, cx| {
        cx.new(|cx| PaneWindow::new(id, handle, opened, window, cx))
    })?;
    cx.global_mut::<Workspace>().pane_windows.push(h);
    // Closing the window re-attaches the pane: it is never lost that way.
    let _ = h.update(cx, |_, window, cx| {
        window.on_window_should_close(cx, move |_, cx| {
            cx.global_mut::<Workspace>()
                .pane_windows
                .retain(|w| w.window_id() != h.window_id());
            cx.defer(move |cx| reattach_pane(id, cx));
            true
        });
    });
    Ok(())
}

fn find(id: PaneId, cx: &App) -> Option<WindowHandle<PaneWindow>> {
    cx.global::<Workspace>()
        .pane_windows
        .iter()
        .find(|w| w.read(cx).is_ok_and(|pw| pw.id == id))
        .copied()
}

/// Close pane `id`'s window, if it has one (without re-attaching: the caller decided).
pub fn close(id: PaneId, cx: &mut App) {
    let Some(h) = find(id, cx) else { return };
    cx.global_mut::<Workspace>()
        .pane_windows
        .retain(|w| w.window_id() != h.window_id());
    let _ = h.update(cx, |_, window, _| window.remove_window());
}

/// Where pane `id`'s window is (for saving).
pub fn placement_of(id: PaneId, cx: &App) -> Option<Placement> {
    let h = find(id, cx)?;
    h.read(cx).ok()?.placement.clone().map(|p| Placement {
        state: WindowState::Windowed,
        ..p
    })
}

/// Every pane window and the size it was opened at (screenshots).
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
pub fn all(cx: &App) -> Vec<(WindowHandle<PaneWindow>, (f32, f32))> {
    cx.global::<Workspace>()
        .pane_windows
        .iter()
        .filter_map(|w| w.read(cx).ok().map(|pw| (*w, pw.opened)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneKind;

    /// Every control a pane window's header shows is a command the window runs.
    #[test]
    fn detached_controls_all_run_here() {
        for kind in [
            PaneKind::Chart,
            PaneKind::Dom,
            PaneKind::Tape,
            PaneKind::Journal,
        ] {
            for c in crate::pane::controls(kind, true) {
                assert_eq!(command(c.cmd), Some(c.cmd), "{kind:?} {}", c.name);
            }
        }
    }
}
