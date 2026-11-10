//! Allocation-free text for panes that repaint many changing values (DOM, T&S, footprint,
//! journal, chain, the chart's order tags).
//!
//! Fonts (decision 0034, `crate::fonts`): a string with no letter (a price, size, time, P&L,
//! "—", "▲") is a number and is in IBM Plex Mono, whatever the UI font; a string with a
//! letter (a header, a message, a contract, "Long", "3 ticks") is words and is in the UI font,
//! so the UI font setting reaches it. [`is_words`] decides, once per string, for measuring and
//! painting alike. Both are shaped with tabular digits (`tnum`).
//!
//! Shaping a string allocates, and a DOM's sizes or a tape's times change every frame, so a
//! per-value label cache would miss constantly. Instead each *character* is shaped once per
//! (colour, weight) with tabular digits (`tnum`), and strings are painted glyph by glyph at
//! the shaped advances. After the first frame painting a new number allocates nothing on our
//! side (docs/design.md §2). Digits, separators and the few markers used here have no kerning
//! that matters, so this is pixel-identical to shaping the whole string.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, Font, FontFeatures, FontWeight, Hsla, Pixels, ShapedLine, SharedString, TextAlign,
    TextRun, Window, point, px,
};

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct Key {
    ch: char,
    color: [u32; 4],
    bold: bool,
    /// In the UI font (part of words), else the number font.
    words: bool,
}

/// Whether `s` is words (in the UI font) rather than a number (in IBM Plex Mono): it has an
/// ASCII letter. No allocation.
pub fn is_words(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_alphabetic())
}

pub struct Glyphs {
    /// The text size at 100 % (`crate::zoom`); [`Glyphs::size`] is it at the zoom now.
    base: f32,
    size: f32,
    /// The zoom generation the cache was shaped at: another one clears it.
    generation: u32,
    /// Follows the UI zoom (else always `base`).
    zooms: bool,
    /// The number font and the UI font (`[mono, ui]`), each made on first use.
    fonts: [Option<Font>; 2],
    cache: HashMap<Key, ShapedLine>,
}

fn color_key(c: Hsla) -> [u32; 4] {
    [c.h.to_bits(), c.s.to_bits(), c.l.to_bits(), c.a.to_bits()]
}

impl Glyphs {
    /// Glyphs of `size` at 100 %, shaped at the UI zoom (`crate::zoom`).
    pub fn new(size: f32) -> Glyphs {
        Glyphs {
            base: size,
            size: crate::zoom::f(size),
            generation: crate::zoom::generation(),
            zooms: true,
            fonts: [None, None],
            cache: HashMap::with_capacity(256),
        }
    }

    /// Glyphs of `size` at every zoom: for text in boxes whose geometry doesn't zoom (the
    /// chart's order and position tags, laid out by `lines_core::geometry::order_tags`).
    pub fn fixed(size: f32) -> Glyphs {
        Glyphs {
            size,
            zooms: false,
            ..Glyphs::new(size)
        }
    }

    /// The text size now: the base at the zoom now. A zoom change, or a UI font change (which
    /// bumps the same generation, even for fixed-size glyphs), drops what was shaped before
    /// (once; nothing is allocated until glyphs are shaped again).
    pub fn size(&mut self) -> f32 {
        let g = crate::zoom::generation();
        if g != self.generation {
            self.generation = g;
            if self.zooms {
                self.size = crate::zoom::f(self.base);
            }
            self.cache.clear();
            self.fonts[1] = None;
        }
        self.size
    }

    fn glyph(
        &mut self,
        ch: char,
        color: Hsla,
        bold: bool,
        words: bool,
        window: &mut Window,
    ) -> &ShapedLine {
        self.size();
        let key = Key {
            ch,
            color: color_key(color),
            bold,
            words,
        };
        if !self.cache.contains_key(&key) {
            let mut font = self.fonts[usize::from(words)]
                .get_or_insert_with(|| {
                    // Numbers are IBM Plex Mono whatever the UI font: every digit the same
                    // width, so columns line up. `tnum` keeps digits even in the UI font and
                    // in a glyph that falls back to another font.
                    let mut f = if words {
                        crate::fonts::ui()
                    } else {
                        crate::fonts::mono()
                    };
                    f.features = FontFeatures(Arc::new(vec![("tnum".into(), 1)]));
                    f
                })
                .clone();
            if bold {
                font.weight = FontWeight::SEMIBOLD;
            }
            let mut buf = [0u8; 4];
            let text = ch.encode_utf8(&mut buf);
            let run = TextRun {
                len: text.len(),
                font,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                SharedString::from(text.to_owned()),
                px(self.size),
                &[run],
                None,
            );
            self.cache.insert(key, line);
        }
        &self.cache[&key]
    }

    /// Width of `s` in logical px (in its font: [`is_words`]).
    pub fn width(&mut self, s: &str, color: Hsla, bold: bool, window: &mut Window) -> f32 {
        let words = is_words(s);
        s.chars()
            .map(|c| f32::from(self.glyph(c, color, bold, words, window).width))
            .sum()
    }

    /// Paint `s` with its left edge at `x`, vertically centred in a row whose top is `y` and
    /// height `row_h`. Returns the right edge.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        s: &str,
        x: f32,
        y: f32,
        row_h: f32,
        color: Hsla,
        bold: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        let size = self.size();
        let line_h = px(size * 1.3);
        let top = y + (row_h - size * 1.3) * 0.5;
        let mut at = x;
        let words = is_words(s);
        for c in s.chars() {
            let g = self.glyph(c, color, bold, words, window);
            let w = f32::from(g.width);
            let _ = g.paint(
                point(px(at), px(top)),
                line_h,
                TextAlign::Left,
                None,
                window,
                cx,
            );
            at += w;
        }
        if crate::layout_rec::on() {
            crate::layout_rec::record_xywh(
                window,
                (x, top, at - x, size * 1.3),
                crate::layout_rec::Spec::text(s)
                    .owner(crate::layout_rec::owner())
                    .layer(crate::layout_rec::layer()),
            );
        }
        at
    }

    /// Paint `s` right-aligned to `right` (numbers line up on their last digit).
    #[allow(clippy::too_many_arguments)]
    pub fn paint_right(
        &mut self,
        s: &str,
        right: f32,
        y: f32,
        row_h: f32,
        color: Hsla,
        bold: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let w = self.width(s, color, bold, window);
        self.paint(s, right - w, y, row_h, color, bold, window, cx);
    }
}

/// `Pixels` → `f32` shorthand.
pub fn f(p: Pixels) -> f32 {
    f32::from(p)
}

#[cfg(test)]
mod tests {
    use super::is_words;

    /// Numbers (and their marks) in Plex Mono; anything with a letter in the UI font.
    #[test]
    fn a_letter_makes_words() {
        for n in [
            "6000.75",
            "10:29:59.873",
            "-$1,250.00",
            "12",
            "—",
            "▲",
            "+3",
            "≈ 2.41",
            "",
        ] {
            assert!(!is_words(n), "{n:?} is a number");
        }
        for w in [
            "Price",
            "no trades yet",
            "ESZ26",
            "Long",
            "3 ticks",
            "Time ET",
            "1.2K",
        ] {
            assert!(is_words(w), "{w:?} is words");
        }
    }
}
