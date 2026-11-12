//! Dropdowns: a trigger (a header chip, a panel field) and, while open, its list directly
//! under it.
//!
//! The rule (docs/invariants.md, lines-app UI): the list is as wide as its content, at least the
//! trigger's width and at most the window's (less `MARGIN` each side); it is at most as tall as
//! the larger room below or above the trigger, and scrolls past that. Its left edge lines up
//! with the trigger's left edge and its top sits `GAP` px under the trigger's bottom. It moves
//! only when it would leave the window: left just far enough to keep `MARGIN` from the right
//! edge, and up above the trigger when there is no room below. [`fit`] is that rule; [`dropdown`]
//! applies it with the trigger's real bounds, in window coordinates, so a list escapes its
//! pane's header and the pane itself. Every dropdown is placed so, the trading panel's Qty and
//! Bracket lists included (docs/design.md: a field's list opens under it, in the menus' look);
//! like every list they cover what is under them, Flatten and Cancel all included (§3).

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Window, point, px, size,
};

use crate::layout::Rect;
use crate::popover::OnDismiss;

/// Space between the trigger and its list (logical px).
pub const GAP: f32 = 2.0;
/// Space a list keeps from the window's right edge (logical px).
pub const MARGIN: f32 = 4.0;
/// Deferred-draw priority of an open list: over the panes.
pub const PRIORITY: usize = 4;
/// Deferred-draw priority of every popover's backdrop (`crate::popover`): over the panes,
/// Flatten and Cancel all included (nothing draws over a backdrop but a popover), under every
/// list and menu.
pub const BACKDROP_PRIORITY: usize = 1;
/// Deferred-draw priority of a list opened inside a popover (the studies menu's): over the
/// popover, its backdrop just under it (a press elsewhere in the popover closes only the list);
/// under the window's overlays like every list.
pub const NESTED_PRIORITY: usize = PRIORITY + 2;
/// Deferred-draw priority of the window's own overlays (pane and tab menus, palette and
/// prompts, the switch-all banner): over any open list (a nested one included), so a list never
/// hides or takes the clicks of a menu opened after it.
pub const OVERLAY_PRIORITY: usize = NESTED_PRIORITY + 1;

/// Where a list `popup` (width, height) opens for a trigger at `trigger`, in a window
/// `window` (width, height) big, all in window logical px: its top-left corner.
pub fn place(trigger: Rect, popup: (f32, f32), window: (f32, f32)) -> (f32, f32) {
    let ((w, h), (ww, wh)) = (popup, window);
    let mut x = trigger.x;
    if x + w > ww - MARGIN {
        x = ww - MARGIN - w;
    }
    let below = trigger.bottom() + GAP;
    let above = trigger.y - GAP - h;
    let y = if below + h <= wh || above < 0.0 && wh - below >= trigger.y {
        // Fits below; or fits neither way and below has more room.
        below
    } else if above >= 0.0 {
        above
    } else {
        0.0
    };
    (x.max(0.0), y)
}

/// The room a list has below its trigger and above it (logical px, never negative).
pub fn room(trigger: Rect, window: (f32, f32)) -> (f32, f32) {
    let below = window.1 - (trigger.bottom() + GAP) - MARGIN;
    let above = trigger.y - GAP - MARGIN;
    (below.max(0.0), above.max(0.0))
}

/// The bounds of a list whose content is `natural` (width, height) for a trigger at `trigger`, in
/// a window `window` big, all logical px: at least the trigger's width, at most the window's,
/// at most the larger room below or above (it scrolls past that), placed by [`place`].
pub fn fit(trigger: Rect, natural: (f32, f32), window: (f32, f32)) -> Rect {
    let w = natural
        .0
        .max(trigger.w)
        .min((window.0 - 2.0 * MARGIN).max(0.0));
    let (below, above) = room(trigger, window);
    let h = natural.1.min(below.max(above));
    let (x, y) = place(trigger, (w, h), window);
    Rect::new(x, y, w, h)
}

