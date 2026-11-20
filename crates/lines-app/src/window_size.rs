//! Secondary windows' sizes follow the UI zoom (owner report, 2026-11-19): a window whose
//! sizes are given at 100 % (Settings, Download days, the main window's minimum) opens at
//! them times the zoom, never larger than the monitor's work area, and a zoom raised while it
//! is open follows it both ways: each window's size is kept in 100 % units ([`Fit`]), so
//! zooming in and back out returns it exactly, and a size the trader dragged it to is kept
//! relative to the zoom. GPUI sets a window's minimum only when it opens: a minimum follows
//! the zoom from the next opening (a smaller zoom never takes a window below its minimum at
//! that zoom). GPUI can't move a window either: growth keeps the top-left corner where it is
//! and stops at the work area's right and bottom edges (the whole window, frame included,
//! stays on it). A maximized or full-screen window is left alone.

use gpui::{App, Window, px, size};

/// `base` (logical px at 100 %) at `zoom`, no larger than `work` (the work area, if known).
pub fn scaled(base: (f32, f32), zoom: f32, work: Option<(f32, f32)>) -> (f32, f32) {
    let (w, h) = (base.0 * zoom, base.1 * zoom);
    match work {
        Some((ww, wh)) => (w.min(ww), h.min(wh)),
        None => (w, h),
    }
}

/// A window's size kept in 100 % units (owner, 2026-11-19): its pixel size is that times the
/// zoom, so zooming in and back out returns it exactly to where it was, and a size the
/// trader dragged it to is kept relative to the zoom.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Fit {
    /// Content size at 100 %.
    base: (f32, f32),
    /// The content size last given to the window (or it opened at).
    applied: (f32, f32),
    /// The zoom `applied` is for.
    zoom: f32,
}

impl Fit {
    /// A window that opened with `content` at `zoom`.
    pub fn new(content: (f32, f32), zoom: f32) -> Fit {
        Fit {
            base: (content.0 / zoom, content.1 / zoom),
            applied: content,
            zoom,
        }
    }

    pub fn applied(&self) -> (f32, f32) {
        self.applied
    }

    /// The zoom is now `zoom`: the content size the window takes, if it differs from `cur`
    /// (its size now; `None` when the window can't say, a hidden one). A `cur` other than
    /// what was last applied is the trader's resize: it becomes the 100 % size (divided by
    /// the zoom it was made at). The new size is the 100 % size times the zoom, never below
    /// `min` (at 100 %) times the zoom; growing, never past `room` (the content the work area
    /// has room for from where the window sits; never shrunk by it either).
    pub fn rezoom(
        &mut self,
        cur: Option<(f32, f32)>,
        zoom: f32,
        min: (f32, f32),
        room: Option<(f32, f32)>,
    ) -> Option<(f32, f32)> {
        let cur = cur.unwrap_or(self.applied);
        let moved = |a: f32, b: f32| (a - b).abs() > 0.5;
        if moved(cur.0, self.applied.0) || moved(cur.1, self.applied.1) {
            self.base = (cur.0 / self.zoom, cur.1 / self.zoom);
        }
        let side = |base: f32, min: f32, cur: f32, room: Option<f32>| {
            let want = (base * zoom).max(min * zoom);
            match room {
                // Growing: no further than the work area has room for (a window already past
                // it isn't shrunk for that).
                Some(r) if want > cur => want.min(r.max(cur)),
                _ => want,
            }
        };
        let to = (
            side(self.base.0, min.0, cur.0, room.map(|r| r.0)),
            side(self.base.1, min.1, cur.1, room.map(|r| r.1)),
        );
        self.zoom = zoom;
        self.applied = to;
        (moved(to.0, cur.0) || moved(to.1, cur.1)).then_some(to)
    }
}

/// Each open window's [`Fit`], by window.
static FITS: std::sync::Mutex<Vec<(gpui::WindowId, Fit)>> = std::sync::Mutex::new(Vec::new());

/// A window opened with `content` at the zoom now: remember it for [`fit_to_zoom`].
pub fn remember(id: gpui::WindowId, content: (f32, f32)) {
    let mut f = FITS.lock().unwrap_or_else(|e| e.into_inner());
    f.retain(|(w, _)| *w != id);
    f.push((id, Fit::new(content, crate::zoom::get())));
}

/// A window closed.
pub fn forget(id: gpui::WindowId) {
    FITS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(w, _)| *w != id);
}

/// The size a window was last given (screenshots: a hidden window reports none).
pub fn applied(id: gpui::WindowId) -> Option<(f32, f32)> {
    FITS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(w, _)| *w == id)
        .map(|(_, f)| f.applied())
}

