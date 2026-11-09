//! App-wide UI zoom (Ctrl+= / Ctrl+− / Ctrl+0; Settings › Appearance): text, row heights,
//! headers, the DOM ladder and time & sales, menus, popovers, the picker and Settings. Never
//! the chart's time or price scale (the wheel and drag). One factor for every window, popped-out
//! ones included, saved as the optional `ui_zoom` setting.
//!
//! Element trees scale through the window's rem size ([`rem`], set at the top of each window's
//! render: `px_2`, `gap_1`, `text_xs`, …) and [`ui`] for an explicit size. Painted rows and text
//! take [`f`] of their 100 % constant. Both read one atomic: no allocation, no lock, in paint.
//! A change bumps [`generation`] so views drop layout and shaped text cached at the old size.
//!
//! Tests never set the factor (they run in parallel and assume 100 %): the pure step logic is
//! tested on its own.

use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{Pixels, px};

/// The zoom steps, in percent.
pub const STEPS: [u16; 6] = [80, 90, 100, 110, 125, 150];
pub const DEFAULT: u16 = 100;

static PERCENT: AtomicU32 = AtomicU32::new(DEFAULT as u32);
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// The factor now (1.0 at 100 %).
#[inline]
pub fn get() -> f32 {
    PERCENT.load(Ordering::Relaxed) as f32 / 100.0
}

/// The percentage now.
pub fn percent() -> u16 {
    PERCENT.load(Ordering::Relaxed) as u16
}

/// A size given at 100 %, at the zoom now.
#[inline]
pub fn f(v: f32) -> f32 {
    v * get()
}

/// [`f`] as pixels: an element's explicit size.
#[inline]
pub fn ui(v: f32) -> Pixels {
    px(f(v))
}

/// The windows' rem size (GPUI's default, 16 px, at 100 %).
#[inline]
pub fn rem() -> Pixels {
    ui(16.0)
}

/// Bumped on every change: a view keeps the generation its caches were built at.
#[inline]
pub fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// Set the zoom (a step; anything else is the nearest step). Whether it changed.
pub fn set(pct: u16) -> bool {
    let pct = nearest(pct);
    let before = PERCENT.swap(pct as u32, Ordering::Relaxed);
    if before == pct as u32 {
        return false;
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    true
}

/// `--ui-zoom`: one of [`STEPS`], nothing else.
pub fn parse_step(s: &str) -> Result<u16, String> {
    s.trim()
        .trim_end_matches('%')
        .parse::<u16>()
        .ok()
        .filter(|p| STEPS.contains(p))
        .ok_or_else(|| format!("{s}: expected one of 80, 90, 100, 110, 125, 150"))
}

/// The step nearest `pct` (a hand-edited setting of 112 is 110).
pub fn nearest(pct: u16) -> u16 {
    *STEPS
        .iter()
        .min_by_key(|s| (i32::from(**s) - i32::from(pct)).abs())
        .expect("steps")
}

/// What Zoom in (`dir` 1), Zoom out (-1) or Reset zoom (0) makes of `pct`; the ends hold.
pub fn step(pct: u16, dir: i8) -> u16 {
    if dir == 0 {
        return DEFAULT;
    }
    let i = STEPS.iter().position(|s| *s == nearest(pct)).unwrap_or(2) as isize;
    let j = (i + isize::from(dir)).clamp(0, STEPS.len() as isize - 1);
    STEPS[j as usize]
}

/// The saved form: absent at the default.
pub fn saved(pct: u16) -> Option<u16> {
    (pct != DEFAULT).then_some(pct)
}

/// The settings row's choices, and a step's index among them.
pub const LABELS: [&str; 6] = ["80%", "90%", "100%", "110%", "125%", "150%"];

pub fn index(pct: u16) -> usize {
    STEPS.iter().position(|s| *s == nearest(pct)).unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_hold_at_the_ends_and_reset_to_100() {
        assert_eq!(step(100, 1), 110);
        assert_eq!(step(110, 1), 125);
        assert_eq!(step(125, 1), 150);
        assert_eq!(step(150, 1), 150, "the top holds");
        assert_eq!(step(100, -1), 90);
        assert_eq!(step(80, -1), 80, "the bottom holds");
        assert_eq!(step(150, 0), 100);
        assert_eq!(step(112, 1), 125, "from the nearest step");
    }

    #[test]
    fn the_flag_takes_a_step_only() {
        assert_eq!(parse_step("125"), Ok(125));
        assert_eq!(parse_step("150%"), Ok(150));
        assert!(parse_step("7").is_err());
        assert!(parse_step("300").is_err());
        assert!(parse_step("abc").is_err());
    }

    #[test]
    fn a_hand_edited_value_reads_as_the_nearest_step() {
        assert_eq!(nearest(0), 80);
        assert_eq!(nearest(95), 90);
        assert_eq!(nearest(140), 150);
        assert_eq!(nearest(1000), 150);
        assert_eq!(saved(100), None, "the default isn't written");
        assert_eq!(saved(125), Some(125));
        assert_eq!(LABELS[index(125)], "125%");
        assert_eq!(STEPS.len(), LABELS.len());
    }
}
