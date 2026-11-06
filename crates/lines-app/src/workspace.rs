//! A window's content: a [`Layout`] tree of independent panes.
//!
//! The workspace creates one pane per leaf, giving each the same [`PaneFeed`] to start with,
//! and renders the tree as nested splits. Panes are rendered as *cached* views: GPUI replays a
//! pane's last frame unless that pane notified, so a tick on one market repaints only the
//! panes showing it. When a pane switches symbol it emits [`FeedChanged`], and the workspace
//! hands that feed to every other pane in the same link group. Nothing else couples panes.

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Entity, IntoElement, ParentElement, Render,
    StyleRefinement, Styled, Subscription as GpuiSubscription, Window, div, px, relative,
};

use crate::chart_view::{self, ChartView, Fixture};
use crate::dom_view::{DomOptions, DomView};
use crate::feeds::PaneFeed;
use crate::layout::{Axis, Layout, PaneKind, PaneSpec};
use crate::pane::{FeedChanged, Pane};
use crate::tape_view::{TapeOptions, TapeView};
use crate::theme::Theme;

/// Everything panes are created from.
#[derive(Clone)]
pub struct PaneSetup {
    pub feed: PaneFeed,
    pub theme: Theme,
    /// Screenshot mode: frozen clock, no pointer.
    pub frozen: bool,
    pub fixture: Fixture,
    pub dom: DomOptions,
    pub tape: TapeOptions,
}

#[derive(Clone)]
pub enum PaneHandle {
    Chart(Entity<ChartView>),
    Dom(Entity<DomView>),
    Tape(Entity<TapeView>),
}

impl PaneHandle {
    fn view(&self) -> AnyView {
        match self {
            PaneHandle::Chart(e) => e.clone().into(),
            PaneHandle::Dom(e) => e.clone().into(),
            PaneHandle::Tape(e) => e.clone().into(),
        }
    }

    fn set_feed(&self, feed: PaneFeed, cx: &mut App) {
        match self {
            PaneHandle::Chart(e) => e.update(cx, |p, cx| p.set_feed(feed, cx)),
            PaneHandle::Dom(e) => e.update(cx, |p, cx| p.set_feed(feed, cx)),
            PaneHandle::Tape(e) => e.update(cx, |p, cx| p.set_feed(feed, cx)),
        }
    }
}

pub struct WorkspaceView {
    layout: Layout,
    panes: Vec<(PaneSpec, PaneHandle)>,
    theme: Theme,
    _subscriptions: Vec<GpuiSubscription>,
}

impl WorkspaceView {
    pub fn new(layout: Layout, setup: PaneSetup, cx: &mut Context<Self>) -> Self {
        let mut panes = Vec::new();
        let mut subs = Vec::new();
        for (i, spec) in layout.panes().into_iter().enumerate() {
            let s = setup.clone();
            let handle = match spec.kind {
                PaneKind::Chart => {
                    let e = cx.new(|cx| ChartView::new(s.feed, s.theme, s.frozen, s.fixture, cx));
                    subs.push(
                        cx.subscribe(&e, move |ws, _, ev: &FeedChanged, cx| ws.forward(i, ev, cx)),
                    );
                    PaneHandle::Chart(e)
                }
                PaneKind::Dom => {
                    let e = cx.new(|cx| DomView::new(s.feed, s.theme, s.frozen, s.dom, cx));
                    subs.push(
                        cx.subscribe(&e, move |ws, _, ev: &FeedChanged, cx| ws.forward(i, ev, cx)),
                    );
                    PaneHandle::Dom(e)
                }
                PaneKind::Tape => {
                    let e = cx.new(|cx| TapeView::new(s.feed, s.theme, s.tape, cx));
                    subs.push(
                        cx.subscribe(&e, move |ws, _, ev: &FeedChanged, cx| ws.forward(i, ev, cx)),
                    );
                    PaneHandle::Tape(e)
                }
            };
            panes.push((spec, handle));
        }
        WorkspaceView {
            layout,
            panes,
            theme: setup.theme,
            _subscriptions: subs,
        }
    }

    /// Pane `from` switched feed: the rest of its link group follows.
    fn forward(&mut self, from: usize, ev: &FeedChanged, cx: &mut Context<Self>) {
        let Some(link) = self.panes[from].0.link else {
            return;
        };
        for (i, (spec, handle)) in self.panes.iter().enumerate() {
            if i != from && spec.link == Some(link) {
                handle.set_feed(ev.0.clone(), cx);
            }
        }
        cx.notify();
    }

    fn render_node(&self, node: &Layout, next: &mut usize) -> AnyElement {
        let t = self.theme;
        match node {
            Layout::Pane(_) => {
                let view = self.panes[*next].1.view();
                *next += 1;
                div()
                    .size_full()
                    .overflow_hidden()
                    .child(view.cached(StyleRefinement::default().size_full()))
                    .into_any_element()
            }
            Layout::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let a = self.render_node(first, next);
                let b = self.render_node(second, next);
                match axis {
                    Axis::Row => div()
                        .size_full()
                        .flex()
                        .flex_row()
                        .child(div().h_full().flex_none().w(relative(*ratio)).child(a))
                        .child(
                            div()
                                .h_full()
                                .flex_1()
                                .min_w_0()
                                .border_l_1()
                                .border_color(t.axis_line)
                                .child(b),
                        )
                        .into_any_element(),
                    Axis::Column => div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(div().w_full().flex_none().h(relative(*ratio)).child(a))
                        .child(div().w_full().flex_1().min_h_0().child(b))
                        .into_any_element(),
                }
            }
        }
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let mut next = 0;
        let body = self.render_node(&self.layout, &mut next);
        let has_dom = self.panes.iter().any(|(s, _)| s.kind == PaneKind::Dom);
        let mut hints = String::from(chart_view::HINTS);
        if has_dom {
            hints.push_str("  ·  DOM: wheel scroll, C centre, G / Shift+G grouping");
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(div().flex_1().min_h_0().child(body))
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(t.text_dim)
                    .bg(t.panel)
                    .border_t_1()
                    .border_color(t.axis_line)
                    .child(hints),
            )
    }
}