/// What a window opens at: `base` (100 %) times the zoom, within the primary display's work
/// area when it is shown (`shown`).
pub fn opening(base: (f32, f32), shown: bool, cx: &App) -> (f32, f32) {
    let work = if shown { primary_work_area(cx) } else { None };
    scaled(base, crate::zoom::get(), work)
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

/// After a zoom change: the window takes its size at the new zoom ([`Fit::rezoom`]: back
/// down when zoomed out, up when zoomed in, the trader's resize kept relative to the zoom),
/// never below `min` (100 %) times the zoom, its whole outer window staying inside its
/// display's work area when it grows (`room`); a maximized or full-screen one is left alone.
/// Returns the new content size if it changed.
pub fn fit_to_zoom(window: &mut Window, min: (f32, f32), cx: &App) -> Option<(f32, f32)> {
    let id = window.window_handle().window_id();
    let v = window.viewport_size();
    let cur = (f32::from(v.width), f32::from(v.height));
    // A hidden window (screenshots) reports no size of its own.
    let cur = (cur.0 >= 1.0 && cur.1 >= 1.0).then_some(cur);
    let fixed = window.is_maximized() || window.is_fullscreen();
    let room = window
        .display(cx)
        .zip(cur)
        .map(|(d, c)| room(rect(window.bounds()), c, rect(d.visible_bounds())));
    let mut fits = FITS.lock().unwrap_or_else(|e| e.into_inner());
    let i = match fits.iter().position(|(w, _)| *w == id) {
        Some(i) => i,
        // Not remembered at opening: from its size now, at the zoom it was made at unknown,
        // so taken as made at this zoom (nothing changes now; later changes follow).
        None => {
            fits.push((id, Fit::new(cur?, crate::zoom::get())));
            return None;
        }
    };
    let fit = &mut fits[i].1;
    if fixed {
        // Kept, at the new zoom: a restored window comes back to the size for the zoom then.
        let z = crate::zoom::get();
        *fit = Fit {
            base: fit.base,
            applied: cur.unwrap_or(fit.applied),
            zoom: z,
        };
        return None;
    }
    let to = fit.rezoom(cur, crate::zoom::get(), min, room)?;
    drop(fits);
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
        let mut f = Fit::new((860.0, 620.0), 1.0);
        assert_eq!(
            f.rezoom(Some((860.0, 620.0)), 1.5, MIN, Some(r)),
            Some((1290.0, 799.0))
        );
        // In the bottom-right corner: no room to grow.
        let corner = (1920.0 - 862.0, 1040.0 - 651.0, 862.0, 651.0);
        let mut f = Fit::new((860.0, 620.0), 1.0);
        assert_eq!(
            f.rezoom(
                Some((860.0, 620.0)),
                1.5,
                MIN,
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

    const MIN: (f32, f32) = (640.0, 420.0);

    /// Zoom 100 → 150 → 100 returns the window to exactly its size, also when the work area
    /// held the growth back.
    #[test]
    fn zooming_in_and_back_out_returns_the_exact_size() {
        let mut f = Fit::new((860.0, 620.0), 1.0);
        let up = f.rezoom(Some((860.0, 620.0)), 1.5, MIN, None).unwrap();
        assert_eq!(up, (1290.0, 930.0));
        assert_eq!(f.rezoom(Some(up), 1.0, MIN, None), Some((860.0, 620.0)));
        // Held back by the work area on the way up: back to the same size on the way down.
        let mut f = Fit::new((860.0, 620.0), 1.0);
        let up = f
            .rezoom(Some((860.0, 620.0)), 1.5, MIN, Some((1280.0, 680.0)))
            .unwrap();
        assert_eq!(up, (1280.0, 680.0));
        assert_eq!(f.rezoom(Some(up), 1.0, MIN, None), Some((860.0, 620.0)));
        // Every step there and back.
        let mut f = Fit::new((860.0, 620.0), 1.0);
        let mut cur = (860.0, 620.0);
        for z in [1.1, 1.25, 1.5, 1.25, 1.1, 1.0, 0.9, 0.8, 1.0] {
            if let Some(to) = f.rezoom(Some(cur), z, MIN, None) {
                cur = to;
            }
        }
        assert_eq!(cur, (860.0, 620.0));
    }

    /// A size the trader dragged to at 150 % is kept relative to the zoom: back at 100 % it is
    /// two thirds of it; never below the minimum at the zoom.
    #[test]
    fn a_dragged_size_is_kept_relative_to_the_zoom() {
        let mut f = Fit::new((860.0, 620.0), 1.0);
        f.rezoom(Some((860.0, 620.0)), 1.5, MIN, None);
        // Dragged to 1500 × 1050 at 150 %.
        assert_eq!(
            f.rezoom(Some((1500.0, 1050.0)), 1.0, MIN, None),
            Some((1000.0, 700.0))
        );
        // Smaller than the minimum (a window that opened smaller on a small screen), zoomed
        // to 80 %: never below the minimum at 80 %.
        let mut f = Fit::new((860.0, 620.0), 1.0);
        assert_eq!(
            f.rezoom(Some((600.0, 400.0)), 0.8, MIN, None),
            Some((640.0 * 0.8, 420.0 * 0.8))
        );
        // A hidden window (no size of its own; Download days, minimum 400 × 340): the last
        // size given is its size.
        let mut f = Fit::new((930.0, 690.0), 1.5);
        assert_eq!(
            f.rezoom(None, 1.0, (400.0, 340.0), None),
            Some((620.0, 460.0))
        );
    }

    /// Growing never goes past the room the work area has, and a window already past it
    /// isn't shrunk for that.
    #[test]
    fn growth_stays_in_the_room_and_never_shrinks_for_it() {
        let mut f = Fit::new((2000.0, 1200.0), 1.0);
        // Zoomed to 110 % with less room than it already has: it stays (never shrunk by
        // the room), and back at 100 % it is as it was.
        assert_eq!(
            f.rezoom(Some((2000.0, 1200.0)), 1.1, MIN, Some((1280.0, 680.0))),
            None
        );
        assert_eq!(f.rezoom(Some((2000.0, 1200.0)), 1.0, MIN, None), None);
    }
}
