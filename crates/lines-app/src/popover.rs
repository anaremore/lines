//! Popovers: the studies menu, the timeframe and account lists, the pane/tab/column menus,
//! the drawing style editor.
//!
//! The rule (docs/invariants.md, lines-app UI): a click outside an open popover closes it and
//! does nothing else (no pan, no drawing selected, no order, no focus change in what was under
//! it); Esc closes it; inline edits not yet applied stay as a draft for the next open; and only
//! one popover is open at a time in the app.
//!
//! Flatten and Cancel all stay one action (docs/design.md §3): [`always_live`] draws them over
//! every backdrop, so their own handler takes the press, and closes the popover.
//!
//! [`outside_click`] is the click rule. A popover puts a transparent, occluding, window-wide
//! backdrop under itself ([`defer_backdrop`], deferred at `dropdown::BACKDROP_PRIORITY`, below
//! every list and menu), so the press reaches the backdrop and nothing under it. [`opened`] keeps one
//! popover open: it closes the previous one (in any pane or window). A new popover is one
//! [`Kind`], an [`opened`] call where it opens, and its list drawn through
//! `dropdown::dropdown(..).on_dismiss(..)` or, for a pop-up panel, [`floating`].

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Context, Element, EntityId, Global, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseDownEvent, Pixels, Styled,
    Window, div, point, px, size,
};

use crate::layout::Rect;

/// What an outside click does to the open popover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dismiss {
    /// The click goes no further (nothing under the popover sees it). Always true.
    pub consume: bool,
}

/// A press at `point` (window logical px) while a popover is `open` with its parts at `rects`:
/// `Some` when it closes the popover (outside every part), `None` when the popover is shut or
/// the press is on one of its parts (they handle their own clicks).
pub fn outside_click(open: bool, point: (f32, f32), rects: &[Rect]) -> Option<Dismiss> {
    if !open || rects.iter().any(|r| r.contains(point.0, point.1)) {
        return None;
    }
    Some(Dismiss { consume: true })
}

/// A key that closes the open popover: Esc (whatever the popover is in the middle of; its
/// state stays as a draft where it has one).
pub fn closes_on_key(key: &str) -> bool {
    key == "escape"
}

/// Close a popover that keeps a draft: its state moves to `draft`. False if it was shut.
pub fn shut<T>(open: &mut Option<T>, draft: &mut Option<T>) -> bool {
    match open.take() {
        Some(m) => {
            *draft = Some(m);
            true
        }
        None => false,
    }
}

/// Open a popover that keeps a draft: the draft if there is one, else a fresh state.
pub fn reopen<T: Default>(draft: &mut Option<T>) -> T {
    draft.take().unwrap_or_default()
}

/// Which popover of its owner (a pane or window view) is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Studies,
    Timeframe,
    Account,
    /// A pane, tab, column or detached-pane menu (`crate::menu`).
    Menu,
    /// A chart's drawing style editor (`crate::style_editor`).
    StyleEditor,
    /// The orders of a chart's group tag (`crate::order_group_view`).
    OrderGroup,
}

/// The one open popover, as a pure value: `K` names a popover (owner and kind).
#[derive(Debug)]
pub struct Slot<K> {
    open: Option<K>,
}

impl<K> Default for Slot<K> {
    fn default() -> Self {
        Slot { open: None }
    }
}

impl<K: Copy + PartialEq> Slot<K> {
    /// `k` opened: the popover to close, if another one was open.
    pub fn open(&mut self, k: K) -> Option<K> {
        self.open.replace(k).filter(|prev| *prev != k)
    }

    /// `k` closed (no change if another one is open now).
    pub fn close(&mut self, k: K) {
        if self.open == Some(k) {
            self.open = None;
        }
    }

    pub fn current(&self) -> Option<K> {
        self.open
    }
}

type Key = (EntityId, Kind);
type Closer = Box<dyn FnOnce(&mut App)>;

/// The app's open popover and how to close it.
#[derive(Default)]
struct Open {
    slot: Slot<Key>,
    close: Option<Closer>,
}

impl Global for Open {}

