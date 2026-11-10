//! The tab bar's strip (owner-approved tabs mock): fixed-width tabs that scroll sideways when
//! they don't fit, the active tab kept in view, and where a dragged tab would land. Pure and
//! tested; `workspace::tab_bar` draws what it says, and hover, clicks and drops use the same
//! geometry, so the pointer always hits what is drawn.

/// The strip's geometry (logical px, at the UI zoom).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Strip {
    /// Where the first tab starts, from the window's left edge.
    pub x0: f32,
    /// One tab's width (tabs never shrink: titles don't re-truncate as tabs come and go).
    pub tab_w: f32,
    /// The visible width for tabs (the window, less the new-tab button and the ▾ list).
    pub room: f32,
    /// How many tabs.
    pub n: usize,
}

impl Strip {
    /// The strip for a window `width` wide: `controls` is the new-tab button's width and `more`
    /// the ▾ list's, which shows only when the tabs don't fit.
    pub fn new(width: f32, x0: f32, tab_w: f32, controls: f32, more: f32, n: usize) -> Strip {
        let room = (width - x0 - controls).max(0.0);
        let room = if n as f32 * tab_w > room {
            (room - more).max(0.0)
        } else {
            room
        };
        Strip { x0, tab_w, room, n }
    }

    /// The tabs don't all fit: the strip scrolls and the ▾ list shows.
    pub fn overflows(&self) -> bool {
        self.n as f32 * self.tab_w > self.room
    }

    pub fn max_scroll(&self) -> f32 {
        (self.n as f32 * self.tab_w - self.room).max(0.0)
    }

    pub fn clamp(&self, scroll: f32) -> f32 {
        scroll.clamp(0.0, self.max_scroll())
    }

    /// The scroll that shows tab `i` whole, moving as little as possible.
    pub fn keep_in_view(&self, scroll: f32, i: usize) -> f32 {
        let left = i as f32 * self.tab_w;
        let s = if left < scroll {
            left
        } else if left + self.tab_w > scroll + self.room {
            left + self.tab_w - self.room
        } else {
            scroll
        };
        self.clamp(s)
    }

    /// The scroll held after a × closed a tab: unchanged (never more than the tabs' width), even
    /// past the new end, so no tab moves under the pointer; the usual clamp returns once the
    /// pointer leaves the bar (`tabstate::CloseGuard`).
    pub fn held(&self, scroll: f32) -> f32 {
        scroll.clamp(0.0, self.n as f32 * self.tab_w)
    }

    /// The x a tab drop reads (window coordinates): inside this window's tab bar, held to the
    /// strip, so the ▾, + or left padding never pick a tab scrolled out of view; anywhere else
    /// (beside or above the window, over the panes) as it is, so a drop on another window or a
    /// tear-off still goes where the pointer is.
    pub fn drop_x(&self, x: f32, y: f32, window_w: f32, bar_h: f32) -> f32 {
        let in_bar = (0.0..window_w).contains(&x) && (0.0..bar_h).contains(&y);
        if in_bar {
            x.clamp(self.x0, (self.x0 + self.room - 1.0).max(self.x0))
        } else {
            x
        }
    }

    /// A wheel turn of `delta` px (positive scrolls toward the last tab).
    pub fn wheel(&self, scroll: f32, delta: f32) -> f32 {
        self.clamp(scroll + delta)
    }

    /// Tab `i`'s left edge in the window.
    pub fn left_of(&self, i: usize, scroll: f32) -> f32 {
        self.x0 + i as f32 * self.tab_w - scroll
    }

    /// The tab under window x `x`, only where the strip shows it.
    pub fn tab_at(&self, x: f32, scroll: f32) -> Option<usize> {
        if x < self.x0 || x >= self.x0 + self.room {
            return None;
        }
        let i = ((x - self.x0 + scroll) / self.tab_w).floor();
        (i >= 0.0 && (i as usize) < self.n).then_some(i as usize)
    }

    /// Each tab's right edge in the window, as drops read them (`wsdoc::tab_drop`).
    pub fn rights(&self, scroll: f32) -> Vec<f32> {
        (0..self.n)
            .map(|i| self.left_of(i, scroll) + self.tab_w)
            .collect()
    }

