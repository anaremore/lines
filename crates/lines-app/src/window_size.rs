//! Secondary windows' sizes follow the UI zoom (owner report, 2026-11-19): a window whose
//! sizes are given at 100 % (Settings, Download days, the main window's minimum) opens at
//! them times the zoom, never larger than the monitor's work area, and a zoom raised while it
//! is open grows it to fit (never shrinks it; past the work area its content scrolls). GPUI
//! sets a window's minimum only when it opens: a minimum follows the zoom from the next
//! opening.

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

/// The work area of the display the window is on.
pub fn work_area(window: &Window, cx: &App) -> Option<(f32, f32)> {
    window.display(cx).map(|d| {
        let b = d.visible_bounds();
        (f32::from(b.size.width), f32::from(b.size.height))
    })
}

/// After a zoom change: grow the window to `base` at the zoom now, within its display's work
/// area. Returns the new size if it changed.
pub fn grow_to_zoom(window: &mut Window, base: (f32, f32), cx: &App) -> Option<(f32, f32)> {
    let v = window.viewport_size();
    let cur = (f32::from(v.width), f32::from(v.height));
    let to = grown(cur, base, crate::zoom::get(), work_area(window, cx))?;
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
