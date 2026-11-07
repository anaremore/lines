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
use lines_core::style::{Color, DashPattern, LineType, Rgb};

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
    /// A session's start on tick charts: a faint line behind the bars (stronger than `grid`).
    pub session_divider: Hsla,
    pub axis_line: Hsla,
    /// A control's edge where the edge is what shows it (unselected chips, checkboxes,
    /// swatch rims, the style editor's warning box): 3:1 on `panel` and `bg` (WCAG 1.4.11).
    pub control_edge: Hsla,
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
    /// Study lines (VWAP, …), picked per instance. Never up/down or the accent.
    pub study: [Hsla; 4],
    /// Drawings (roadmap 11: lines, zones, notes, measures), picked per drawing
    /// (`lines_core::drawings::Style::color`). Perceptually clear (CIEDE2000 >= 20) of every
    /// color with a meaning (up/down in both palettes, accent/last, warn, bracket target and
    /// stop) and at least 3:1 against the chart background (WCAG 1.4.11): tested.
    pub drawing: [Hsla; 4],
    /// The drawing colors' names in the style editor (shown beside the color row's label, so a
    /// swatch never needs its color to be told apart).
    pub drawing_names: [&'static str; 4],
    /// Behind text drawn over the plot (study legends, the volume profile's tag): a slightly
    /// translucent panel box, so lines and profile rows never run through the text. Its text
    /// meets AA over any chart content below it (`label_box_text_meets_aa_over_chart_content`).
    pub label_bg: Hsla,
    /// Chart volume profile: rows inside the value area (rows outside it use `volume_bar`),
    /// the point of control, and the value-area edges. Neutral and translucent, drawn behind
    /// the candles; never the accent, the up/down or the study colours.
    pub profile_va: Hsla,
    pub profile_poc: Hsla,
    pub profile_edge: Hsla,
    /// Volume profile split by aggressor: buyer- and seller-initiated volume (direction
    /// colours, translucent; the two are also told apart by position: sells are always
    /// drawn from the profile's base edge).
    pub profile_buy: Hsla,
    pub profile_sell: Hsla,
    /// Trading: a warning cell (a prop rule near its limit or reached, a rejection), always
    /// with words, and the text on it. Not up/down, not the accent.
    pub warn: Hsla,
    pub on_warn: Hsla,
    /// The PAPER badge beside a paper account's name: an inverted chip (the word carries it;
    /// live accounts get a different badge).
    pub paper_badge: Hsla,
    pub on_paper_badge: Hsla,
    /// DOM orders column: a bracket's take-profit and stop-loss (entries use the side's
    /// up/down colour). Text on them is `bg`. They also differ by letter (T / S) and shape.
    pub order_target: Hsla,
    pub order_stop: Hsla,
}

/// A dash pattern, logical px: `on` drawn, `off` skipped.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Dash {
    pub on: f32,
    pub off: f32,
}

/// The chart's open-position line at the average price.
pub const POSITION_DASH: Dash = Dash { on: 6.0, off: 4.0 };
/// A held order (bracket exit waiting for its entry) in the DOM's orders column.
pub const HELD_DASH: Dash = Dash { on: 3.0, off: 3.0 };
/// Dashed drawings: a long dash no trading state uses, so a drawn level never reads as a
/// position or an order (tested against the two above).
pub const DRAWING_DASH: Dash = Dash { on: 12.0, off: 4.0 };

/// A drawing's (or a styled study line's) dash pattern for its line type and width (logical
/// px). Dashed is [`DRAWING_DASH`]; dotted dots are as long as the line is wide (at least 2
/// px) with twice the width plus 4 px between; dash-dot is a long dash, a dot, and gaps. None of
/// them matches [`POSITION_DASH`] or [`HELD_DASH`] (tested). No allocation.
pub fn drawing_dash(line: LineType, width: f32) -> DashPattern {
    let w = width.max(1.0);
    match line {
        LineType::Solid => DashPattern::SOLID,
        LineType::Dashed => DashPattern {
            seg: [DRAWING_DASH.on, DRAWING_DASH.off, 0.0, 0.0],
            n: 2,
        },
        // Dots at least 2 px long, so a thin dotted line still reads (on dark especially),
        // with wide gaps: never the even rhythm of a held order (3/3).
        LineType::Dotted => DashPattern {
            seg: [w.max(2.0), 2.0 * w + 4.0, 0.0, 0.0],
            n: 2,
        },
        LineType::DashDot => DashPattern {
            seg: [12.0, 3.0 + w, w, 3.0 + w],
            n: 4,
        },
    }
}