/// The view's popover `kind` just opened: any other open popover (this view's, another pane's,
/// another window's) closes. `close` closes this one when the next popover opens; it must do
/// nothing if this one is already shut. It runs after the current update (never re-entrantly).
pub fn opened<V: 'static>(cx: &mut Context<V>, kind: Kind, close: fn(&mut V, &mut Context<V>)) {
    let key = (cx.entity_id(), kind);
    let weak = cx.weak_entity();
    let g = cx.default_global::<Open>();
    let prev = g.slot.open(key).and(g.close.take());
    g.close = Some(Box::new(move |cx: &mut App| {
        let _ = weak.update(cx, close);
    }));
    if let Some(prev) = prev {
        cx.defer(prev);
    }
}

/// The view's popover `kind` closed (keeps the slot exact; a stale entry is harmless, since
/// closing a shut popover does nothing).
pub fn closed<V: 'static>(cx: &mut Context<V>, kind: Kind) {
    mark_moved_under_pointer();
    let key = (cx.entity_id(), kind);
    if let Some(g) = cx.try_global::<Open>()
        && g.slot.current() == Some(key)
    {
        let g = cx.global_mut::<Open>();
        g.slot.close(key);
        g.close = None;
    }
}

/// A handler that closes a popover (from `cx.listener`).
pub type OnDismiss = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// Call in prepaint, just before deferring a popover at `priority`: a transparent backdrop
/// over the whole window, under the popover and over everything else, that takes any press
/// outside `rects` and closes the popover with `dismiss` (consumed: [`outside_click`]).
pub fn defer_backdrop(
    window: &mut Window,
    cx: &mut App,
    priority: usize,
    rects: &[Rect],
    dismiss: OnDismiss,
) {
    let rects: Vec<Rect> = rects.to_vec();
    let v = window.viewport_size();
    let mut backdrop = div()
        .occlude()
        .w(v.width)
        .h(v.height)
        .on_any_mouse_down(move |ev: &MouseDownEvent, window, cx| {
            let p = (f32::from(ev.position.x), f32::from(ev.position.y));
            if let Some(d) = outside_click(true, p, &rects) {
                if d.consume {
                    cx.stop_propagation();
                }
                mark_dismissing();
                dismiss(ev, window, cx);
            }
        })
        .into_any_element();
    backdrop.layout_as_root(
        size(
            AvailableSpace::Definite(v.width),
            AvailableSpace::Definite(v.height),
        ),
        window,
        cx,
    );
    window.defer_draw(backdrop, point(px(0.0), px(0.0)), priority, None);
}

