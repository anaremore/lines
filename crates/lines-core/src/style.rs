//! User styles (decision 0026): one model for how a drawing or a study part is drawn, and the
//! color math behind the style editor's readability guard. Pure, allocation-free, no serde
//! (the app saves styles through its own documents).
//!
//! - [`LineStyle`]: color, line type (solid, dashed, dotted, dash-dot), width 1–4 px, opacity.
//! - [`FillStyle`]: color, opacity. [`TextStyle`]: size S/M/L, color, background on/off.
//! - A [`Color`] is a theme token by default ([`Color::Palette`]: the n-th color of the palette
//!   the context offers, the theme's drawing or study colors; [`Color::Text`]: the theme's main
//!   text color; [`Color::Own`]: a study part's own color), so it follows dark/light and the
//!   colorblind palette. [`Color::Custom`] is user data saved with the drawing or study, never
//!   a literal in code.
//! - Opacity is a whole percent ([`MIN_OPACITY`]..=100): exact, so styles compare and save
//!   exactly.
//!
//! The guard ([`readability`]) never blocks: it reports when a color is perceptually close
//! (CIEDE2000, [`delta_e_2000`]) to a color with a meaning, or below the WCAG contrast a line
//! (3:1) or text (4.5:1) needs against its background.

/// An opaque sRGB color, 8 bits per channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// From `0xRRGGBB`.
    pub const fn hex(v: u32) -> Rgb {
        Rgb {
            r: (v >> 16) as u8,
            g: (v >> 8) as u8,
            b: v as u8,
        }
    }

    pub const fn to_u32(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// `#rrggbb` or `rrggbb` (case-insensitive); anything else is `None`.
    pub fn parse(s: &str) -> Option<Rgb> {
        let h = s.strip_prefix('#').unwrap_or(s);
        if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(h, 16).ok().map(Rgb::hex)
    }

    /// `#rrggbb`, lower case, without allocating.
    pub fn write_hex<W: std::fmt::Write>(self, out: &mut W) -> std::fmt::Result {
        write!(out, "#{:06x}", self.to_u32())
    }

    /// From float channels in 0..=1 (rounded, clamped).
    pub fn from_f32(r: f32, g: f32, b: f32) -> Rgb {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Rgb {
            r: q(r),
            g: q(g),
            b: q(b),
        }
    }
}

/// A style's color.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    /// The n-th color of the context's theme palette (drawings: the theme's drawing colors;
    /// out-of-range indices wrap).
    Palette(u8),
    /// The theme's main text color.
    Text,
    /// A color the user chose (user data, saved with the drawing or study).
    Custom(Rgb),
    /// A study part's own color (decision 0026, studies only): the theme's color for that
    /// part, the color of the study's line it follows, or the colors a level source gives
    /// its levels (decision 0022). Never saved: a part without a saved color has it.
    Own,
}

/// How a line is stroked.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum LineType {
    #[default]
    Solid,
    Dashed,
    Dotted,
    DashDot,
}

impl LineType {
    pub const ALL: [LineType; 4] = [
        LineType::Solid,
        LineType::Dashed,
        LineType::Dotted,
        LineType::DashDot,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LineType::Solid => "Solid",
            LineType::Dashed => "Dashed",
            LineType::Dotted => "Dotted",
            LineType::DashDot => "Dash-dot",
        }
    }

    /// The saved name.
    pub fn key(self) -> &'static str {
        match self {
            LineType::Solid => "solid",
            LineType::Dashed => "dashed",
            LineType::Dotted => "dotted",
            LineType::DashDot => "dash-dot",
        }
    }

    pub fn from_key(s: &str) -> Option<LineType> {
        LineType::ALL.into_iter().find(|t| t.key() == s)
    }
}

/// Narrowest and widest line, logical px.
pub const MIN_WIDTH: u8 = 1;
pub const MAX_WIDTH: u8 = 4;
/// The faintest a style may be (percent): a drawing never becomes invisible.
pub const MIN_OPACITY: u8 = 10;
/// The opacities the editor offers for lines and text, percent.
pub const LINE_OPACITIES: [u8; 4] = [100, 75, 50, 25];
/// The opacities the editor offers for fills, percent (12 is a zone's default).
pub const FILL_OPACITIES: [u8; 4] = [12, 25, 40, 60];