/// Starting points for a custom color in the style editor (decision 0026), with their names.
/// These are *user data presets*, not theme tokens: once chosen, a color is saved with the
/// drawing as the user's own. Picked to read as a line (3:1) on both themes' charts and to stay
/// clear (CIEDE2000 >= 20) of every color with a meaning in all four theme/palette combinations
/// (tested): no market up/down colors among them; any color is still one "Custom…" away.
pub const CUSTOM_SWATCHES: [(u32, &str); 2] = [(0xf719cb, "Magenta"), (0x9c8756, "Khaki")];

/// A user-chosen color (decision 0026: user data saved with a drawing or study, never a
/// literal in a view), at `opacity` percent.
pub fn user_color(c: Rgb, opacity: u8) -> Hsla {
    gpui::Rgba {
        r: f32::from(c.r) / 255.0,
        g: f32::from(c.g) / 255.0,
        b: f32::from(c.b) / 255.0,
        a: f32::from(opacity.min(100)) / 100.0,
    }
    .into()
}

/// A token's sRGB (opaque; its own alpha is ignored), for the readability guard.
pub fn rgb_of(c: Hsla) -> Rgb {
    let c = c.to_rgb();
    Rgb::from_f32(c.r, c.g, c.b)
}

impl Theme {
    /// A style color in this theme: palette indices from `palette` (drawings:
    /// [`Theme::drawing`]), `Text` the main text color, custom colors as saved.
    pub fn style_color(&self, c: Color, palette: &[Hsla], opacity: u8) -> Hsla {
        let a = f32::from(opacity.min(100)) / 100.0;
        match c {
            Color::Palette(i) if !palette.is_empty() => {
                // `opacity` scales the token's own alpha.
                palette[usize::from(i) % palette.len()].opacity(a)
            }
            Color::Palette(_) | Color::Text => self.text_main.opacity(a),
            Color::Custom(rgb) => user_color(rgb, opacity),
        }
    }

