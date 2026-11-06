//! Color themes. Colors are data so screenshots can pin a theme (decision 0009).

use gpui::{Hsla, rgb};

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Theme {
    pub bg: Hsla,
    pub panel: Hsla,
    pub grid: Hsla,
    pub axis_line: Hsla,
    pub text_dim: Hsla,
    pub text_main: Hsla,
    pub up: Hsla,
    pub down: Hsla,
    pub last: Hsla,
    /// Text drawn on top of `last`.
    pub on_last: Hsla,
    /// Crosshair lines.
    pub crosshair: Hsla,
    /// Crosshair price/time tags on the axes, and the text on them.
    pub cross_tag: Hsla,
    pub on_cross_tag: Hsla,
    /// Brand accent (signal violet, docs/design.md): focus, selection, active state such as
    /// manual price scale. Never used for up/down.
    pub accent: Hsla,
}

impl Theme {
    pub fn dark() -> Self {
        Theme {
            bg: rgb(0x0f1115).into(),
            panel: rgb(0x161a20).into(),
            grid: rgb(0x1f242c).into(),
            axis_line: rgb(0x2c323c).into(),
            text_dim: rgb(0x8a93a3).into(),
            text_main: rgb(0xd7dce4).into(),
            up: rgb(0x26a69a).into(),
            down: rgb(0xef5350).into(),
            last: rgb(0xf0b429).into(),
            on_last: rgb(0x0f1115).into(),
            crosshair: Hsla::from(rgb(0x9aa4b2)).opacity(0.55),
            cross_tag: rgb(0x3b4252).into(),
            on_cross_tag: rgb(0xeceff4).into(),
            accent: rgb(0x8c7bff).into(), // 5.3:1 on `panel`
        }
    }

    pub fn light() -> Self {
        Theme {
            bg: rgb(0xffffff).into(),
            panel: rgb(0xf3f4f6).into(),
            grid: rgb(0xeceef1).into(),
            axis_line: rgb(0xd1d5db).into(),
            text_dim: rgb(0x6b7280).into(),
            text_main: rgb(0x111827).into(),
            up: rgb(0x089981).into(),
            down: rgb(0xf23645).into(),
            last: rgb(0x2962ff).into(),
            on_last: rgb(0xffffff).into(),
            crosshair: Hsla::from(rgb(0x4b5563)).opacity(0.55),
            cross_tag: rgb(0x374151).into(),
            on_cross_tag: rgb(0xffffff).into(),
            // Brand violet is 3.0:1 on the light panel; this darker violet of the same hue
            // meets WCAG AA (5.3:1) for text.
            accent: rgb(0x5e4ae3).into(),
        }
    }
}
