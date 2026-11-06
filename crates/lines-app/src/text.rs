//! Allocation-free number text for panes that repaint many changing values (DOM, T&S).
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
}

pub struct Glyphs {
    size: f32,
    font: Option<Font>,
    cache: HashMap<Key, ShapedLine>,
}

fn color_key(c: Hsla) -> [u32; 4] {
    [c.h.to_bits(), c.s.to_bits(), c.l.to_bits(), c.a.to_bits()]
}

impl Glyphs {
    pub fn new(size: f32) -> Glyphs {
        Glyphs {
            size,
            font: None,
            cache: HashMap::with_capacity(256),
        }
    }

    fn glyph(&mut self, ch: char, color: Hsla, bold: bool, window: &mut Window) -> &ShapedLine {
        let key = Key {
            ch,
            color: color_key(color),
            bold,
        };
        if !self.cache.contains_key(&key) {
            let mut font = self
                .font
                .get_or_insert_with(|| {
                    let mut f = window.text_style().font();
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

    /// Width of `s` in logical px.
    pub fn width(&mut self, s: &str, color: Hsla, bold: bool, window: &mut Window) -> f32 {
        s.chars()
            .map(|c| f32::from(self.glyph(c, color, bold, window).width))
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
        let line_h = px(self.size * 1.3);
        let top = y + (row_h - self.size * 1.3) * 0.5;
        let mut at = x;
        for c in s.chars() {
            let g = self.glyph(c, color, bold, window);
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