    /// Colors with a meaning, which a user color shouldn't resemble (decision 0026): this
    /// theme and palette's up and down, the violet last price / accent, the bracket target and
    /// stop, and warn. Names as the style editor words them ("the {name} color").
    pub fn meanings(&self) -> [(&'static str, Rgb); 6] {
        [
            ("up", rgb_of(self.up)),
            ("down", rgb_of(self.down)),
            ("last-price", rgb_of(self.last)),
            ("order target", rgb_of(self.order_target)),
            ("order stop", rgb_of(self.order_stop)),
            ("warning", rgb_of(self.warn)),
        ]
    }

    pub fn is_dark(&self) -> bool {
        self.name.starts_with("dark")
    }

    /// This theme as the style editor's guard sees it (`crate::style_editor::ThemeCtx`).
    pub fn style_ctx(&self) -> crate::style_editor::ThemeCtx {
        crate::style_editor::ThemeCtx {
            dark: self.is_dark(),
            colorblind: self.name.ends_with("colorblind"),
            palette: self.drawing.map(rgb_of),
            text: rgb_of(self.text_main),
            chart: rgb_of(self.bg),
            panel: rgb_of(self.panel),
            meanings: self.meanings(),
        }
    }

    /// Text on a swatch filled with `fill` (the editor's "current" dot): the theme's ink or
    /// paper, whichever reads better.
    pub fn on_fill(&self, fill: Hsla) -> Hsla {
        let f = rgb_of(fill);
        let (a, b) = (self.text_main, self.bg);
        let k = |x: Hsla| lines_core::style::contrast(f, rgb_of(x));
        if k(a) >= k(b) { a } else { b }
    }

    /// A drawing's price tag on the axis (decision 0026): outlined, never filled (filled tags
    /// are the last price's and orders'). Its border is the drawing's color, opaque; its text
    /// the same color where that is AA (4.5:1) on the axis background, else the theme's text
    /// color. Returns (border, text).
    pub fn drawing_tag(&self, color: Hsla) -> (Hsla, Hsla) {
        let mut solid = color;
        solid.a = 1.0;
        let reads = lines_core::style::contrast(rgb_of(solid), rgb_of(self.bg)) >= 4.5;
        (solid, if reads { solid } else { self.text_main })
    }

    /// A note's text drawn in `color` (opaque), its box asked for or not: (draw the box, text
    /// color). Text below AA on the chart gets its box anyway (the saved style is unchanged);
    /// text below AA on the box is drawn in the theme's text color.
    pub fn note_text(&self, color: Hsla, boxed: bool) -> (bool, Hsla) {
        let k = |bg: Hsla| lines_core::style::contrast(rgb_of(color), rgb_of(bg));
        if !boxed && k(self.bg) >= 4.5 {
            return (false, color);
        }
        (
            true,
            if k(self.panel) >= 4.5 {
                color
            } else {
                self.text_main
            },
        )
    }
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
            session_divider: c(0x3a4256),
            axis_line: c(0x2a3040),
            control_edge: c(0x6e7489),
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
            // sand, teal, silver, orchid
            study: [c(0xe2c46a), c(0x4cc4c9), c(0xc4c8d6), c(0xd98ac4)],
            // paper, lime, olive, slate
            drawing: [c(brand::PAPER), c(0xd4ff3a), c(0x78871e), c(0x788787)],
            drawing_names: ["White", "Lime", "Olive", "Slate"],
            label_bg: c(0x171b24).opacity(0.9),
            profile_va: c(0x9a9fb2).opacity(0.30),
            profile_poc: c(brand::PAPER).opacity(0.70),
            profile_edge: c(brand::PAPER).opacity(0.30),
            profile_buy: c(up).opacity(0.38),
            profile_sell: c(down).opacity(0.38),
            warn: c(0xf5b83d),
            on_warn: c(brand::INK),
            paper_badge: c(brand::PAPER),
            on_paper_badge: c(brand::INK),
            order_target: c(0x67d0e0),
            order_stop: c(0xe3a3f5),
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
            session_divider: c(0xb4b0c6),
            axis_line: c(0xcdcad9),
            control_edge: c(0x7d7a8c),
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
            study: [c(0x7a5c00), c(0x00666b), c(0x4a4f63), c(0x8f2f78)],
            // ink, warm gray, olive, slate: none in the red or green families
            drawing: [c(brand::INK), c(0x695a5a), c(0x707000), c(0x3a3a50)],
            drawing_names: ["Black", "Warm gray", "Olive", "Slate"],
            label_bg: c(brand::PAPER).opacity(0.9),
            profile_va: c(0x565b6e).opacity(0.24),
            profile_poc: c(brand::INK).opacity(0.65),
            profile_edge: c(brand::INK).opacity(0.30),
            profile_buy: c(up).opacity(0.32),
            profile_sell: c(down).opacity(0.32),
            warn: c(0x8a5300),
            on_warn: c(0xffffff),
            paper_badge: c(brand::INK),
            on_paper_badge: c(brand::PAPER),
            order_target: c(0x006b7a),
            order_stop: c(0x86399f),
        }
    }

    /// Every built-in theme/preset combination (the style editor checks user colors in all).
    pub fn all() -> [Theme; 4] {
        [
            Theme::dark(Palette::Standard),
            Theme::dark(Palette::Colorblind),
            Theme::light(Palette::Standard),
            Theme::light(Palette::Colorblind),
        ]
    }
}