    /// Where the insertion marker goes while tab `from` is dragged over the tab at `to` (the
    /// slot it would take): before it when moving left, after it when moving right; inside the
    /// strip.
    pub fn marker_x(&self, from: usize, to: usize, scroll: f32) -> f32 {
        let x = if to < from {
            self.left_of(to, scroll)
        } else {
            self.left_of(to, scroll) + self.tab_w
        };
        x.clamp(self.x0, self.x0 + self.room)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(n: usize, width: f32) -> Strip {
        Strip::new(width, 8.0, 100.0, 24.0, 24.0, n)
    }

    #[test]
    fn tabs_that_fit_never_scroll() {
        let s = strip(3, 1000.0);
        assert!(!s.overflows());
        assert_eq!(s.max_scroll(), 0.0);
        assert_eq!(s.room, 1000.0 - 8.0 - 24.0, "no ▾ when they fit");
        assert_eq!(s.wheel(0.0, 500.0), 0.0);
        assert_eq!(s.tab_at(8.0 + 150.0, 0.0), Some(1));
        assert_eq!(s.tab_at(8.0 + 350.0, 0.0), None, "past the last tab");
        assert_eq!(s.tab_at(4.0, 0.0), None);
    }

    #[test]
    fn overflow_scrolls_keeps_the_active_tab_in_view_and_hits_follow() {
        let s = strip(10, 500.0);
        assert!(s.overflows());
        assert_eq!(
            s.room,
            500.0 - 8.0 - 24.0 - 24.0,
            "the ▾ list takes its width"
        );
        assert_eq!(s.max_scroll(), 1000.0 - s.room);
        // The wheel scrolls within the strip's ends.
        assert_eq!(s.wheel(0.0, -50.0), 0.0);
        assert_eq!(s.wheel(0.0, 10_000.0), s.max_scroll());
        // The last tab, activated, comes into view whole; the first goes back to 0.
        let at = s.keep_in_view(0.0, 9);
        assert_eq!(at, s.max_scroll());
        assert!(s.left_of(9, at) + s.tab_w <= s.x0 + s.room + 0.01);
        assert_eq!(s.keep_in_view(at, 0), 0.0);
        // A tab already in view doesn't move the strip.
        assert_eq!(s.keep_in_view(150.0, 2), 150.0);
        // Hits are where the tabs are drawn, scrolled.
        let x = s.left_of(4, 150.0) + 1.0;
        assert_eq!(s.tab_at(x, 150.0), Some(4));
        assert_eq!(s.rights(150.0)[4], s.left_of(5, 150.0));
        // Past the strip's visible end (over the ▾): no tab.
        assert_eq!(s.tab_at(s.x0 + s.room + 1.0, 150.0), None);
    }

    /// Closing the last tab with × at the strip's end: the held scroll keeps every remaining
    /// tab where it was drawn, so the pointer's slot keeps the tab that slid into it.
    #[test]
    fn a_close_at_the_end_moves_no_tab_while_held() {
        let before = strip(10, 500.0);
        let s = before.max_scroll();
        let after = strip(9, 500.0);
        assert!(after.clamp(s) < s, "the plain clamp would shift every tab");
        let held = after.held(s);
        for i in 0..9 {
            assert_eq!(after.left_of(i, held), before.left_of(i, s));
        }
    }

    /// A drop inside this window's tab bar is held to the strip; beside or above the window
    /// (another window's tab bar at the same height, a tear-off) the pointer is read as it is.
    #[test]
    fn a_drop_is_held_to_the_strip_only_inside_this_windows_bar() {
        let s = strip(10, 500.0);
        let end = s.x0 + s.room - 1.0;
        assert_eq!(s.drop_x(495.0, 10.0, 500.0, 30.0), end, "over the ▾ or +");
        assert_eq!(s.drop_x(2.0, 10.0, 500.0, 30.0), s.x0, "the left padding");
        assert_eq!(s.drop_x(100.0, 10.0, 500.0, 30.0), 100.0, "on a tab");
        assert_eq!(
            s.drop_x(800.0, 10.0, 500.0, 30.0),
            800.0,
            "beside: another window"
        );
        assert_eq!(
            s.drop_x(-50.0, 10.0, 500.0, 30.0),
            -50.0,
            "beside, to the left"
        );
        assert_eq!(
            s.drop_x(495.0, -20.0, 500.0, 30.0),
            495.0,
            "above the window"
        );
        assert_eq!(s.drop_x(495.0, 200.0, 500.0, 30.0), 495.0, "over the panes");
        // A strip with no room never panics.
        let z = Strip::new(10.0, 8.0, 100.0, 24.0, 24.0, 3);
        assert_eq!(z.drop_x(5.0, 10.0, 10.0, 30.0), z.x0);
    }

    #[test]
    fn the_insertion_marker_is_where_the_tab_would_land() {
        let s = strip(4, 1000.0);
        // Moving tab 3 left over tab 1: before tab 1.
        assert_eq!(s.marker_x(3, 1, 0.0), s.left_of(1, 0.0));
        // Moving tab 0 right over tab 2: after tab 2.
        assert_eq!(s.marker_x(0, 2, 0.0), s.left_of(3, 0.0));
        // Never outside the strip.
        let o = strip(10, 500.0);
        assert!(o.marker_x(0, 9, 0.0) <= o.x0 + o.room);
    }
}
