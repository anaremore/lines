//! The custom color picker inside the style editor (owner request, 2026-11-10): a
//! saturation-brightness square, hue and opacity sliders, a hex field and the eyedropper.
//! Pure (no GPUI): `style_editor_view` draws it and feeds pointer positions and keys back.
//!
//! Every change previews live (the style editor reports `Outcome::Preview`). Enter (or
//! clicking anywhere outside it) keeps the color, Esc puts back the one it opened with.
//!
//! Keys: ↑↓ move between Saturation, Brightness, Hue, Opacity, Hex and the dropper; ←→ change
//! the value (Shift: by ten steps); in the hex field, hex digits type and Backspace deletes;
//! Enter on the dropper arms it.

use lines_core::style::{Color, Rgb};

/// What the keyboard is on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Zone {
    Saturation,
    Brightness,
    Hue,
    Opacity,
    Hex,
    Dropper,
}

/// What a key or pointer did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Nothing to apply (moved, or not the picker's key).
    Nothing,
    /// The color or opacity changed: preview it.
    Changed,
    /// Keep the color and close.
    Keep,
    /// Put back the color it opened with and close.
    Revert,
    /// Arm the eyedropper.
    Dropper,
}

/// The picker's state while open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Custom {
    /// For the tab's zone fill (not its color).
    pub fill: bool,
    /// Hue in degrees (0..360), saturation and brightness in percent.
    pub h: u16,
    pub s: u8,
    pub v: u8,
    /// Opacity in percent; `None` for a part without one (text).
    pub opacity: Option<u8>,
    /// The hex field's text (no `#`), up to six digits.
    pub hex: String,
    /// Hex digits typed and not yet applied.
    pub typing: bool,
    pub zone: Zone,
    /// A color taken whole (opened on, typed, picked from the chart) is kept exactly, not
    /// rounded through the square; a drag or arrow key on the square or hue clears it.
    pub exact: Option<Rgb>,
    /// What it opened with (Esc puts it back).
    pub initial: (Color, Option<u8>),
}

/// Longest hex: `rrggbb`.
const HEX_LEN: usize = 6;

impl Custom {
    /// Open on `start` (the part's color, `start_rgb` as the theme shows it).
    pub fn open(fill: bool, start: Color, start_rgb: Rgb, opacity: Option<u8>) -> Custom {
        let (h, s, v) = rgb_to_hsv(start_rgb);
        Custom {
            fill,
            h,
            s,
            v,
            opacity,
            hex: hex_of(start_rgb),
            typing: false,
            zone: Zone::Saturation,
            exact: Some(start_rgb),
            initial: (start, opacity),
        }
    }

    /// The color as picked.
    pub fn color(&self) -> Rgb {
        self.exact
            .unwrap_or_else(|| from_hsv(self.h, self.s, self.v))
    }

    /// The zones the keyboard moves through, in order.
    pub fn zones(&self) -> Vec<Zone> {
        let mut z = vec![Zone::Saturation, Zone::Brightness, Zone::Hue];
        if self.opacity.is_some() {
            z.push(Zone::Opacity);
        }
        z.push(Zone::Hex);
        z.push(Zone::Dropper);
        z
    }

    fn synced(&mut self) -> Event {
        self.exact = None;
        self.hex = hex_of(self.color());
        self.typing = false;
        Event::Changed
    }

    /// The square at fractions `x` (saturation) and `y` (0 = bright) of its size.
    pub fn set_square(&mut self, x: f32, y: f32) -> Event {
        self.s = pct(x);
        self.v = 100 - pct(y);
        self.synced()
    }

    /// The hue slider at fraction `x`.
    pub fn set_hue(&mut self, x: f32) -> Event {
        self.h = ((x.clamp(0.0, 1.0) * 360.0).round() as u16).min(359);
        self.synced()
    }

    /// The opacity slider at fraction `x`.
    pub fn set_opacity(&mut self, x: f32) -> Event {
        if self.opacity.is_none() {
            return Event::Nothing;
        }
        self.opacity = Some(pct(x).max(1));
        Event::Changed
    }

    /// A color taken whole (the eyedropper, a pasted hex).
    pub fn set_color(&mut self, rgb: Rgb) -> Event {
        let (h, s, v) = rgb_to_hsv(rgb);
        // A gray has no hue: keep the slider where it was.
        if s > 0 {
            self.h = h;
        }
        self.s = s;
        self.v = v;
        self.exact = Some(rgb);
        self.hex = hex_of(rgb);
        self.typing = false;
        Event::Changed
    }