/// A colour an external level source fixed for its levels (`0xRRGGBBAA`), alpha scaled by
/// `alpha`. Decision 0022: the one exception to "colours are tokens", for external levels whose
/// source contract fixes the colour. Their labels still use theme text tokens.
pub fn source_colour(rgba: u32, alpha: f32) -> Hsla {
    gpui::Rgba {
        r: ((rgba >> 24) & 0xff) as f32 / 255.0,
        g: ((rgba >> 16) & 0xff) as f32 / 255.0,
        b: ((rgba >> 8) & 0xff) as f32 / 255.0,
        a: (rgba & 0xff) as f32 / 255.0 * alpha.clamp(0.0, 1.0),
    }
    .into()
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
            // Study legends name each line in its colour.
            ("study0", t.study[0], "bg", t.bg),
            ("study1", t.study[1], "bg", t.bg),
            ("study2", t.study[2], "bg", t.bg),
            ("study3", t.study[3], "bg", t.bg),
            ("study0", t.study[0], "panel", t.panel),
            ("study1", t.study[1], "panel", t.panel),
            ("study2", t.study[2], "panel", t.panel),
            ("study3", t.study[3], "panel", t.panel),
            // Trading header: warnings, the PAPER badge, ARMED (accent fill), and working
            // orders' quantities on their side's colour.
            ("on_warn", t.on_warn, "warn", t.warn),
            (
                "on_paper_badge",
                t.on_paper_badge,
                "paper_badge",
                t.paper_badge,
            ),
            ("on_last", t.on_last, "accent", t.accent),
            ("bg", t.bg, "up", t.up),
            ("bg", t.bg, "down", t.down),
            ("bg", t.bg, "order_target", t.order_target),
            ("bg", t.bg, "order_stop", t.order_stop),
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

    /// Legend and tag text sits on `label_bg` over whatever the plot shows there: the
    /// background, candles (up/down), study lines and fills, profile rows. Every text colour
    /// used on it must meet AA over each, in every theme and palette.
    #[test]
    fn label_box_text_meets_aa_over_chart_content() {
        let mut failures = Vec::new();
        for t in Theme::all() {
            let mut under = vec![
                ("bg", t.bg),
                ("up", t.up),
                ("down", t.down),
                ("grid", t.grid),
                ("profile_va", over(t.profile_va, t.bg)),
                ("profile_poc", over(t.profile_poc, t.bg)),
                ("last", t.last),
            ];
            for (i, s) in t.study.iter().enumerate() {
                under.push((["study0", "study1", "study2", "study3"][i], *s));
            }
            let mut text = vec![("text_dim", t.text_dim), ("text_main", t.text_main)];
            for (i, s) in t.study.iter().enumerate() {
                text.push((["study0", "study1", "study2", "study3"][i], *s));
            }
            for (un, u) in &under {
                let box_ = over(t.label_bg, *u);
                for (tn, c) in &text {
                    let r = contrast(*c, box_);
                    if r < 4.5 {
                        failures.push(format!("{}: {tn} on label_bg over {un} = {r:.2}", t.name));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "below AA:
{}",
            failures.join(
                "
"
            )
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
            for s in t.study {
                assert!(s != t.up && s != t.down && s != t.accent, "{}", t.name);
            }
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
    fn trading_tokens_never_borrow_market_or_accent_colours() {
        for t in Theme::all() {
            for (name, tok) in [("up", t.up), ("down", t.down), ("accent", t.accent)] {
                assert_ne!(t.warn, tok, "{}: warn is {name}", t.name);
                assert_ne!(t.paper_badge, tok, "{}: paper badge is {name}", t.name);
            }
            // Bracket exits: their own colours, never the market's or the accent, and
            // distinct from each other.
            for (name, tok) in [
                ("order_target", t.order_target),
                ("order_stop", t.order_stop),
            ] {
                for (other, o) in [
                    ("up", t.up),
                    ("down", t.down),
                    ("accent", t.accent),
                    ("warn", t.warn),
                ] {
                    assert_ne!(tok, o, "{}: {name} is {other}", t.name);
                }
                assert!(contrast(tok, t.panel) >= 3.0, "{}: {name} on panel", t.name);
            }
            assert_ne!(t.order_target, t.order_stop, "{}", t.name);
            // The badge stands out from the header it sits on (non-text contrast, 3:1).
            assert!(contrast(t.paper_badge, t.panel) >= 3.0, "{}", t.name);
            assert!(contrast(t.warn, t.panel) >= 3.0, "{}", t.name);
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
            ("profile.rs", include_str!("profile.rs")),
            ("levels_layer.rs", include_str!("levels_layer.rs")),
            ("dom_view.rs", include_str!("dom_view.rs")),
            ("tape_view.rs", include_str!("tape_view.rs")),
            ("workspace.rs", include_str!("workspace.rs")),
            ("pane.rs", include_str!("pane.rs")),
            ("picker_view.rs", include_str!("picker_view.rs")),
            ("bench.rs", include_str!("bench.rs")),
            ("text.rs", include_str!("text.rs")),
            ("studies.rs", include_str!("studies.rs")),
            ("main.rs", include_str!("main.rs")),
            ("screenshot.rs", include_str!("screenshot.rs")),
            ("trade_bar.rs", include_str!("trade_bar.rs")),
            ("trading.rs", include_str!("trading.rs")),
            ("dom_trade.rs", include_str!("dom_trade.rs")),
            ("chart_draw.rs", include_str!("chart_draw.rs")),
            ("style_editor.rs", include_str!("style_editor.rs")),
            ("style_editor_view.rs", include_str!("style_editor_view.rs")),
        ] {
            for needle in ["rgb(", "rgba(", "hsla(", "Hsla {"] {
                assert!(
                    !src.contains(needle),
                    "{file} contains a colour literal ({needle})"
                );
            }
        }
    }

    /// Drawing colors read against the chart (3:1, graphical objects) and never borrow a
    /// meaning: clearly apart from up, down and the accent (sRGB distance, so near-misses such
    /// as a sky blue beside the colorblind palette's blue up fail too), in every theme and
    /// palette.
    /// Study lines (VWAP, averages, Bollinger band edges) carry data: each study color is at
    /// least 3:1 on the chart in every theme and palette.
    #[test]
    fn study_colors_are_3_to_1_on_the_chart() {
        for t in Theme::all() {
            for (i, c) in t.study.iter().enumerate() {
                let r = contrast(*c, t.bg);
                assert!(r >= 3.0, "{} study[{i}]: {r:.2}:1", t.name);
            }
        }
    }

    #[test]
    fn drawing_colors_contrast_and_stay_distinct() {
        for t in Theme::all() {
            // Both palettes' up/down: a drawing keeps its color when the palette changes.
            let other = if t.name.contains("colorblind") {
                Palette::Standard
            } else {
                Palette::Colorblind
            };
            let twin = if t.name.starts_with("dark") {
                Theme::dark(other)
            } else {
                Theme::light(other)
            };
            let reserved = [
                ("up", t.up),
                ("down", t.down),
                ("other palette's up", twin.up),
                ("other palette's down", twin.down),
                ("accent", t.accent),
                ("last", t.last),
                ("warn", t.warn),
                ("order_target", t.order_target),
                ("order_stop", t.order_stop),
            ];
            for (i, d) in t.drawing.iter().enumerate() {
                let r = contrast(*d, t.bg);
                assert!(r >= 3.0, "{}: drawing color {i}: {r:.2}", t.name);
                for (j, e) in t.drawing.iter().enumerate().skip(i + 1) {
                    let de = delta_e_2000(*d, *e);
                    assert!(
                        de >= 12.0,
                        "{}: drawing colors {i} and {j}: dE {de:.1}",
                        t.name
                    );
                }
                for (what, c) in reserved {
                    let de = delta_e_2000(*d, c);
                    assert!(
                        de >= 20.0,
                        "{}: drawing color {i} near {what}: dE {de:.1}",
                        t.name
                    );
                }
            }
        }
    }

    /// A dashed drawing never looks like the position line or a held order.
    #[test]
    fn drawing_dash_is_its_own() {
        for (what, d) in [("position", POSITION_DASH), ("held", HELD_DASH)] {
            let (a, b) = (DRAWING_DASH, d);
            assert!(
                a.on >= 1.5 * b.on && a.on + a.off >= b.on + b.off + 4.0,
                "drawing dash too close to {what}"
            );
        }
        assert_ne!(POSITION_DASH, HELD_DASH);
    }

    /// Control edges read (WCAG 1.4.11) on the panels they sit on, in every theme.
    #[test]
    fn control_edges_read_on_panels() {
        for t in Theme::all() {
            for bg in [t.panel, t.bg] {
                let r = contrast(t.control_edge, bg);
                assert!(r >= 3.0, "{}: control edge {r:.2}:1", t.name);
            }
        }
    }

    /// Every line type's pattern is its own: none looks like the position line or a held
    /// order at any width (decision 0026), and only solid is solid. "Looks like": two
    /// segments whose period is within a third and whose duty (the drawn share) is within
    /// 0.2 of a trading pattern's, so a thin dotted line (2/6) never reads as a held order
    /// (3/3) nor a dotted 2 px one (2/8) as the position line (6/4).
    #[test]
    fn drawing_line_types_never_look_like_trading_lines() {
        let looks_alike = |d: DashPattern, t: Dash| {
            let period = d.seg[0] + d.seg[1];
            let tp = t.on + t.off;
            d.n == 2
                && (period - tp).abs() <= tp / 3.0
                && (d.seg[0] / period - t.on / tp).abs() <= 0.2
        };
        for w in 1..=4 {
            let w = f32::from(w as u8);
            for lt in LineType::ALL {
                let d = drawing_dash(lt, w);
                assert_eq!(d.is_solid(), lt == LineType::Solid, "{lt:?}");
                if d.is_solid() {
                    continue;
                }
                for trading in [POSITION_DASH, HELD_DASH] {
                    assert!(
                        !looks_alike(d, trading),
                        "{lt:?} at {w} px ({:?}) looks like {trading:?}",
                        &d.seg[..d.n]
                    );
                }
            }
        }
        // The check itself catches the near misses it is for.
        let p = |on, off| DashPattern {
            seg: [on, off, 0.0, 0.0],
            n: 2,
        };
        assert!(looks_alike(p(2.0, 4.0), HELD_DASH));
        assert!(looks_alike(p(6.0, 5.0), POSITION_DASH));
    }

    /// A style's opacity scales a token's own alpha exactly once.
    #[test]
    fn style_color_applies_opacity_once() {
        let t = Theme::dark(Palette::Standard);
        let half = [t.text_main.opacity(0.5)];
        let c = t.style_color(Color::Palette(0), &half, 50);
        assert!((c.a - 0.25).abs() < 1e-6, "{}", c.a);
        let c = t.style_color(Color::Palette(5), &t.drawing, 100);
        assert_eq!(c, t.drawing[1]);
        assert!((t.style_color(Color::Text, &t.drawing, 25).a - 0.25).abs() < 1e-6);
    }

    /// Any color a user can pick, coarsely: every 0x33 step per channel (216 colors).
    fn any_user_color() -> impl Iterator<Item = Hsla> {
        (0..216u32).map(|i| {
            let ch = |k: u32| (k % 6) * 0x33;
            let rgb = ch(i / 36) << 16 | ch(i / 6) << 8 | ch(i);
            user_color(Rgb::hex(rgb), 100)
        })
    }

    /// A drawing's axis tag is outlined in the drawing's color and its text reads: AA on the
    /// axis background for every theme drawing color, starting swatch and any custom color
    /// (216 sampled, with half opacity too), in every theme and palette.
    #[test]
    fn drawing_tag_text_meets_aa_for_any_color() {
        for t in Theme::all() {
            let colors = t
                .drawing
                .iter()
                .copied()
                .chain(
                    CUSTOM_SWATCHES
                        .iter()
                        .map(|&(h, _)| user_color(Rgb::hex(h), 100)),
                )
                .chain(any_user_color())
                .chain(any_user_color().map(|c| c.opacity(0.5)));
            for c in colors {
                let (border, text) = t.drawing_tag(c);
                assert_eq!(border.a, 1.0, "the outline is opaque");
                let r = contrast(text, t.bg);
                assert!(r >= 4.5, "{}: tag text for {:?}: {r:.2}", t.name, rgb_of(c));
            }
        }
    }

    /// A note's text reads in any color: on the chart, or on its box (drawn when needed).
    #[test]
    fn note_text_meets_aa_for_any_color() {
        for t in Theme::all() {
            for c in any_user_color() {
                for boxed in [false, true] {
                    let (draw_box, text) = t.note_text(c, boxed);
                    assert!(draw_box || !boxed, "a box asked for is drawn");
                    let bg = if draw_box { t.panel } else { t.bg };
                    let r = contrast(text, bg);
                    assert!(r >= 4.5, "{}: note in {:?}: {r:.2}", t.name, rgb_of(c));
                }
            }
        }
    }

    /// The quick custom swatches carry no warning anywhere: 3:1 as a line on both charts and
    /// clear of every color with a meaning (no market up/down) in all four combinations; and
    /// they are clear of each other and of the theme's drawing colors (no near-duplicates).
    #[test]
    fn quick_swatches_are_clear_everywhere() {
        use lines_core::style::{NEAR_MEANING, readability};
        for t in Theme::all() {
            for (h, name) in CUSTOM_SWATCHES {
                let c = Rgb::hex(h);
                let r = readability(c, 100, rgb_of(t.bg), &t.meanings(), false);
                assert!(r.ok(), "{}: {name}: {r:?}", t.name);
                for d in t.drawing {
                    let de = lines_core::style::delta_e_2000(c, rgb_of(d));
                    assert!(de >= 10.0, "{}: {name} next to a drawing color", t.name);
                }
            }
        }
        let (a, b) = (
            Rgb::hex(CUSTOM_SWATCHES[0].0),
            Rgb::hex(CUSTOM_SWATCHES[1].0),
        );
        assert!(lines_core::style::delta_e_2000(a, b) >= NEAR_MEANING);
    }

    /// The default style reads: a note's text color on its box and on the chart, and every
    /// theme drawing color passes the editor's guard as a line (no warning by default).
    #[test]
    fn default_styles_pass_the_readability_guard() {
        use lines_core::style::readability;
        for t in Theme::all() {
            let m = t.meanings();
            let text = t.style_color(Color::Text, &t.drawing, 100);
            assert!(readability(rgb_of(text), 100, rgb_of(t.panel), &m, true).ok());
            assert!(readability(rgb_of(text), 100, rgb_of(t.bg), &m, true).ok());
            for i in 0..4 {
                let c = t.style_color(Color::Palette(i), &t.drawing, 100);
                let r = readability(rgb_of(c), 100, rgb_of(t.bg), &m, false);
                assert!(r.ok(), "{}: drawing color {i}: {r:?}", t.name);
            }
        }
    }

    /// CIEDE2000 color difference (sRGB, D65): the perceptual gap the drawing test uses
    /// (`lines_core::style::delta_e_2000`, which the style editor's guard uses too).
    fn delta_e_2000(a: Hsla, b: Hsla) -> f64 {
        lines_core::style::delta_e_2000(rgb_of(a), rgb_of(b))
    }

    #[test]
    fn delta_e_2000_matches_reference_pairs() {
        // Sharma, Wu & Dalal (2005) test data, pairs 1 and 17 (Lab inputs via sRGB are not
        // exact, so check the metric on identical and clearly different colors instead).
        assert!(delta_e_2000(c(0x808080), c(0x808080)) < 1e-9);
        let black_white = delta_e_2000(c(0x000000), c(0xffffff));
        assert!((black_white - 100.0).abs() < 0.5, "{black_white}");
    }

    /// The DOM price trail (`crate::dom_trail`) is fill only, in its own column, so it never
    /// sits under text: the price column beside it keeps its AA contrast (its numbers are on
    /// `bg`, or on the last-price tag). The chevron drawn on the trail is a graphical mark
    /// (WCAG 1.4.11: 3:1): it stays legible on the strongest fill of either side or neutral,
    /// in every theme and palette.
    #[test]
    fn dom_trail_keeps_contrast() {
        use lines_core::trail::MAX_ALPHA;
        for t in Theme::all() {
            assert!(contrast(t.text_main, t.bg) >= 4.5, "{}", t.name);
            assert!(contrast(t.on_last, t.last) >= 4.5, "{}", t.name);
            for (what, color) in [("up", t.up), ("down", t.down), ("neutral", t.text_dim)] {
                for alpha in [MAX_ALPHA * 0.25, MAX_ALPHA * 0.5, MAX_ALPHA] {
                    let cell = over(color.opacity(alpha), t.bg);
                    let r = contrast(t.text_main, cell);
                    assert!(
                        r >= 3.0,
                        "{}: chevron on {what} trail at {alpha}: {r:.2}",
                        t.name
                    );
                }
            }
        }
    }

    /// Trade panel v2: the position card (on `panel`) writes S1 / T1 in the stop and target
    /// colors, P&L in up/down, and the warn switch carries `on_warn`; the order buttons put
    /// `bg` text on up/down fills. All AA in every theme and palette.
    #[test]
    fn trade_panel_text_meets_aa() {
        for t in Theme::all() {
            for (what, fg, bg) in [
                ("S1 on the card", t.order_stop, t.panel),
                ("T1 on the card", t.order_target, t.panel),
                ("up P&L on the card", t.up, t.panel),
                ("down P&L on the card", t.down, t.panel),
                ("dim on the card", t.text_dim, t.panel),
                ("switch text", t.on_warn, t.warn),
                ("BUY MKT", t.bg, t.up),
                ("SELL MKT", t.bg, t.down),
                ("selected segment", t.on_last, t.accent),
            ] {
                let r = contrast(fg, bg);
                assert!(r >= 4.5, "{}: {what} {r:.2}", t.name);
            }
        }
    }
}
