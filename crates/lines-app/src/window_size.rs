//! Secondary windows' sizes follow the UI zoom (owner report, 2026-11-19): a window whose
//! sizes are given at 100 % (Settings, Download days, the main window's minimum) opens at
//! them times the zoom, never larger than the monitor's work area, and a zoom raised while it
//! is open grows it to fit (never shrinks it; past the work area its content scrolls). GPUI
//! sets a window's minimum only when it opens: a minimum follows the zoom from the next
//! opening. GPUI can't move a window either: growth keeps the top-left corner where it is and
//! stops at the work area's right and bottom edges (the whole window, frame included, stays
//! on it). A maximized or full-screen window is left alone.

use gpui::{App, Window, px, size};

/// `base` (logical px at 100 %) at `zoom`, no larger than `work` (the work area, if known).
pub fn scaled(base: (f32, f32), zoom: f32, work: Option<(f32, f32)>) -> (f32, f32) {
    let (w, h) = (base.0 * zoom, base.1 * zoom);
    match work {
        Some((ww, wh)) => (w.min(ww), h.min(wh)),
        None => (w, h),
    }
}

/// The size a window `cur` wide and high grows to for `base` at `zoom`: each side the larger
/// of what it is and the scaled size, never past the work area; `None` when it already fits.
pub fn grown(
    cur: (f32, f32),
    base: (f32, f32),
    zoom: f32,
    work: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    let (tw, th) = scaled(base, zoom, work);
    let mut w = cur.0.max(tw);
    let mut h = cur.1.max(th);
    if let Some((ww, wh)) = work {
        // Never past the work area, but never shrunk below what the trader made it.
        w = w.min(ww.max(cur.0));
        h = h.min(wh.max(cur.1));
    }
    ((w - cur.0).abs() > 0.5 || (h - cur.1).abs() > 0.5).then_some((w, h))
}

/// The primary display's work area (logical px), where a new window opens.
pub fn primary_work_area(cx: &App) -> Option<(f32, f32)> {
    cx.primary_display().map(|d| {
        let b = d.visible_bounds();
        (f32::from(b.size.width), f32::from(b.size.height))
    })
}

/// Rectangles as (x, y, w, h), logical px, in screen coordinates.
pub type Rect = (f32, f32, f32, f32);

/// The most content a window can have without leaving the work area, its top-left corner
/// staying put: the room from its outer corner to the work area's right and bottom edges,
/// less its frame (title bar, borders: the outer size less the content size).
pub fn room(outer: Rect, content: (f32, f32), work: Rect) -> (f32, f32) {
    let frame = (
        (outer.2 - content.0).max(0.0),
        (outer.3 - content.1).max(0.0),
    );
    (
        (work.0 + work.2 - outer.0 - frame.0).max(0.0),
        (work.1 + work.3 - outer.1 - frame.1).max(0.0),
    )
}

fn rect(b: gpui::Bounds<gpui::Pixels>) -> Rect {
    (
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

/// After a zoom change: grow the window to `base` at the zoom now, its whole outer window
/// staying inside its display's work area (`room`); a maximized or full-screen one is left
/// alone. Returns the new content size if it changed.
pub fn grow_to_zoom(window: &mut Window, base: (f32, f32), cx: &App) -> Option<(f32, f32)> {
    if window.is_maximized() || window.is_fullscreen() {
        return None;
    }
    let v = window.viewport_size();
    let cur = (f32::from(v.width), f32::from(v.height));
    let cap = window
        .display(cx)
        .map(|d| room(rect(window.bounds()), cur, rect(d.visible_bounds())));
    let to = grown(cur, base, crate::zoom::get(), cap)?;
    window.resize(size(px(to.0), px(to.1)));
    Some(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_scale_with_the_zoom_within_the_work_area() {
        assert_eq!(scaled((860.0, 620.0), 1.0, None), (860.0, 620.0));
        assert_eq!(scaled((860.0, 620.0), 1.5, None), (1290.0, 930.0));
        // A small screen: never past its work area.
        assert_eq!(
            scaled((860.0, 620.0), 1.5, Some((1366.0, 728.0))),
            (1290.0, 728.0)
        );
    }

    /// The room a window has to grow: to the work area's right and bottom edges from where it
    /// sits, less its frame; on a second monitor whose origin isn't (0, 0) too.
    #[test]
    fn room_keeps_the_whole_window_on_the_work_area() {
        // 1920×1040 work area; Settings centered at the top 210 px down, a 31 px title bar.
        let work = (0.0, 0.0, 1920.0, 1040.0);
        let outer = (530.0, 210.0, 862.0, 651.0);
        let r = room(outer, (860.0, 620.0), work);
        assert_eq!(r, (1920.0 - 530.0 - 2.0, 1040.0 - 210.0 - 31.0));
        // At 150 % it grows to 1290 wide but only as tall as fits below its top.
        assert_eq!(
            grown((860.0, 620.0), (860.0, 620.0), 1.5, Some(r)),
            Some((1290.0, 799.0))
        );
        // In the bottom-right corner: no room to grow.
        let corner = (1920.0 - 862.0, 1040.0 - 651.0, 862.0, 651.0);
        assert_eq!(
            grown(
                (860.0, 620.0),
                (860.0, 620.0),
                1.5,
                Some(room(corner, (860.0, 620.0), work))
            ),
            None
        );
        // A second monitor to the right (origin 1920, 0).
        let right = (1920.0, 0.0, 2560.0, 1400.0);
        let outer = (2200.0, 100.0, 862.0, 651.0);
        assert_eq!(
            room(outer, (860.0, 620.0), right),
            (1920.0 + 2560.0 - 2200.0 - 2.0, 1400.0 - 100.0 - 31.0)
        );
    }

    /// A zoom raised while open grows the window; one lowered never shrinks it; a window the
    /// trader made larger than the work area allows stays as it is.
    #[test]
    fn a_window_grows_with_the_zoom_and_never_shrinks() {
        let work = Some((1920.0, 1040.0));
        assert_eq!(
            grown((860.0, 620.0), (860.0, 620.0), 1.5, work),
            Some((1290.0, 930.0))
        );
        assert_eq!(grown((1290.0, 930.0), (860.0, 620.0), 1.0, work), None);
        // Wider by hand already: only the height grows.
        assert_eq!(
            grown((1500.0, 620.0), (860.0, 620.0), 1.25, work),
            Some((1500.0, 775.0))
        );
        // A small screen caps the growth.
        assert_eq!(
            grown((860.0, 620.0), (860.0, 620.0), 1.5, Some((1280.0, 680.0))),
            Some((1280.0, 680.0))
        );
        // Bigger than the work area already: left alone.
        assert_eq!(
            grown((2000.0, 1200.0), (860.0, 620.0), 1.5, Some((1280.0, 680.0))),
            None
        );
    }
}