/// `trigger`, with `popup` (when open) sized and placed by [`fit`] under it, drawn over
/// everything else in the window and taking the pointer from what it covers.
pub fn dropdown(trigger: impl IntoElement, popup: Option<AnyElement>) -> Dropdown {
    Dropdown {
        trigger: trigger.into_any_element(),
        popup,
        list: None,
        dismiss: None,
        priority: PRIORITY,
        backdrop: BACKDROP_PRIORITY,
        lint_trigger: None,
    }
}

/// An open list for [`dropdown_list`]: the list (its rows its own children), its scroll, its
/// cap (logical px, already zoomed; `crate::drop_list::MAX_H`) and its background (the fade
/// that says more is below).
pub type OpenList = (
    gpui::Stateful<gpui::Div>,
    gpui::ScrollHandle,
    f32,
    gpui::Hsla,
);

/// `trigger`, with `list` (when open) placed by [`fit`] as [`dropdown`] places a popup, but
/// scrolling itself: at most its cap and the room tall, so its `scroll` keeps a row in view
/// (`ScrollHandle::scroll_to_item` with `crate::drop_list::DropList::child_index`); while more
/// is below, its bottom fades into its background (the Settings page's overflow cue).
pub fn dropdown_list(trigger: impl IntoElement, list: Option<OpenList>) -> Dropdown {
    Dropdown {
        trigger: trigger.into_any_element(),
        popup: None,
        list,
        dismiss: None,
        priority: PRIORITY,
        backdrop: BACKDROP_PRIORITY,
        lint_trigger: None,
    }
}

pub struct Dropdown {
    trigger: AnyElement,
    popup: Option<AnyElement>,
    /// A list scrolling itself ([`dropdown_list`]).
    list: Option<OpenList>,
    dismiss: Option<OnDismiss>,
    priority: usize,
    /// Its backdrop's priority (`popover::defer_backdrop`).
    backdrop: usize,
    /// Screenshots: the open list is recorded for the layout lint as a popup of the control
    /// labelled so (`crate::layout_rec`).
    lint_trigger: Option<String>,
}

impl Dropdown {
    /// Closes the open list on a press anywhere outside it, which goes no further
    /// (`popover::defer_backdrop`, the popover rule).
    pub fn on_dismiss(mut self, dismiss: OnDismiss) -> Self {
        self.dismiss = Some(dismiss);
        self
    }

    /// Open inside a popover (the studies menu): drawn over it ([`NESTED_PRIORITY`]), its
    /// backdrop over the popover too.
    pub fn nested(mut self) -> Self {
        self.priority = NESTED_PRIORITY;
        self.backdrop = NESTED_PRIORITY - 1;
        self
    }

    /// Record the open list for the layout lint as a popup of the control labelled `trigger`
    /// (only while recording: nothing is kept otherwise).
    pub fn lint_trigger(mut self, trigger: &str) -> Self {
        if crate::layout_rec::on() {
            self.lint_trigger = Some(trigger.to_owned());
        }
        self
    }
}

