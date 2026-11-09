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
//! `dropdown::dropdown(..).on_dismiss(..)` or [`layer`].

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

/// `content` where its own style puts it (e.g. absolute in a pane), drawn over the window at
/// `dropdown::PRIORITY` with [`defer_backdrop`] under it: the studies menu.
pub fn layer(content: impl IntoElement, dismiss: OnDismiss) -> Layer {
    Layer {
        content: Some(content.into_any_element()),
        dismiss: Some(dismiss),
    }
}

pub struct Layer {
    content: Option<AnyElement>,
    dismiss: Option<OnDismiss>,
}

impl IntoElement for Layer {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Layer {
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
        let rect = Rect::new(
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );
        let priority = crate::dropdown::PRIORITY;
        defer_backdrop(
            window,
            cx,
            crate::dropdown::BACKDROP_PRIORITY,
            &[rect],
            dismiss,
        );
        // As `gpui::deferred`: at the parent's offset, keeping the content's own layout.
        let offset = window.element_offset();
        window.defer_draw(content, offset, priority, None);
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
