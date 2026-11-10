//! Dropdowns: a trigger (a header chip) and, while open, its list directly under it.
//!
//! The rule (docs/invariants.md, lines-app UI): the list's left edge lines up with the trigger's left edge and
//! its top sits `GAP` px under the trigger's bottom. It moves only when it would leave the
//! window: left just far enough to keep `MARGIN` from the right edge, and up above the trigger when
//! there is no room below. `place` is that rule; [`dropdown`] applies it with the trigger's
//! real bounds, in window coordinates, so a list escapes its pane's header and the pane itself.

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
/// under Flatten and Cancel all (`LIVE_PRIORITY`) and every list and menu.
pub const BACKDROP_PRIORITY: usize = 1;
/// Deferred-draw priority of a trading panel's own lists (Qty, Bracket): over the backdrops and
/// the panes, under Flatten and Cancel all, which they open beside (docs/design.md §3: those
/// stay one press and in view with any popover open).
pub const PANEL_PRIORITY: usize = 2;
/// Deferred-draw priority of Flatten and Cancel all (`popover::always_live`): over the
/// backdrops and the panel's lists, so they stay one press with a popover open; under the
/// other lists.
pub const LIVE_PRIORITY: usize = 3;
/// Deferred-draw priority of the window's own overlays (pane and tab menus, palette and
/// prompts, the switch-all banner): over any open list, so a list never hides or takes the
/// clicks of a menu opened after it.
pub const OVERLAY_PRIORITY: usize = 5;

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

/// `trigger`, with `popup` (when open) placed by [`place`] under it, drawn over everything
/// else in the window and taking the pointer from what it covers.
pub fn dropdown(trigger: impl IntoElement, popup: Option<AnyElement>) -> Dropdown {
    Dropdown {
        trigger: trigger.into_any_element(),
        popup,
        dismiss: None,
        priority: PRIORITY,
    }
}

pub struct Dropdown {
    trigger: AnyElement,
    popup: Option<AnyElement>,
    dismiss: Option<OnDismiss>,
    priority: usize,
}

impl Dropdown {
    /// A trading panel's list ([`PANEL_PRIORITY`]): drawn under Flatten and Cancel all.
    pub fn in_panel(mut self) -> Self {
        self.priority = PANEL_PRIORITY;
        self
    }

    /// Closes the open list on a press anywhere outside it, which goes no further
    /// (`popover::defer_backdrop`, the popover rule).
    pub fn on_dismiss(mut self, dismiss: OnDismiss) -> Self {
        self.dismiss = Some(dismiss);
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
        let Some(mut popup) = self.popup.take() else {
            return;
        };
        // The part of the chip that shows: a header clips its content before the hover
        // controls' gutter. A chip clipped away entirely opens nothing.
        let bounds = bounds.intersect(&window.content_mask().bounds);
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
            return;
        }
        // Its natural size, at most the window's width (a list's own `max_w` wraps its text).
        let v = window.viewport_size();
        let s = popup.layout_as_root(
            size(
                AvailableSpace::Definite(v.width),
                AvailableSpace::MaxContent,
            ),
            window,
            cx,
        );
        let trigger = Rect::new(
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );
        let (x, y) = place(
            trigger,
            (f32::from(s.width), f32::from(s.height)),
            (f32::from(v.width), f32::from(v.height)),
        );
        let (x, y) = (x.round(), y.round());
        // Under the list, over everything else: a press outside closes it and does only that.
        if let Some(dismiss) = self.dismiss.take() {
            let list = Rect::new(x, y, f32::from(s.width), f32::from(s.height));
            crate::popover::defer_backdrop(window, cx, BACKDROP_PRIORITY, &[list], dismiss);
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