impl IntoElement for Dropdown {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Dropdown {
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
        // Laid out exactly as the trigger alone: the list takes no room in the header.
        (self.trigger.request_layout(window, cx), ())
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
        self.trigger.prepaint(window, cx);
        if self.popup.is_none() && self.list.is_none() {
            return;
        }
        // The part of the chip that shows: a header clips its content before the hover
        // controls' gutter. A chip clipped away entirely opens nothing.
        let bounds = bounds.intersect(&window.content_mask().bounds);
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
            return;
        }
        // Its content's size, at least the trigger's width and at most the window's (a list's own
        // `max_w` wraps its text), at most the larger room below or above (it scrolls past that):
        // never stretched to the window ([`fit`]).
        let v = window.viewport_size();
        let win = (f32::from(v.width), f32::from(v.height));
        let trigger = Rect::new(
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );
        let (below, above) = room(trigger, win);
        let mut popup = if let Some((list, scroll, cap, bg)) = self.list.take() {
            use gpui::StatefulInteractiveElement as _;
            use gpui::prelude::FluentBuilder as _;
            use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
            // More below what shows (as of the last frame; the offset runs from 0 down to
            // -max_offset).
            let more = scroll.max_offset().y > px(0.0)
                && -scroll.offset().y < scroll.max_offset().y - px(1.0);
            let list = list
                .occlude()
                .min_w(bounds.size.width)
                .max_w(px((win.0 - 2.0 * MARGIN).max(0.0)))
                .max_h(px(below.max(above).min(cap)))
                .overflow_y_scroll()
                .track_scroll(&scroll);
            gpui::div()
                .relative()
                .flex()
                .flex_col()
                .child(list)
                .when(more, |d| {
                    d.child(
                        gpui::div()
                            .absolute()
                            .left(px(1.0))
                            .right(px(1.0))
                            .bottom(px(1.0))
                            .h(crate::zoom::ui(16.0))
                            .rounded_b_md()
                            // The list's background, fully transparent: a scroll cue, not a
                            // UI state (no state token).
                            .bg(gpui::linear_gradient(
                                180.0,
                                gpui::linear_color_stop(gpui::Hsla { a: 0.0, ..bg }, 0.0),
                                gpui::linear_color_stop(bg, 1.0),
                            )),
                    )
                })
                .into_any_element()
        } else {
            let Some(popup) = self.popup.take() else {
                return;
            };
            use gpui::StatefulInteractiveElement as _;
            use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
            // Its own id (from where its trigger is): two lists never share a scroll offset. It
            // takes the wheel; the list inside blocks every other press but lets the wheel
            // through (`block_mouse_except_scroll`), so a held-down list scrolls.
            let key = ((trigger.x.max(0.0) as u64) << 32) | trigger.y.max(0.0) as u64;
            gpui::div()
                .id(("dropdown-list", key))
                .occlude()
                .flex()
                .flex_col()
                .min_w(bounds.size.width)
                .max_w(px((win.0 - 2.0 * MARGIN).max(0.0)))
                .max_h(px(below.max(above)))
                .overflow_y_scroll()
                .child(popup)
                .into_any_element()
        };
        let s = popup.layout_as_root(
            size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
            window,
            cx,
        );
        let at = fit(trigger, (f32::from(s.width), f32::from(s.height)), win);
        let (x, y) = (at.x.round(), at.y.round());
        if let Some(trigger) = self.lint_trigger.take() {
            crate::layout_rec::record_xywh(
                window,
                (x, y, at.w, at.h),
                crate::layout_rec::Spec::new(
                    lines_core::layout_lint::Kind::Popup { trigger },
                    "dropdown list",
                )
                .layer(crate::layout_rec::POPUP),
            );
        }
        // Under the list, over everything else: a press outside closes it and does only that.
        if let Some(dismiss) = self.dismiss.take() {
            let list = Rect::new(x, y, at.w, at.h);
            crate::popover::defer_backdrop(window, cx, self.backdrop, &[list], dismiss);
        }
        // Deferred: drawn after (over) the rest of the window, and not clipped by the header
        // or the pane.
        window.defer_draw(popup, point(px(x), px(y)), self.priority, None);
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
        self.trigger.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: (f32, f32) = (1400.0, 800.0);
    /// A header chip: "5m ▾" at x 200, in a 24 px header under a 28 px tab bar.
    const CHIP: Rect = Rect {
        x: 200.0,
        y: 32.0,
        w: 34.0,
        h: 18.0,
    };

