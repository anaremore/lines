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
        Command::DockBesideChart
        | Command::Menu
        | Command::Settings
        | Command::SettingsAt(_) => Some(cmd),
        // A chart's drawing style editor, and deleting the selected drawing (its menu's
        // Delete; the rest of the drawing keys: main windows).
        Command::Draw(
            crate::commands::DrawCmd::Style
            | crate::commands::DrawCmd::DeleteSelected
            // The measure's menu: its parts and Remove.
            | crate::commands::DrawCmd::MeasurePart(_)
            | crate::commands::DrawCmd::RemoveMeasure,
        ) => Some(cmd),
        // Stop all strategies works from every window (decision 0025's kill switch).
        Command::Strategy(
            crate::commands::StrategyCmd::StopAll
            | crate::commands::StrategyCmd::StopAllKeep
            | crate::commands::StrategyCmd::ToggleMarks,
        ) => Some(cmd),
        // Trading keys act on this window's pane, as in main windows.
        Command::Trade(_) => Some(cmd),
        // A popped-out chart's trade history (Alt+E, its menu's switch).
        Command::ToggleTrades
        | Command::ToggleTradeLines
        | Command::ToggleTradeLabels
        | Command::TradeAccounts(_)
        | Command::TradePeriod(_) => Some(cmd),
        // Every open chart's trade history, from any window.
        Command::TradesOnAllCharts(_) => Some(cmd),
        // A popped-out chart's Volume, on the chart or in a pane (its menu's rows).
        Command::VolumeOverlay(_) => Some(cmd),
        // A popped-out chart's drawing toolbar (its menu's switch).
        Command::ToggleDrawingTools => Some(cmd),
        // An order tag's menu (no key reaches here with one).
        Command::Order(_) => Some(cmd),
        // A legend row's menu; adding to every chart of a tab is the main window's.
        Command::Study(s) if s != crate::commands::StudyCmd::AddToTab => Some(cmd),
        // The UI zoom is app-wide: its keys work from every window.
        Command::ZoomIn | Command::ZoomOut | Command::ZoomReset => Some(cmd),
        // The replay is app-wide too (Go to is the main window's prompt).
        Command::Replay(r) if r != crate::commands::ReplayCmd::GoTo => Some(cmd),
        _ => None,
    }
}