fn opacity(v: u8) -> u8 {
    v.clamp(MIN_OPACITY, 100)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LineStyle {
    pub color: Color,
    pub line: LineType,
    /// Logical px, [`MIN_WIDTH`]..=[`MAX_WIDTH`].
    pub width: u8,
    /// Percent, [`MIN_OPACITY`]..=100.
    pub opacity: u8,
}

impl LineStyle {
    pub const DEFAULT: LineStyle = LineStyle {
        color: Color::Palette(0),
        line: LineType::Solid,
        width: 1,
        opacity: 100,
    };

    /// In range (a file may hold anything).
    pub fn normalised(self) -> LineStyle {
        LineStyle {
            width: self.width.clamp(MIN_WIDTH, MAX_WIDTH),
            opacity: opacity(self.opacity),
            ..self
        }
    }
}

impl Default for LineStyle {
    fn default() -> Self {
        LineStyle::DEFAULT
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FillStyle {
    pub color: Color,
    /// Percent, [`MIN_OPACITY`]..=100 (fills are translucent: 12 by default).
    pub opacity: u8,
}

impl FillStyle {
    pub const DEFAULT: FillStyle = FillStyle {
        color: Color::Palette(0),
        opacity: 12,
    };

    pub fn normalised(self) -> FillStyle {
        FillStyle {
            opacity: opacity(self.opacity),
            ..self
        }
    }
}

impl Default for FillStyle {
    fn default() -> Self {
        FillStyle::DEFAULT
    }
}

/// Text sizes, on the type scale (docs/design.md §5).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl TextSize {
    pub const ALL: [TextSize; 3] = [TextSize::Small, TextSize::Medium, TextSize::Large];

    /// Logical px.
    pub fn px(self) -> f32 {
        match self {
            TextSize::Small => 11.0,
            TextSize::Medium => 12.0,
            TextSize::Large => 15.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TextSize::Small => "S",
            TextSize::Medium => "M",
            TextSize::Large => "L",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            TextSize::Small => "s",
            TextSize::Medium => "m",
            TextSize::Large => "l",
        }
    }

    pub fn from_key(s: &str) -> Option<TextSize> {
        TextSize::ALL.into_iter().find(|t| t.key() == s)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextStyle {
    pub size: TextSize,
    pub color: Color,
    /// An opaque panel box behind the text.
    pub background: bool,
}

impl TextStyle {
    pub const DEFAULT: TextStyle = TextStyle {
        size: TextSize::Medium,
        color: Color::Text,
        background: true,
    };
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle::DEFAULT
    }
}

/// A dash pattern, logical px: on, off, on, off (`n` entries used, 0 = solid). Fixed size, so
/// a line's pattern never allocates.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DashPattern {
    pub seg: [f32; 4],
    pub n: usize,
}

impl DashPattern {
    pub const SOLID: DashPattern = DashPattern {
        seg: [0.0; 4],
        n: 0,
    };

    pub fn is_solid(&self) -> bool {
        self.n == 0
    }

    /// Calls `f(start, end)` for each drawn piece along a line `len` long (or once, solid).
    /// Generic, so nothing is boxed per line per frame.
    pub fn segments(&self, len: f32, mut f: impl FnMut(f32, f32)) {
        let period: f32 = self.seg[..self.n].iter().sum();
        if self.n == 0 || period <= 0.0 {
            f(0.0, len);
            return;
        }
        let mut s = 0.0;
        let mut i = 0;
        while s < len {
            let d = self.seg[i];
            if i % 2 == 0 && d > 0.0 {
                f(s, (s + d).min(len));
            }
            s += d;
            i = (i + 1) % self.n;
        }
    }
}

// ---- color math -----------------------------------------------------------------------

fn linear(v: u8) -> f64 {
    let v = f64::from(v) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2.x relative luminance.
pub fn luminance(c: Rgb) -> f64 {
    0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
}

/// WCAG contrast ratio (1..=21).
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `top` at `opacity` percent over opaque `bottom`, as drawn.
pub fn over(top: Rgb, opacity: u8, bottom: Rgb) -> Rgb {
    let a = f64::from(opacity.min(100)) / 100.0;
    let mix = |x: u8, y: u8| (f64::from(x) * a + f64::from(y) * (1.0 - a)).round() as u8;
    Rgb {
        r: mix(top.r, bottom.r),
        g: mix(top.g, bottom.g),
        b: mix(top.b, bottom.b),
    }
}

/// CIE L*a*b* (D65) of an sRGB color.
fn lab(c: Rgb) -> (f64, f64, f64) {
    let (r, g, b) = (linear(c.r), linear(c.g), linear(c.b));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f64| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// CIEDE2000 color difference (sRGB, D65): the perceptual gap the theme's drawing palette is
/// tested with, and the readability guard's "close to a meaning" test.
pub fn delta_e_2000(a: Rgb, b: Rgb) -> f64 {
    let ((l1, a1, b1), (l2, a2, b2)) = (lab(a), lab(b));
    let (c1, c2) = (a1.hypot(b1), a2.hypot(b2));
    let cb7 = ((c1 + c2) / 2.0).powi(7);
    let g = 0.5 * (1.0 - (cb7 / (cb7 + 25f64.powi(7))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = (a1p.hypot(b1), a2p.hypot(b2));
    let hue = |b: f64, a: f64| b.atan2(a).to_degrees().rem_euclid(360.0);
    let (h1, h2) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dh = if c1p * c2p == 0.0 {
        0.0
    } else if (h2 - h1).abs() <= 180.0 {
        h2 - h1
    } else if h2 > h1 {
        h2 - h1 - 360.0
    } else {
        h2 - h1 + 360.0
    };
    let dhh = 2.0 * (c1p * c2p).sqrt() * (dh.to_radians() / 2.0).sin();
    let lb = (l1 + l2) / 2.0;
    let cbp = (c1p + c2p) / 2.0;
    let hb = if c1p * c2p == 0.0 {
        h1 + h2
    } else if (h1 - h2).abs() <= 180.0 {
        (h1 + h2) / 2.0
    } else if h1 + h2 < 360.0 {
        (h1 + h2 + 360.0) / 2.0
    } else {
        (h1 + h2 - 360.0) / 2.0
    };
    let cos = |d: f64| d.to_radians().cos();
    let t = 1.0 - 0.17 * cos(hb - 30.0) + 0.24 * cos(2.0 * hb) + 0.32 * cos(3.0 * hb + 6.0)
        - 0.20 * cos(4.0 * hb - 63.0);
    let dth = 30.0 * (-((hb - 275.0) / 25.0).powi(2)).exp();
    let cbp7 = cbp.powi(7);
    let rc = 2.0 * (cbp7 / (cbp7 + 25f64.powi(7))).sqrt();
    let sl = 1.0 + 0.015 * (lb - 50.0).powi(2) / (20.0 + (lb - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cbp;
    let sh = 1.0 + 0.015 * cbp * t;
    let rt = -(2.0 * dth).to_radians().sin() * rc;
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dhh / sh).powi(2) + rt * (dc / sc) * (dhh / sh))
        .sqrt()
}

// ---- readability guard ------------------------------------------------------------------

/// Below this CIEDE2000 distance a color reads as a color with a meaning (the theme's drawing
/// palette keeps at least this far from every one).
pub const NEAR_MEANING: f64 = 20.0;
/// WCAG non-text contrast (1.4.11): lines and fills' edges.
pub const LINE_CONTRAST: f64 = 3.0;
/// WCAG AA text contrast.
pub const TEXT_CONTRAST: f64 = 4.5;

/// What the guard found about one color. Never a block: the editor shows it and applies the
/// color anyway.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Readability {
    /// The nearest color with a meaning within [`NEAR_MEANING`]: its name.
    pub near: Option<&'static str>,
    /// The contrast against the background, when below what's needed.
    pub low_contrast: Option<f64>,
    /// What was needed (3:1 lines, 4.5:1 text).
    pub needed: f64,
}

impl Readability {
    pub fn ok(&self) -> bool {
        self.near.is_none() && self.low_contrast.is_none()
    }
}

/// Check `color` drawn at `opacity` percent over `background`, against `meanings` (name,
/// color) of the current theme and palette. `text`: 4.5:1 is needed, else 3:1.
pub fn readability(
    color: Rgb,
    opacity: u8,
    background: Rgb,
    meanings: &[(&'static str, Rgb)],
    text: bool,
) -> Readability {
    let seen = over(color, opacity, background);
    let near = meanings
        .iter()
        .map(|&(name, m)| (name, delta_e_2000(seen, m)))
        .filter(|&(_, d)| d < NEAR_MEANING)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(name, _)| name);
    let needed = if text { TEXT_CONTRAST } else { LINE_CONTRAST };
    let r = contrast(seen, background);
    Readability {
        near,
        low_contrast: (r < needed).then_some(r),
        needed,
    }
}

#[cfg(test)]
mod tests;