    /// A list is anchored to its trigger and stays inside the window: as wide as its content,
    /// never narrower than the trigger nor wider than the window; under the trigger, above it
    /// when there's no room below; scrolling past the larger room. The trading panel's Qty list
    /// (a field near the top right of a pane) opens under its field, not across the window.
    #[test]
    fn a_list_is_anchored_to_its_trigger_and_inside_the_window() {
        let inside = |r: Rect, win: (f32, f32)| {
            r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= win.0 + 0.01 && r.y + r.h <= win.1 + 0.01
        };
        // The panel's Qty field (the owner's screenshot): 1517,208, 66 x 24, window 1790 x 930.
        let win = (1790.0, 930.0);
        let qty = Rect::new(1517.0, 208.0, 66.0, 24.0);
        let r = fit(qty, (200.0, 140.0), win);
        assert_eq!((r.x, r.y), (1517.0, 234.0), "under the field, left-aligned");
        assert_eq!(
            (r.w, r.h),
            (200.0, 140.0),
            "its content's size, not the window's"
        );
        assert!(inside(r, win));
        // Narrower content than the trigger: as wide as the trigger.
        let wide = Rect::new(1590.0, 208.0, 190.0, 24.0);
        let r = fit(wide, (120.0, 100.0), win);
        assert_eq!(r.w, 190.0);
        assert!(
            r.x + r.w <= win.0 - MARGIN + 0.01,
            "kept in from the right edge"
        );
        // No room below: above the trigger.
        let low = Rect::new(400.0, 860.0, 60.0, 24.0);
        let r = fit(low, (150.0, 200.0), win);
        assert_eq!(
            r.y + r.h + GAP,
            low.y,
            "above, its bottom at the trigger's top"
        );
        assert!(inside(r, win));
        // Taller than either room: the larger room, and it scrolls.
        let mid = Rect::new(400.0, 300.0, 60.0, 24.0);
        let r = fit(mid, (150.0, 5000.0), win);
        assert_eq!(r.h, room(mid, win).0.max(room(mid, win).1));
        assert!(inside(r, win));
        // A narrow window (a 165 px pane popped out, 200 px wide): inside, never wider.
        let narrow = (200.0, 400.0);
        let f = Rect::new(120.0, 40.0, 60.0, 24.0);
        let r = fit(f, (300.0, 100.0), narrow);
        assert!(inside(r, narrow), "{r:?}");
        assert_eq!(r.w, narrow.0 - 2.0 * MARGIN);
        // At 150 % UI zoom (everything 1.5 x): the same rule.
        let z = Rect::new(qty.x, qty.y, qty.w * 1.5, qty.h * 1.5);
        let r = fit(z, (300.0, 210.0), win);
        assert_eq!(
            (r.x, r.y),
            (z.x.min(win.0 - MARGIN - 300.0), z.bottom() + GAP)
        );
        assert!(inside(r, win));
    }

    #[test]
    fn opens_under_the_trigger_left_aligned() {
        assert_eq!(place(CHIP, (150.0, 300.0), WIN), (200.0, 52.0));
    }

    #[test]
    fn shifts_left_only_as_far_as_the_window_edge_needs() {
        let chip = Rect { x: 1300.0, ..CHIP };
        // 1300 + 150 > 1400: the right edge sits MARGIN in from the window's.
        assert_eq!(place(chip, (150.0, 300.0), WIN), (1246.0, 52.0));
        // Within MARGIN of the edge: moved in to MARGIN.
        let chip = Rect { x: 1248.0, ..CHIP };
        assert_eq!(place(chip, (150.0, 300.0), WIN).0, 1246.0);
        // Exactly fits with MARGIN: not moved.
        let chip = Rect { x: 1246.0, ..CHIP };
        assert_eq!(place(chip, (150.0, 300.0), WIN).0, 1246.0);
        // Wider than the window: the left edge stays in it.
        assert_eq!(place(chip, (1500.0, 300.0), WIN).0, 0.0);
    }

    #[test]
    fn flips_up_only_when_there_is_no_room_below() {
        let chip = Rect { y: 700.0, ..CHIP };
        // 720 + 300 > 800, and above fits: bottom edge GAP above the trigger.
        assert_eq!(place(chip, (150.0, 300.0), WIN), (200.0, 398.0));
        // Fits below exactly: stays below.
        assert_eq!(place(chip, (150.0, 80.0), WIN).1, 720.0);
    }

    #[test]
    fn too_tall_for_either_side_takes_the_roomier_one() {
        // Near the top: below has more room.
        assert_eq!(place(CHIP, (150.0, 900.0), WIN).1, 52.0);
        // Near the bottom: pinned to the window's top.
        let chip = Rect { y: 700.0, ..CHIP };
        assert_eq!(place(chip, (150.0, 900.0), WIN).1, 0.0);
    }
}