    /// A key (`key` as GPUI names it; `text` the character typed).
    pub fn key(&mut self, key: &str, shift: bool, text: Option<&str>) -> Event {
        let step: i32 = if shift { 10 } else { 1 };
        let zones = self.zones();
        let at = zones.iter().position(|z| *z == self.zone).unwrap_or(0);
        match key {
            "escape" => return Event::Revert,
            "enter" if self.zone == Zone::Hex && self.typing => {
                return match Rgb::parse(&self.hex) {
                    Some(rgb) => self.set_color(rgb),
                    None => Event::Nothing,
                };
            }
            "enter" | "space" if self.zone == Zone::Dropper => return Event::Dropper,
            "enter" => return Event::Keep,
            "up" => {
                self.zone = zones[at.saturating_sub(1)];
                return Event::Nothing;
            }
            "down" => {
                self.zone = zones[(at + 1).min(zones.len() - 1)];
                return Event::Nothing;
            }
            "left" | "right" => {
                let d = if key == "left" { -step } else { step };
                return match self.zone {
                    Zone::Saturation => {
                        self.s = add_pct(self.s, d);
                        self.synced()
                    }
                    Zone::Brightness => {
                        self.v = add_pct(self.v, d);
                        self.synced()
                    }
                    Zone::Hue => {
                        // Degrees: a step is 2°, Shift 20°; it wraps around the wheel.
                        self.h = (i32::from(self.h) + d * 2).rem_euclid(360) as u16;
                        self.synced()
                    }
                    Zone::Opacity => match self.opacity {
                        Some(o) => {
                            self.opacity = Some(add_pct(o, d).max(1));
                            Event::Changed
                        }
                        None => Event::Nothing,
                    },
                    Zone::Hex | Zone::Dropper => Event::Nothing,
                };
            }
            "backspace" if self.zone == Zone::Hex => {
                if !self.typing {
                    self.hex.clear();
                    self.typing = true;
                } else {
                    self.hex.pop();
                }
                return Event::Nothing;
            }
            _ => {}
        }
        if self.zone == Zone::Hex
            && let Some(c) = text.and_then(|t| {
                let mut ch = t.chars();
                ch.next().filter(|_| ch.next().is_none())
            })
            && c.is_ascii_hexdigit()
        {
            if !self.typing {
                self.hex.clear();
                self.typing = true;
            }
            if self.hex.len() < HEX_LEN {
                self.hex.push(c.to_ascii_lowercase());
            }
            // Six digits apply at once (a live preview), as a drag does.
            if self.hex.len() == HEX_LEN
                && let Some(rgb) = Rgb::parse(&self.hex)
            {
                return self.set_color(rgb);
            }
        }
        Event::Nothing
    }
}

fn pct(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 100.0).round() as u8
}

fn add_pct(p: u8, d: i32) -> u8 {
    (i32::from(p) + d).clamp(0, 100) as u8
}

