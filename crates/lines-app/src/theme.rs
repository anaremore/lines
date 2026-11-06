//! Lines' colour themes (docs/design.md §5). Colours are tokens here and nowhere else; views
//! use the tokens, never literals. Themes are data so screenshots can pin one (decision 0009).
//!
//! Built from the brand palette: ink `#11141B`, signal violet `#8C7BFF`, paper `#ECEAF4`.
//! - Dark (primary) is ink-based; light is paper-based.
//! - Violet is the UI accent only: focus, selection, active state, and the last-price tag.
//!   It is never up/down, so market colours mean only market direction.
//! - Up/down are Lines' own (mint/rose; blue/orange in the colourblind preset), not another
//!   platform's, and every text token meets WCAG AA (4.5:1) on its backgrounds (tested below).
//! - Up/down also differ by shape, not only colour: one direction's candle bodies are hollow
//!   ([`HollowBodies`]), so the chart reads correctly in greyscale or with colour blindness.

#[cfg(test)]
use gpui::Rgba;
use gpui::{Hsla, rgb};

/// Brand palette.
pub mod brand {
    pub const INK: u32 = 0x11141b;
    pub const VIOLET: u32 = 0x8c7bff;
    pub const PAPER: u32 = 0xeceaf4;
}

/// Which candle bodies are drawn hollow (outlined) rather than filled.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum HollowBodies {
    /// Rising candles are hollow, falling are filled (the classic candlestick convention).
    #[default]
    Up,
    /// Falling candles are hollow, rising are filled.
    Down,
    // Deliberately no "all filled" option: up/down must never rely on colour alone
    // (docs/design.md §4).
}

impl HollowBodies {
    pub fn is_hollow(self, up: bool) -> bool {
        match self {
            HollowBodies::Up => up,
            HollowBodies::Down => !up,
        }
    }
}

/// Up/down colour scheme.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Palette {
    /// Mint up, rose down.
    #[default]
    Standard,
    /// Blue up, orange down: distinguishable with red-green colour blindness
    /// (deuteranopia/protanopia), the common forms.
    Colorblind,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Hsla,
    pub panel: Hsla,
    pub grid: Hsla,
    pub axis_line: Hsla,
    pub text_dim: Hsla,
    pub text_main: Hsla,
    pub up: Hsla,
    pub down: Hsla,
    /// Last-price tag and line.
    pub last: Hsla,
    /// Text drawn on top of `last`.
    pub on_last: Hsla,
    /// Crosshair lines.
    pub crosshair: Hsla,
    /// Crosshair price/time tags on the axes, and the text on them.
    pub cross_tag: Hsla,
    pub on_cross_tag: Hsla,
    /// Brand accent (signal violet): focus, selection, active state such as manual price
    /// scale. Never used for up/down.
    pub accent: Hsla,
    /// Shape cue for direction (in addition to colour).
    pub hollow: HollowBodies,
    /// DOM: depth bars behind bid / ask sizes (subtle; text sits on them).
    pub bid_bar: Hsla,
    pub ask_bar: Hsla,
    /// DOM: the best bid / best ask cells (inside market), stronger than the bars.
    pub inside_bid: Hsla,
    pub inside_ask: Hsla,
    /// DOM: traded-volume bars.
    pub volume_bar: Hsla,
    /// T&S: background of a large print (with bold text).
    pub large_print: Hsla,
    /// Link group swatches A..D (decision 0016). Always shown with the group's letter, so a
    /// group never relies on colour alone; none of them is violet or up/down.
    pub link: [Hsla; 4],
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Theme {
    /// Ink-based dark theme (primary).
    pub fn dark(palette: Palette) -> Self {
        let (up, down) = match palette {
            Palette::Standard => (0x3fcf9a, 0xff6b7f),   // mint, rose
            Palette::Colorblind => (0x4fa8ff, 0xff9f1a), // blue, orange
        };
        Theme {
            name: match palette {
                Palette::Standard => "dark",
                Palette::Colorblind => "dark-colorblind",
            },
            bg: c(brand::INK),
            panel: c(0x171b24),
            grid: c(0x20242f),
            axis_line: c(0x2a3040),
            text_dim: c(0x9a9fb2),
            text_main: c(brand::PAPER),
            up: c(up),
            down: c(down),
            last: c(brand::VIOLET),
            on_last: c(brand::INK),
            crosshair: c(brand::PAPER).opacity(0.45),
            cross_tag: c(0x2b3142),
            on_cross_tag: c(brand::PAPER),
            accent: c(brand::VIOLET),
            hollow: HollowBodies::Up,
            bid_bar: c(up).opacity(0.16),
            ask_bar: c(down).opacity(0.16),
            inside_bid: c(up).opacity(0.30),
            inside_ask: c(down).opacity(0.30),
            volume_bar: c(0x9a9fb2).opacity(0.18),
            large_print: c(0x262b39),
            // amber, teal, orchid, slate
            link: [c(0xe0a84a), c(0x3fb8c4), c(0xc77ddb), c(0xb8bccb)],
        }
    }

    /// Paper-based light theme. Violet is darkened (same hue) to keep AA on light grounds.
    pub fn light(palette: Palette) -> Self {
        let (up, down) = match palette {
            Palette::Standard => (0x0a7350, 0xc42b4c), // deep mint, deep rose
            Palette::Colorblind => (0x0a63b8, 0xa64b00), // deep blue, deep orange
        };
        let violet = 0x5b47e0;
        Theme {
            name: match palette {
                Palette::Standard => "light",
                Palette::Colorblind => "light-colorblind",
            },
            bg: c(0xfbfafd),
            panel: c(brand::PAPER),
            grid: c(0xe4e2ec),
            axis_line: c(0xcdcad9),
            text_dim: c(0x565b6e),
            text_main: c(brand::INK),
            up: c(up),
            down: c(down),
            last: c(violet),
            on_last: c(0xffffff),
            crosshair: c(brand::INK).opacity(0.45),
            cross_tag: c(0x2e3344),
            on_cross_tag: c(brand::PAPER),
            accent: c(violet),
            hollow: HollowBodies::Up,
            bid_bar: c(up).opacity(0.12),
            ask_bar: c(down).opacity(0.12),
            inside_bid: c(up).opacity(0.22),
            inside_ask: c(down).opacity(0.22),
            volume_bar: c(0x565b6e).opacity(0.14),
            large_print: c(brand::PAPER),
            link: [c(0x9a6a10), c(0x13808b), c(0x8d3fa3), c(0x5d6275)],
        }
    }

    /// Every built-in theme/preset combination.
    #[cfg(test)]
    pub fn all() -> [Theme; 4] {
        [
            Theme::dark(Palette::Standard),
            Theme::dark(Palette::Colorblind),
            Theme::light(Palette::Standard),
            Theme::light(Palette::Colorblind),
        ]
    }
}