/// `items` without the rows this window can't run (`command`), and without separators left
/// at an end or doubled.
fn runnable(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut out: Vec<MenuItem> = Vec::with_capacity(items.len());
    for i in items {
        if i.cmd.is_some_and(|c| command(c).is_none()) {
            continue;
        }
        if i.is_sep() && out.last().is_none_or(MenuItem::is_sep) {
            continue;
        }
        out.push(i);
    }
    while out.last().is_some_and(MenuItem::is_sep) {
        out.pop();
    }
    out
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
        subs.push(crate::settings_view::observe_os_appearance(window, cx));
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

    fn run(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let id = self.id;
        match command(cmd) {
            Some(Command::Reattach) => cx.defer(move |cx| reattach_pane(id, cx)),
            Some(Command::DockBesideChart) => cx.defer(move |cx| dock_beside_chart(id, cx)),
            Some(Command::Menu) => self.open_menu(point(px(24.0), px(24.0)), cx),
            Some(Command::Settings) => cx.defer(|cx| crate::settings_view::open(None, cx)),
            Some(Command::SettingsAt(s)) => {
                cx.defer(move |cx| crate::settings_view::open_section(s, cx));
            }
            // The replay bar from this window: steps by this chart's bars.
            Some(Command::Replay(rc)) => {
                let tf = match &self.handle {
                    PaneHandle::Chart(c) => Some(c.read(cx).timeframe()),
                    _ => None,
                };
                // A refusal (no replay) has nothing to show here; a jump's note reaches the bar.
                let _ = crate::replay_bar::run(rc, crate::replay_bar::step_ms(tf), cx);
            }
            Some(c @ (Command::ZoomIn | Command::ZoomOut | Command::ZoomReset)) => {
                cx.defer(move |cx| crate::workspace::zoom_command(c, cx));
            }
            Some(Command::VolumeOverlay(overlay)) => {
                if let PaneHandle::Chart(chart) = &self.handle {
                    chart.update(cx, |ch, cx| ch.place_volume(overlay, cx));
                }
            }
            Some(Command::ToggleDrawingTools) => {
                if let PaneHandle::Chart(chart) = &self.handle {
                    chart.update(cx, |ch, cx| ch.toggle_drawing_tools(cx));
                }
            }
            Some(Command::TradesOnAllCharts(show)) => {
                cx.defer(move |cx| crate::workspace::trades_on_all_charts(show, cx));
            }
            Some(
                c @ (Command::ToggleTrades
                | Command::ToggleTradeLines
                | Command::ToggleTradeLabels
                | Command::TradeAccounts(_)
                | Command::TradePeriod(_)),
            ) => {
                if let PaneHandle::Chart(chart) = &self.handle {
                    chart.update(cx, |ch, cx| {
                        let t = crate::trade_marks::apply(c, ch.trades_config());
                        ch.set_trades(t, cx);
                    });
                }
            }
            // Every strategy command by name: hiding markers must never fall through to a stop.
            Some(Command::Strategy(c)) => {
                use crate::commands::StrategyCmd as S;
                match c {
                    S::ToggleMarks => {
                        if let PaneHandle::Chart(chart) = &self.handle {
                            chart.update(cx, |c, cx| c.toggle_strategy_marks(cx));
                        }
                    }
                    S::StopAll | S::StopAllKeep => {
                        if let Some(t) = crate::trading::Trading::try_global(cx) {
                            crate::strategy_panel::submit_stop_all(
                                t,
                                c == S::StopAll,
                                lines_trading::Source::Hotkey,
                            );
                        }
                    }
                    // `command` lets none of these through to a pane window.
                    S::Open | S::Start | S::Stop => {}
                }
            }
            // Only keys reach here with a trading command (the window's menu has none).
            Some(Command::Trade(tc)) => {
                let issued = std::time::Instant::now();
                // A size being typed needs its keys; focus may sit on this window's root.
                if tc == crate::commands::TradeCmd::TypeQty {
                    self.handle.focus(window, cx);
                }
                match &self.handle {
                    PaneHandle::Dom(d) => {
                        d.update(cx, |v, cx| crate::trade_bar::run_key(v, tc, issued, cx));
                    }
                    PaneHandle::Chart(c) if crate::trade_bar::chart_runs(tc) => {
                        c.update(cx, |v, cx| crate::trade_bar::run_key(v, tc, issued, cx));
                    }
                    PaneHandle::Chain(c) if tc == crate::commands::TradeCmd::NextAccount => {
                        c.update(cx, |v, cx| v.next_account(cx));
                    }
                    PaneHandle::Chain(c)
                        if matches!(
                            tc,
                            crate::commands::TradeCmd::Flatten
                                | crate::commands::TradeCmd::CancelAll
                        ) =>
                    {
                        let flatten = tc == crate::commands::TradeCmd::Flatten;
                        c.update(cx, |v, cx| {
                            v.run_exit(flatten, lines_trading::Source::Hotkey, cx)
                        });
                    }
                    _ => {}
                }
            }
            Some(Command::Draw(d)) => {
                if let PaneHandle::Chart(chart) = &self.handle {
                    chart.update(cx, |c, cx| c.drawing_command(d, cx));
                }
            }
            Some(Command::Study(crate::commands::StudyCmd::Undo)) => {
                crate::workspace::undo_study(cx);
            }
            Some(Command::Study(s)) => {
                if let PaneHandle::Chart(chart) = &self.handle {
                    chart.update(cx, |c, cx| c.study_command(s, cx));
                }
            }
            Some(Command::Order(o)) => {
                // The size field needs the chart's focus (it closes without it).
                // So does a staged order's Enter and Esc.
                if matches!(
                    o,
                    crate::commands::OrderCmd::Resize(_) | crate::commands::OrderCmd::Stage(..)
                ) {
                    self.handle.focus(window, cx);
                }
                if let PaneHandle::Chart(chart) = &self.handle {
                    chart.update(cx, |c, cx| c.order_command(o, cx));
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// The window's menu (`crate::menu` rules): an object's after a right click on an order's
    /// tag or a drawing, else the pane's, with what this window can run (`command`).
    fn open_menu(&mut self, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        // Any other popover closes (`crate::popover`).
        crate::popover::opened(
            cx,
            crate::popover::Kind::Menu,
            |this: &mut Self, cx| {
                if this.menu.take().is_some() {
                    cx.notify();
                }
            },
            |this: &Self| this.menu.is_some(),
        );
        let object = match &self.handle {
            PaneHandle::Chart(chart) => chart.update(cx, |c, _| c.take_menu_object()),
            _ => None,
        };
        // The right click's price is taken by this menu: no later one offers orders at it.
        let price = match &self.handle {
            PaneHandle::Chart(chart) => chart.update(cx, |c, _| c.take_menu_price()),
            _ => None,
        };
        self.menu = Some(match object {
            Some(o) => Menu::new(at, runnable(crate::pane_menu::object_items(&o))).unselected(),
            None => {
                let menu = Menu::new(at, self.pane_items(price, cx));
                // Never an order on a stray Enter (design.md §6 rule 5).
                if crate::pane_menu::places_orders(&menu.items) {
                    menu.unselected()
                } else {
                    menu
                }
            }
        });
        cx.notify();
    }

    fn pane_items(&self, price: Option<lines_core::Price>, cx: &Context<Self>) -> Vec<MenuItem> {
        let mut items = Vec::with_capacity(7);
        if let PaneHandle::Chart(chart) = &self.handle {
            let c = chart.read(cx);
            // The order section first, as in the main window's chart menu.
            let switches = crate::workspace::dom_switches(cx, None);
            if let Some(o) = c.order_menu(price, crate::workspace::chart_trades(c, &switches), cx) {
                items.extend(crate::pane_menu::order_items(&o));
            }
            if c.drawing_selected() {
                items.push(MenuItem::named(
                    "Drawing style…",
                    Command::Draw(crate::commands::DrawCmd::Style),
                    false,
                ));
            }
            items.push(
                MenuItem::named(
                    "Strategy markers",
                    Command::Strategy(commands::StrategyCmd::ToggleMarks),
                    !c.strategy_marks_hidden(),
                )
                .switch(),
            );
            if let Some(overlay) = c.volume_overlay() {
                items.push(
                    MenuItem::named(
                        "Volume on the chart",
                        Command::VolumeOverlay(!overlay),
                        overlay,
                    )
                    .switch(),
                );
            }
            items.push(
                MenuItem::named(
                    "Drawing tools",
                    Command::ToggleDrawingTools,
                    c.drawing_tools(),
                )
                .switch(),
            );
            items.push(
                MenuItem::named(
                    "Show trades",
                    Command::ToggleTrades,
                    !c.trades_config().hidden,
                )
                .switch(),
            );
            items.push(MenuItem::named(
                "Hide trades on all charts",
                Command::TradesOnAllCharts(false),
                false,
            ));
            items.push(MenuItem::named(
                "Show trades on all charts",
                Command::TradesOnAllCharts(true),
                false,
            ));
            items.push(MenuItem::sep());
        }
        items.push(MenuItem::named(
            "Re-attach to its previous spot",
            Command::Reattach,
            false,
        ));
        items.push(MenuItem::of(Command::DockBesideChart, false));
        items.push(MenuItem::sep());
        // This window has no command palette: Settings stays here, at the pane's own section.
        items.push(crate::pane_menu::settings_row(self.handle.kind()));
        items
    }

    fn on_key_capture(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Flatten and Cancel all always act: an open menu or popover closes and never takes
        // the key (as in main windows).
        let route = commands::key_route(
            commands::command_for(&ev.keystroke).and_then(command),
            self.menu.is_some(),
            false,
        );
        if let commands::KeyRoute::Emergency(cmd) = route {
            self.menu = None;
            crate::popover::close_any(cx);
            if !ev.is_held {
                self.run(cmd, window, cx);
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
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
            // A pane taking typed text (a note, the style editor, the symbol search) keeps its
            // plain and Alt keys, as in main windows.
            if self.handle.takes_text(cx) && !commands::runs_while_typing(&ev.keystroke, cmd) {
                return;
            }
            // The replay's plain keys, by the main window's rule (`replay_bar::key_goes_to_pane`).
            if let Command::Replay(rc) = cmd {
                let replaying = cx.has_global::<crate::replay_bar::Replay>();
                let typing = self.handle.takes_text(cx);
                let chain = matches!(self.handle, PaneHandle::Chain(_));
                if crate::replay_bar::key_goes_to_pane(rc, replaying, typing, chain) {
                    return;
                }
            }
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
        window.set_rem_size(crate::zoom::rem());
        let t = self.theme;
        // Controls only while the pointer is over this window (no permanent chrome).
        let controls = window.is_window_hovered().then(|| {
            let kind = self.handle.kind();
            header_controls(
                &t,
                kind,
                true,
                false,
                f32::from(window.viewport_size().width),
                |this: &mut Self, cmd, _, at, window, cx| match (cmd, at) {
                    (Command::Menu, Some(at)) => this.open_menu(at, cx),
                    _ => this.run(cmd, window, cx),
                },
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
            // The UI font (`crate::fonts`): every text in the window inherits it.
            .font_family(crate::fonts::ui_family())
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
            window_min_size: (handle.kind() == crate::layout::PaneKind::Dom).then(|| {
                // The trading panel's least size, at the zoom it opens with.
                gpui::size(
                    crate::zoom::ui(crate::trade_bar::MIN_DOM_WIDTH),
                    crate::zoom::ui(crate::trade_bar::MIN_DOM_PANE_H),
                )
            }),
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

    /// Stop all strategies (the kill switch) runs from a popped-out window too.
    #[test]
    fn stop_all_strategies_runs_in_pane_windows() {
        use crate::commands::StrategyCmd;
        for c in [StrategyCmd::StopAll, StrategyCmd::StopAllKeep] {
            assert_eq!(command(Command::Strategy(c)), Some(Command::Strategy(c)));
        }
        // A popped-out chart's menu toggles its markers.
        let marks = Command::Strategy(StrategyCmd::ToggleMarks);
        assert_eq!(command(marks), Some(marks));
        // A popped-out chart's drawing toolbar.
        let tools = Command::ToggleDrawingTools;
        assert_eq!(command(tools), Some(tools));
        // Every open chart's trade history, from a popped-out window too.
        for show in [false, true] {
            let c = Command::TradesOnAllCharts(show);
            assert_eq!(command(c), Some(c));
            // A popped-out chart's Volume placement.
            let v = Command::VolumeOverlay(show);
            assert_eq!(command(v), Some(v));
        }
        // Trade history on a popped-out chart, too.
        for c in [
            Command::ToggleTrades,
            Command::TradePeriod(crate::trade_marks::Period::Week),
        ] {
            assert_eq!(command(c), Some(c));
        }
        // Starting stays in main windows (the strategy panel).
        assert_eq!(command(Command::Strategy(StrategyCmd::Start)), None);
    }

    /// Every object menu keeps its rows in a popped-out window: an order's, a drawing's and
    /// the measure's (nothing filtered down to a bare header).
    #[test]
    fn object_menus_keep_their_rows_here() {
        use crate::pane_menu::{MenuObject, object_items};
        for o in [
            MenuObject::Order {
                id: 1,
                title: "Stop 1 @ 1.00".into(),
                stop: true,
            },
            MenuObject::Drawing {
                title: "Trend line".into(),
            },
            MenuObject::Measure {
                show: crate::chart_draw::MeasureShow::default(),
                pnl: crate::pnl::PnlDisplay::Both,
            },
        ] {
            let n = object_items(&o).len();
            assert_eq!(runnable(object_items(&o)).len(), n, "{o:?}");
        }
    }

    /// Every control a pane window's header shows is a command the window runs.
    #[test]
    fn detached_controls_all_run_here() {
        for kind in [
            PaneKind::Chart,
            PaneKind::Dom,
            PaneKind::Tape,
            PaneKind::Journal,
            PaneKind::Strategies,
            PaneKind::Chain,
            PaneKind::Positions,
        ] {
            for c in crate::pane::controls(kind, true) {
                assert_eq!(command(c.cmd), Some(c.cmd), "{kind:?} {}", c.name);
            }
        }
    }
}