/// `rrggbb`, lowercase.
pub fn hex_of(c: Rgb) -> String {
    format!("{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Hue (degrees), saturation and brightness (percent) of a color.
pub fn rgb_to_hsv(c: Rgb) -> (u16, u8, u8) {
    let (r, g, b) = (
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (
        (h.round() as u16) % 360,
        (s * 100.0).round() as u8,
        (max * 100.0).round() as u8,
    )
}

/// The color at a hue (degrees), saturation and brightness (percent).
pub fn from_hsv(h: u16, s: u8, v: u8) -> Rgb {
    let s = f32::from(s.min(100)) / 100.0;
    let v = f32::from(v.min(100)) / 100.0;
    let h = f32::from(h % 360) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to = |u: f32| ((u + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgb {
        r: to(r),
        g: to(g),
        b: to(b),
    }
}

/// Recent custom colors: `rgb` moved to the front, at most ten, no repeats.
pub fn remember(recent: &mut Vec<Rgb>, rgb: Rgb) {
    recent.retain(|c| *c != rgb);
    recent.insert(0, rgb);
    recent.truncate(RECENT);
}

/// How many recent colors are kept.
pub const RECENT: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(h: u32) -> Rgb {
        Rgb::hex(h)
    }

    #[test]
    fn hsv_round_trips_within_a_step() {
        for c in [
            0xff8a1f, 0x1d2a6b, 0x8c7bff, 0x000000, 0xffffff, 0x808080, 0x3fcf9a, 0xff6b7f,
        ] {
            let (h, s, v) = rgb_to_hsv(rgb(c));
            let back = from_hsv(h, s, v);
            let orig = rgb(c);
            for (a, b) in [(back.r, orig.r), (back.g, orig.g), (back.b, orig.b)] {
                assert!(
                    (i32::from(a) - i32::from(b)).abs() <= 3,
                    "{c:06x}: {back:?}"
                );
            }
        }
        assert_eq!(rgb_to_hsv(rgb(0xff0000)), (0, 100, 100));
        assert_eq!(rgb_to_hsv(rgb(0x00ff00)), (120, 100, 100));
        assert_eq!(from_hsv(240, 100, 100), rgb(0x0000ff));
    }

    /// Owner request: ↑↓ move between the zones, ←→ change the value (Shift by ten), every
    /// change previews, Enter keeps, Esc puts back.
    #[test]
    fn the_keyboard_reaches_every_part_of_the_picker() {
        let mut p = Custom::open(false, Color::Palette(1), rgb(0xff0000), Some(100));
        assert_eq!(p.zone, Zone::Saturation);
        assert_eq!(p.key("left", true, None), Event::Changed);
        assert_eq!(p.s, 90);
        assert_eq!(p.hex, hex_of(p.color()));
        p.key("down", false, None);
        assert_eq!(p.zone, Zone::Brightness);
        assert_eq!(p.key("left", false, None), Event::Changed);
        assert_eq!(p.v, 99);
        p.key("down", false, None);
        assert_eq!(p.key("left", false, None), Event::Changed);
        assert_eq!(p.h, 358, "the hue wraps");
        p.key("down", false, None);
        assert_eq!(p.zone, Zone::Opacity);
        assert_eq!(p.key("left", true, None), Event::Changed);
        assert_eq!(p.opacity, Some(90));
        p.key("down", false, None);
        assert_eq!(p.zone, Zone::Hex);
        for d in ["1", "d", "2", "a", "6"] {
            assert_eq!(p.key(d, false, Some(d)), Event::Nothing);
        }
        assert_eq!(
            p.key("b", false, Some("b")),
            Event::Changed,
            "six digits apply"
        );
        assert_eq!(p.color(), rgb(0x1d2a6b));
        p.key("down", false, None);
        assert_eq!(p.zone, Zone::Dropper);
        assert_eq!(p.key("enter", false, None), Event::Dropper);
        p.key("down", false, None);
        assert_eq!(p.zone, Zone::Dropper, "stays on the last");
        assert_eq!(p.key("up", false, None), Event::Nothing);
        assert_eq!(p.key("enter", false, None), Event::Keep);
        assert_eq!(p.key("escape", false, None), Event::Revert);
        assert_eq!(p.initial, (Color::Palette(1), Some(100)));
        // Text has no opacity: no Opacity zone.
        let t = Custom::open(false, Color::Text, rgb(0xeeeeee), None);
        assert!(!t.zones().contains(&Zone::Opacity));
    }

    #[test]
    fn the_square_and_sliders_take_pointer_fractions() {
        let mut p = Custom::open(true, Color::Palette(0), rgb(0x808080), Some(40));
        assert_eq!(p.set_square(1.0, 0.0), Event::Changed);
        assert_eq!((p.s, p.v), (100, 100));
        assert_eq!(p.set_hue(0.5), Event::Changed);
        assert_eq!(p.h, 180);
        assert_eq!(p.color(), rgb(0x00ffff));
        assert_eq!(p.set_opacity(0.0), Event::Changed);
        assert_eq!(p.opacity, Some(1), "never fully transparent");
        // A gray taken whole keeps the hue slider where it was.
        p.set_color(rgb(0x404040));
        assert_eq!(p.h, 180);
    }

    #[test]
    fn recent_colors_are_ten_newest_first_without_repeats() {
        let mut r = Vec::new();
        for i in 0..12u32 {
            remember(&mut r, rgb(i));
        }
        assert_eq!(r.len(), RECENT);
        assert_eq!(r[0], rgb(11));
        remember(&mut r, rgb(5));
        assert_eq!(r[0], rgb(5));
        assert_eq!(r.iter().filter(|c| **c == rgb(5)).count(), 1);
    }
}