thread_local! {
    static DISMISSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Where and when a first press landed.
type FirstPress = (f32, f32, std::time::Instant);

thread_local! {
    /// Where the last first press that reached an order control landed, until something under the
    /// pointer goes or moves.
    static FIRST_PRESS: std::cell::Cell<Option<FirstPress>> = const { std::cell::Cell::new(None) };
}

/// Note that what was under the pointer has gone or moved (a list closed, a menu item ran, a pane
/// changed size): a double-click's second press was aimed at it, not at what is there now.
pub fn mark_moved_under_pointer() {
    FIRST_PRESS.with(|f| f.set(None));
}

/// How far apart a double-click's presses can be (the system's own limit is a few pixels).
const SAME_SPOT: f32 = 8.0;

/// How old a first press can be and still be this double-click's. Generous for a normal
/// double-click; a slower one's second press does nothing, the safe way to be wrong.
const FIRST_PRESS_AGE: std::time::Duration = std::time::Duration::from_secs(1);

/// One press's verdict: whether to do nothing. A first press always acts and is remembered. A
/// second may act only if a first press that reached an order control landed on the same spot just
/// now and nothing has moved since. Anything else is safe by default: a list or button that closed
/// on the first press, or a control that has taken its place, needs no list of its own here.
///
/// An order control stays refused for the whole run of presses, until a pause makes the next a
/// first press again. An exit (`exit`) is refused only once: the refused press is remembered, so a
/// trader pressing Flatten again and again after a near miss gets it on the next press.
fn refuse_press(
    first: &mut Option<FirstPress>,
    click_count: usize,
    at: (f32, f32),
    now: std::time::Instant,
    exit: bool,
) -> bool {
    if click_count <= 1 {
        *first = Some((at.0, at.1, now));
        return false;
    }
    let refused = !first.is_some_and(|(x, y, when)| {
        (x - at.0).abs() <= SAME_SPOT
            && (y - at.1).abs() <= SAME_SPOT
            && now.saturating_duration_since(when) <= FIRST_PRESS_AGE
    });
    if !refused || exit {
        *first = Some((at.0, at.1, now));
    }
    refused
}

fn press_verdict(click_count: usize, at: gpui::Point<gpui::Pixels>, exit: bool) -> bool {
    let at = (f32::from(at.x), f32::from(at.y));
    FIRST_PRESS.with(|f| {
        let mut first = f.get();
        let refused = refuse_press(&mut first, click_count, at, std::time::Instant::now(), exit);
        f.set(first);
        refused
    })
}

/// Called by every press that can send an order or set what one uses: whether to do nothing.
pub fn second_press(click_count: usize, at: gpui::Point<gpui::Pixels>) -> bool {
    press_verdict(click_count, at, false)
}

/// The same for Flatten and Cancel all, which are refused at most once in a run of presses.
pub fn second_press_exit(click_count: usize, at: gpui::Point<gpui::Pixels>) -> bool {
    press_verdict(click_count, at, true)
}

/// A backdrop just took a press: its release is the dismiss's too, and finishes nothing (a
/// drawing waiting for its second point). Cleared by the next press a pane sees.
pub fn mark_dismissing() {
    mark_moved_under_pointer();
    DISMISSING.with(|d| d.set(true));
}

/// Whether the press now being released was a dismiss ([`mark_dismissing`]).
pub fn dismissing() -> bool {
    DISMISSING.with(|d| d.get())
}

/// A pane took a press of its own: releases are its again.
pub fn clear_dismissing() {
    DISMISSING.with(|d| d.set(false));
}

/// Close whatever popover is open (Flatten or Cancel all was pressed with one open).
pub fn close_any(cx: &mut App) {
    if cx.try_global::<Open>().is_some_and(|g| g.close.is_some()) {
        mark_moved_under_pointer();
        let g = cx.global_mut::<Open>();
        let close = g.close.take();
        g.slot = Slot::default();
        if let Some(close) = close {
            cx.defer(close);
        }
    }
}

/// `content` (Flatten, Cancel all) drawn over every popover's backdrop, at
/// `dropdown::LIVE_PRIORITY` and clipped as where it sits: one press even with a popover
/// open (docs/design.md §3). Its own handler runs (left button only, as always) and closes
/// the popover ([`close_any`]). Deferred draws are kept with a cached pane, so this holds on
/// frames the pane doesn't redraw.
pub fn always_live(content: impl IntoElement) -> Live {
    let mask = std::rc::Rc::new(std::cell::Cell::new(None));
    Live {
        content: Some(
            Masked {
                child: content.into_any_element(),
                mask: mask.clone(),
            }
            .into_any_element(),
        ),
        mask,
    }
}

type MaskCell = std::rc::Rc<std::cell::Cell<Option<gpui::ContentMask<Pixels>>>>;

pub struct Live {
    content: Option<AnyElement>,
    /// The clip where the control sits, for the deferred draw ([`Masked`]).
    mask: MaskCell,
}

/// A deferred child under a clip in every phase: GPUI applies a deferred draw's mask when
/// painting only, so without this the control's hitbox would take presses outside its pane
/// (a short DOM's clipped Flatten over the pane below).
struct Masked {
    child: AnyElement,
    mask: MaskCell,
}

impl IntoElement for Masked {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Masked {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(self.mask.get(), |w| {
            self.child.prepaint(w, cx);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(self.mask.get(), |w| self.child.paint(w, cx));
    }
}

impl IntoElement for Live {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Live {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let content = self.content.as_mut().expect("laid out once");
        (content.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let _ = cx;
        let Some(content) = self.content.take() else {
            return;
        };
        let offset = window.element_offset();
        let mask = window.content_mask();
        self.mask.set(Some(mask));
        window.defer_draw(content, offset, crate::dropdown::LIVE_PRIORITY, Some(mask));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

// ---- floating panels ----------------------------------------------------------------------

/// A pop-up panel, drawn by [`floating`] with a title bar (grip, title, ×) that drags it.
/// Each kind remembers where it was dragged to (`Settings::panel_places`, window logical px),
/// shared by every pane and window, and is kept inside the window every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Panel {
    Palette,
    Picker,
    OpenWorkspace,
    SaveAs,
    Studies,
    StyleEditor,
    OrderGroup,
}

impl Panel {
    pub const ALL: [Panel; 7] = [
        Panel::Palette,
        Panel::Picker,
        Panel::OpenWorkspace,
        Panel::SaveAs,
        Panel::Studies,
        Panel::StyleEditor,
        Panel::OrderGroup,
    ];

    /// Its key in `Settings::panel_places`.
    pub fn key(self) -> &'static str {
        match self {
            Panel::Palette => "palette",
            Panel::Picker => "picker",
            Panel::OpenWorkspace => "open_workspace",
            Panel::SaveAs => "save_as",
            Panel::Studies => "studies",
            Panel::StyleEditor => "style_editor",
            Panel::OrderGroup => "order_group",
        }
    }
}

/// Whole rows a wheel moves a panel's list by: `dy` (px, positive = up) added to what earlier
/// small deltas left over (`carry`), a row per `line` px. Positive rows move down the list.
pub fn wheel_steps(carry: &mut f32, dy: f32, line: f32) -> isize {
    if line <= 0.0 {
        return 0;
    }
    *carry += dy;
    let n = (*carry / line).trunc();
    *carry -= n * line;
    -(n as isize)
}

thread_local! {
    static WHEEL_CARRY: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// [`wheel_steps`] for a wheel event over a panel's list.
pub fn wheel_rows(ev: &gpui::ScrollEvent, window: &Window) -> isize {
    let line = window.line_height();
    let dy = f32::from(ev.delta.pixel_delta(line).y);
    WHEEL_CARRY.with(|c| {
        let mut carry = c.get();
        let n = wheel_steps(&mut carry, dy, f32::from(line));
        c.set(carry);
        n
    })
}

/// The title bar's height (logical px): the × keeps its 20 px hit box.
pub const TITLE_H: f32 = 24.0;

/// `origin` moved so a panel of `size` lies inside `window` (all logical px). A panel bigger
/// than the window keeps its top-left (the title bar) on screen.
pub fn clamp_place(origin: (f32, f32), size: (f32, f32), window: (f32, f32)) -> (f32, f32) {
    let fit = |at: f32, len: f32, room: f32| at.min(room - len).max(0.0);
    (
        fit(origin.0, size.0, window.0),
        fit(origin.1, size.1, window.1),
    )
}

/// Where a panel goes: where it was dragged to (`saved`), else where it opens (`natural`),
/// kept inside the window.
pub fn panel_place(
    saved: Option<(f32, f32)>,
    natural: (f32, f32),
    size: (f32, f32),
    window: (f32, f32),
) -> (f32, f32) {
    clamp_place(saved.unwrap_or(natural), size, window)
}

/// A drag in progress: the panel, the pointer's offset from its top-left, and where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PanelDrag {
    panel: Panel,
    grab: (f32, f32),
    at: (f32, f32),
}

thread_local! {
    static PANEL_DRAG: std::cell::Cell<Option<PanelDrag>> = const { std::cell::Cell::new(None) };
}

fn dragged(panel: Panel) -> Option<PanelDrag> {
    PANEL_DRAG.with(|d| d.get()).filter(|d| d.panel == panel)
}

fn saved_place(panel: Panel, cx: &App) -> Option<(f32, f32)> {
    if let Some(d) = dragged(panel) {
        return Some(d.at);
    }
    crate::picker_view::settings(cx)
        .panel_places
        .get(panel.key())
        .map(|&(x, y)| (x as f32, y as f32))
}

type Dismisser = std::rc::Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// `content` in a panel frame: a title bar above it (a grip and `title`, draggable; a × that
/// closes it with `dismiss`), drawn over the window at `dropdown::PRIORITY` with
/// [`defer_backdrop`] under it, so a press outside closes it too and does nothing else. A drag
/// never closes it. Where `content`'s own style puts it (e.g. absolute in a pane) is where it
/// opens until it is dragged ([`panel_place`]).
pub fn floating(
    panel: Panel,
    title: impl Into<gpui::SharedString>,
    theme: &crate::theme::Theme,
    content: impl IntoElement,
    dismiss: OnDismiss,
) -> Floating {
    Floating {
        panel,
        title: title.into(),
        theme: *theme,
        content: Some(content.into_any_element()),
        dismiss: Some(std::rc::Rc::from(dismiss)),
        priority: crate::dropdown::PRIORITY,
    }
}

pub struct Floating {
    priority: usize,
    panel: Panel,
    title: gpui::SharedString,
    theme: crate::theme::Theme,
    content: Option<AnyElement>,
    dismiss: Option<Dismisser>,
}

impl IntoElement for Floating {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Floating {
    /// Drawn at this deferred priority instead of `dropdown::PRIORITY` (the workspace's
    /// prompts sit above header dropdowns).
    pub fn priority(mut self, priority: usize) -> Self {
        self.priority = priority;
        self
    }

    fn title_bar(&self, width: Pixels, at: (f32, f32), dismiss: Dismisser) -> AnyElement {
        use gpui::{MouseButton, ParentElement};
        let t = self.theme;
        let panel = self.panel;
        div()
            .id(("panel-title", panel as u64))
            .occlude()
            .w(width)
            .h(px(TITLE_H))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .pl_2()
            .pr(px(2.0))
            .bg(t.panel)
            .border_1()
            .border_b_0()
            .border_color(t.axis_line)
            .rounded_t_md()
            .text_size(px(12.0))
            .cursor_grab()
            .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, _| {
                let p = (f32::from(ev.position.x), f32::from(ev.position.y));
                PANEL_DRAG.with(|d| {
                    d.set(Some(PanelDrag {
                        panel,
                        grab: (p.0 - at.0, p.1 - at.1),
                        at,
                    }))
                });
                window.refresh();
            })
            .child(div().flex_none().text_color(t.text_dim).child("⠿"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_main)
                    .child(self.title.clone()),
            )
            .child(
                crate::icons::button(
                    ("panel-close", panel as u64),
                    crate::icons::Icon::Close,
                    &format!("Close {}", self.title),
                    None,
                    false,
                    &t,
                )
                .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                    cx.stop_propagation();
                    mark_dismissing();
                    dismiss(ev, window, cx);
                }),
            )
            .into_any_element()
    }
}

impl Element for Floating {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let content = self.content.as_mut().expect("laid out once");
        (content.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let (Some(content), Some(dismiss)) = (self.content.take(), self.dismiss.take()) else {
            return;
        };
        let v = window.viewport_size();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let natural = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let at = panel_place(
            saved_place(self.panel, cx),
            natural,
            (w, h + TITLE_H),
            (f32::from(v.width), f32::from(v.height)),
        );
        // A drag keeps the place as drawn (inside the window), which is what a release saves.
        if let Some(mut d) = dragged(self.panel) {
            d.at = at;
            PANEL_DRAG.with(|c| c.set(Some(d)));
        }
        let rect = Rect::new(at.0, at.1, w, h + TITLE_H);
        let close = dismiss.clone();
        defer_backdrop(
            window,
            cx,
            crate::dropdown::BACKDROP_PRIORITY,
            &[rect],
            Box::new(move |ev, window, cx| close(ev, window, cx)),
        );
        let priority = self.priority;
        let mut bar = self.title_bar(bounds.size.width, at, dismiss);
        bar.layout_as_root(
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(px(TITLE_H)),
            ),
            window,
            cx,
        );
        window.defer_draw(bar, point(px(at.0), px(at.1)), priority, None);
        // As `gpui::deferred`, moved from where its own layout put it to under the title bar.
        let offset =
            window.element_offset() + point(px(at.0 - natural.0), px(at.1 + TITLE_H - natural.1));
        window.defer_draw(content, offset, priority, None);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        // While this panel is dragged it follows the pointer anywhere in the window; a release
        // keeps the place (the drag never reaches the backdrop: a move or release isn't a press).
        let panel = self.panel;
        if dragged(panel).is_none() {
            return;
        }
        window.on_mouse_event(move |ev: &gpui::MouseMoveEvent, phase, window, _| {
            if phase != gpui::DispatchPhase::Capture {
                return;
            }
            let p = (f32::from(ev.position.x), f32::from(ev.position.y));
            if let Some(mut g) = dragged(panel) {
                g.at = (p.0 - g.grab.0, p.1 - g.grab.1);
                PANEL_DRAG.with(|d| d.set(Some(g)));
                window.refresh();
            }
        });
        window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, window, cx| {
            if phase != gpui::DispatchPhase::Capture {
                return;
            }
            if let Some(g) = dragged(panel) {
                PANEL_DRAG.with(|d| d.set(None));
                let at = (g.at.0.round() as i32, g.at.1.round() as i32);
                crate::picker_view::update_settings(cx, |s| {
                    s.panel_places.insert(panel.key().to_owned(), at);
                });
                window.refresh();
            }
        });
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    /// A panel stays inside the window: pulled back from every edge, and one bigger than the
    /// window keeps its title bar on screen.
    #[test]
    fn a_panel_is_kept_inside_the_window() {
        let win = (1000.0, 600.0);
        assert_eq!(
            clamp_place((100.0, 50.0), (300.0, 200.0), win),
            (100.0, 50.0)
        );
        assert_eq!(
            clamp_place((900.0, 500.0), (300.0, 200.0), win),
            (700.0, 400.0)
        );
        assert_eq!(clamp_place((-40.0, -10.0), (300.0, 200.0), win), (0.0, 0.0));
        assert_eq!(clamp_place((50.0, 50.0), (1200.0, 900.0), win), (0.0, 0.0));
        // After the window shrinks (or moves to a smaller monitor) the same place is pulled in.
        assert_eq!(
            clamp_place((700.0, 400.0), (300.0, 200.0), (800.0, 500.0)),
            (500.0, 300.0)
        );
    }

    /// Where it was dragged wins over where it opens; with none, where it opens.
    #[test]
    fn a_dragged_place_is_remembered() {
        let win = (1000.0, 600.0);
        let size = (300.0, 200.0);
        assert_eq!(panel_place(None, (20.0, 30.0), size, win), (20.0, 30.0));
        assert_eq!(
            panel_place(Some((400.0, 100.0)), (20.0, 30.0), size, win),
            (400.0, 100.0)
        );
        assert_eq!(
            panel_place(Some((990.0, 590.0)), (20.0, 30.0), size, win),
            (700.0, 400.0)
        );
    }

    /// A wheel notch is a row; a trackpad's small deltas add up to one.
    #[test]
    fn the_wheel_moves_whole_rows() {
        let mut carry = 0.0;
        assert_eq!(wheel_steps(&mut carry, -20.0, 20.0), 1, "down a row");
        assert_eq!(wheel_steps(&mut carry, 40.0, 20.0), -2, "up two");
        assert_eq!(wheel_steps(&mut carry, -7.0, 20.0), 0);
        assert_eq!(wheel_steps(&mut carry, -7.0, 20.0), 0);
        assert_eq!(
            wheel_steps(&mut carry, -7.0, 20.0),
            1,
            "small deltas add up"
        );
        assert_eq!(wheel_steps(&mut carry, -7.0, 0.0), 0);
    }

    #[test]
    fn every_panel_has_its_own_key() {
        let all = Panel::ALL;
        let keys: std::collections::BTreeSet<_> = all.iter().map(|p| p.key()).collect();
        assert_eq!(keys.len(), all.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second press acts only after a guarded first press on the same spot with nothing moved
    /// since: double-clicking a control still repeats it, and everything else is refused.
    #[test]
    fn a_second_press_acts_only_after_its_own_first() {
        let spot = (100.0, 200.0);
        let now = std::time::Instant::now();
        let later = now + FIRST_PRESS_AGE + std::time::Duration::from_millis(1);
        for exit in [false, true] {
            // Double-click on a control: both presses act, as a click twice does. For an exit
            // that sends one order: the core places no second Flatten while the first is open.
            let mut first = None;
            assert!(
                !refuse_press(&mut first, 1, spot, now, exit),
                "a first press"
            );
            assert!(!refuse_press(&mut first, 2, spot, now, exit));
            assert!(
                !refuse_press(&mut first, 3, (103.0, 198.0), now, exit),
                "a few px off"
            );
            // An old press on this spot is not this double-click's first.
            assert!(refuse_press(
                &mut Some((spot.0, spot.1, now)),
                2,
                spot,
                later,
                exit
            ));
            // The first press went elsewhere: nothing remembered, or something far away.
            assert!(refuse_press(&mut None, 2, spot, now, exit));
            assert!(refuse_press(
                &mut Some((400.0, 200.0, now)),
                2,
                spot,
                now,
                exit
            ));
            // A pause makes the next a first press, whatever came before.
            assert!(!refuse_press(&mut None, 1, spot, now, exit));
        }
    }

    /// A triple-click after a list closed: Sell or a ladder row stays refused for the whole run,
    /// while an exit pressed again is never dead.
    #[test]
    fn a_run_of_presses_refuses_orders_throughout_and_an_exit_once() {
        let spot = (100.0, 200.0);
        let now = std::time::Instant::now();
        let mut order = None;
        assert!(refuse_press(&mut order, 2, spot, now, false));
        assert!(refuse_press(&mut order, 3, spot, now, false));
        assert!(refuse_press(&mut order, 4, spot, now, false));
        assert!(
            !refuse_press(&mut order, 1, spot, now, false),
            "after a pause"
        );
        let mut exit = None;
        assert!(refuse_press(&mut exit, 2, spot, now, true));
        assert!(!refuse_press(&mut exit, 3, spot, now, true));
        assert!(!refuse_press(&mut exit, 4, spot, now, true));
    }

    /// A list under a header chip.
    const LIST: Rect = Rect {
        x: 200.0,
        y: 52.0,
        w: 150.0,
        h: 300.0,
    };

    #[test]
    fn an_outside_click_closes_and_is_consumed() {
        for p in [(10.0, 10.0), (199.9, 60.0), (350.0, 60.0), (220.0, 352.0)] {
            assert_eq!(
                outside_click(true, p, &[LIST]),
                Some(Dismiss { consume: true }),
                "{p:?}"
            );
        }
    }

    #[test]
    fn an_inside_click_is_left_to_the_popover() {
        for p in [(200.0, 52.0), (275.0, 200.0), (349.9, 351.9)] {
            assert_eq!(outside_click(true, p, &[LIST]), None, "{p:?}");
        }
        // Any of its parts (a list and its editor).
        let editor = Rect::new(360.0, 52.0, 200.0, 100.0);
        assert_eq!(outside_click(true, (400.0, 60.0), &[LIST, editor]), None);
        assert!(outside_click(true, (400.0, 200.0), &[LIST, editor]).is_some());
    }

    #[test]
    fn nothing_open_dismisses_nothing() {
        assert_eq!(outside_click(false, (10.0, 10.0), &[LIST]), None);
        assert_eq!(outside_click(false, (10.0, 10.0), &[]), None);
    }

    #[test]
    fn esc_closes() {
        assert!(closes_on_key("escape"));
        for k in ["enter", "backspace", "up", "a", "space"] {
            assert!(!closes_on_key(k), "{k}");
        }
    }

    #[test]
    fn a_closed_popover_keeps_its_draft_for_the_next_open() {
        let (mut open, mut draft) = (Some(String::from("half typed")), None);
        assert!(shut(&mut open, &mut draft));
        assert_eq!(
            (open.as_deref(), draft.as_deref()),
            (None, Some("half typed"))
        );
        // Closing again changes nothing (a stale close from another popover opening).
        assert!(!shut(&mut open, &mut draft));
        assert_eq!(draft.as_deref(), Some("half typed"));
        assert_eq!(reopen(&mut draft), "half typed");
        assert_eq!(draft, None);
        // No draft: a fresh state.
        assert_eq!(reopen::<String>(&mut draft), "");
    }

    #[test]
    fn only_one_popover_is_open_at_a_time() {
        let mut s = Slot::default();
        assert_eq!(s.open((1, Kind::Timeframe)), None);
        // Another pane's list: the first one closes.
        assert_eq!(s.open((2, Kind::Account)), Some((1, Kind::Timeframe)));
        // The same pane's other popover: the list closes.
        assert_eq!(s.open((2, Kind::Studies)), Some((2, Kind::Account)));
        // The workspace's menu: the studies menu closes.
        assert_eq!(s.open((0, Kind::Menu)), Some((2, Kind::Studies)));
        // Reopening the open one closes nothing.
        assert_eq!(s.open((0, Kind::Menu)), None);
        assert_eq!(s.current(), Some((0, Kind::Menu)));
    }

    /// Draw order: every backdrop under Flatten and Cancel all (one press with a popover
    /// open), those under the lists and menus, the window's overlays on top.
    #[test]
    fn always_live_controls_draw_between_backdrops_and_popovers() {
        use crate::dropdown::{BACKDROP_PRIORITY, LIVE_PRIORITY, OVERLAY_PRIORITY, PRIORITY};
        const { assert!(BACKDROP_PRIORITY < LIVE_PRIORITY) };
        const { assert!(LIVE_PRIORITY < PRIORITY) };
        const { assert!(PRIORITY < OVERLAY_PRIORITY) };
    }

    #[test]
    fn closing_clears_only_its_own_popover() {
        let mut s = Slot::default();
        s.open((1, Kind::Timeframe));
        s.open((2, Kind::Menu));
        // A stale close of the replaced list leaves the menu open.
        s.close((1, Kind::Timeframe));
        assert_eq!(s.current(), Some((2, Kind::Menu)));
        s.close((2, Kind::Menu));
        assert_eq!(s.current(), None);
        assert_eq!(s.open((1, Kind::Timeframe)), None);
    }
}