/// `top` (possibly translucent) composited over opaque `bottom`, as drawn.
#[cfg(test)]
pub fn over(top: Hsla, bottom: Hsla) -> Hsla {
    let (t, b) = (Rgba::from(top), Rgba::from(bottom));
    let a = t.a;
    let mix = |x: f32, y: f32| x * a + y * (1.0 - a);
    Hsla::from(Rgba {
        r: mix(t.r, b.r),
        g: mix(t.g, b.g),
        b: mix(t.b, b.b),
        a: 1.0,
    })
}

/// WCAG 2.x relative luminance.
#[cfg(test)]
pub fn luminance(c: Hsla) -> f64 {
    let Rgba { r, g, b, .. } = Rgba::from(c);
    let lin = |v: f32| {
        let v = v as f64;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG contrast ratio (1..=21).
#[cfg(test)]
pub fn contrast(a: Hsla, b: Hsla) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every (text token, background) pair that appears in the UI. Up/down count as text
    /// (prices and changes coloured by direction) on both the chart and panel backgrounds.
    fn text_pairs(t: &Theme) -> Vec<(&'static str, Hsla, &'static str, Hsla)> {
        vec![
            ("text_main", t.text_main, "bg", t.bg),
            ("text_main", t.text_main, "panel", t.panel),
            ("text_dim", t.text_dim, "bg", t.bg),
            ("text_dim", t.text_dim, "panel", t.panel),
            ("up", t.up, "bg", t.bg),
            ("up", t.up, "panel", t.panel),
            ("down", t.down, "bg", t.bg),
            ("down", t.down, "panel", t.panel),
            ("accent", t.accent, "bg", t.bg),
            ("accent", t.accent, "panel", t.panel),
            ("on_last", t.on_last, "last", t.last),
            ("on_cross_tag", t.on_cross_tag, "cross_tag", t.cross_tag),
            // DOM: sizes and prices over depth bars and inside-market cells.
            (
                "text_main",
                t.text_main,
                "bid_bar/bg",
                over(t.bid_bar, t.bg),
            ),
            (
                "text_main",
                t.text_main,
                "ask_bar/bg",
                over(t.ask_bar, t.bg),
            ),
            (
                "text_main",
                t.text_main,
                "inside_bid/bg",
                over(t.inside_bid, t.bg),
            ),
            (
                "text_main",
                t.text_main,
                "inside_ask/bg",
                over(t.inside_ask, t.bg),
            ),
            (
                "text_main",
                t.text_main,
                "volume_bar/bg",
                over(t.volume_bar, t.bg),
            ),
            (
                "text_dim",
                t.text_dim,
                "volume_bar/bg",
                over(t.volume_bar, t.bg),
            ),
            // T&S: aggressor-coloured prints, also on a large-print row.
            ("up", t.up, "large_print", t.large_print),
            ("down", t.down, "large_print", t.large_print),
            ("text_main", t.text_main, "large_print", t.large_print),
        ]
    }

    #[test]
    fn every_text_token_meets_wcag_aa_in_every_theme() {
        let mut failures = Vec::new();
        for t in Theme::all() {
            for (fg_name, fg, bg_name, bg) in text_pairs(&t) {
                let r = contrast(fg, bg);
                if r < 4.5 {
                    failures.push(format!("{}: {fg_name} on {bg_name} = {r:.2}", t.name));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "below WCAG AA 4.5:1:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn contrast_formula_matches_known_values() {
        let (black, white) = (c(0x000000), c(0xffffff));
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 1e-9);
        // Brand violet on ink: 5.61:1 (computed independently from the sRGB hex values).
        assert!((contrast(c(brand::VIOLET), c(brand::INK)) - 5.61).abs() < 0.02);
        // And the check discriminates: brand violet on paper is only ~3.0:1, which is why the
        // light theme uses a darker violet of the same hue.
        assert!(contrast(c(brand::VIOLET), c(brand::PAPER)) < 4.5);
    }

    #[test]
    fn violet_is_never_up_or_down_and_shape_differs() {
        for t in Theme::all() {
            assert_ne!(t.up, t.accent, "{}", t.name);
            assert_ne!(t.down, t.accent, "{}", t.name);
            assert_ne!(t.up, t.down, "{}", t.name);
            // Exactly one direction is hollow in every theme: shape differs, not only colour.
            assert_ne!(
                t.hollow.is_hollow(true),
                t.hollow.is_hollow(false),
                "{}",
                t.name
            );
        }
    }

    #[test]
    fn not_another_platforms_colours() {
        // TradingView's current and older defaults; Lines has its own look (docs/design.md).
        let borrowed = [
            0x089981u32,
            0xf23645,
            0x2962ff,
            0x26a69a,
            0xef5350,
            0x22ab94,
            0xf7525f,
        ];
        for t in Theme::all() {
            for (name, tok) in [
                ("up", t.up),
                ("down", t.down),
                ("last", t.last),
                ("accent", t.accent),
            ] {
                for b in borrowed {
                    assert_ne!(tok, c(b), "{}: {name} is #{b:06x}", t.name);
                }
            }
        }
    }

    #[test]
    fn link_colours_are_distinct_and_never_market_or_accent_colours() {
        for t in Theme::all() {
            for (i, l) in t.link.iter().enumerate() {
                for (name, tok) in [("up", t.up), ("down", t.down), ("accent", t.accent)] {
                    assert_ne!(*l, tok, "{}: link {i} is {name}", t.name);
                }
                for (j, m) in t.link.iter().enumerate() {
                    assert!(i == j || l != m, "{}: links {i} and {j}", t.name);
                }
                // Swatches are non-text, but must still stand out from the header (3:1).
                assert!(
                    contrast(*l, t.panel) >= 3.0,
                    "{}: link {i} on panel = {:.2}",
                    t.name,
                    contrast(*l, t.panel)
                );
            }
        }
    }

    #[test]
    fn hollow_selection() {
        assert!(HollowBodies::Up.is_hollow(true) && !HollowBodies::Up.is_hollow(false));
        assert!(HollowBodies::Down.is_hollow(false) && !HollowBodies::Down.is_hollow(true));
    }

    #[test]
    fn views_use_tokens_not_colour_literals() {
        // docs/design.md §5: all colours come from theme tokens.
        for (file, src) in [
            ("chart_view.rs", include_str!("chart_view.rs")),
            ("dom_view.rs", include_str!("dom_view.rs")),
            ("tape_view.rs", include_str!("tape_view.rs")),
            ("workspace.rs", include_str!("workspace.rs")),
            ("pane.rs", include_str!("pane.rs")),
            ("picker_view.rs", include_str!("picker_view.rs")),
            ("bench.rs", include_str!("bench.rs")),
            ("text.rs", include_str!("text.rs")),
            ("main.rs", include_str!("main.rs")),
            ("screenshot.rs", include_str!("screenshot.rs")),
        ] {
            for needle in ["rgb(", "rgba(", "hsla(", "Hsla {"] {
                assert!(
                    !src.contains(needle),
                    "{file} contains a colour literal ({needle})"
                );
            }
        }
    }
}
