//! Line icons for pane headers, tabs, the replay bar and the drawing toolbar: [Lucide](https://lucide.dev)
//! (vendored in `lines-icons`: ISC, MIT for the icons derived from Feather; stroke 1.6 on the
//! 24 grid, round caps and joins), drawn 16 px in a 20 px hit box. GPUI rasterizes each SVG
//! once per size into its sprite atlas (antialiased, at device resolution) and tints it with the
//! caller's theme token, so painting a cached icon allocates nothing of ours. The same SVGs are
//! the app's assets ([`Assets`], registered with the app) under `icons/<name>.svg`, for GPUI's
//! `svg()` element ([`lucide`]: the drawing toolbar's).

use std::borrow::Cow;

use gpui::{
    App, AssetSource, Bounds, Div, ElementId, Hsla, InteractiveElement, ParentElement, Pixels,
    SharedString, Stateful, Styled, TransformationMatrix, Window, canvas, div, point, px, size,
};

use crate::commands::Command;
use crate::theme::Theme;

/// An icon's drawn size (logical px): Lucide's 24 grid scaled to this.
pub const SIZE: f32 = 16.0;
/// A clickable icon's box (logical px): never under 20 × 20.
pub const HIT: f32 = 20.0;

/// The icons Lines draws (each a Lucide icon: [`Icon::lucide`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Icon {
    /// A square with an arrow leaving up-right: pop the pane out into its own window.
    PopOut,
    /// An arrow coming into an open bracket: re-attach a popped-out pane where it was.
    Reattach,
    /// A panel with its right side marked off: dock beside the chart.
    Dock,
    /// An eye with a stroke through it: hide the pane.
    Hide,
    /// Four corners pointing out: chart only (the chart takes the space).
    ChartOnly,
    /// Three dots: the pane menu.
    Menu,
    /// ×: close.
    Close,
    /// A pushpin: a pinned tab (in the tab's close slot).
    Pin,
    /// A triangle pointing right: play (the replay bar).
    Play,
    /// Two bars: pause.
    Pause,
    /// A bar and a triangle pointing left: the previous session open.
    SkipBack,
    /// A triangle pointing right and a bar: the next session open.
    SkipForward,
}

impl Icon {
    #[cfg(test)]
    pub const ALL: [Icon; 12] = [
        Icon::Play,
        Icon::Pause,
        Icon::SkipBack,
        Icon::SkipForward,
        Icon::PopOut,
        Icon::Reattach,
        Icon::Dock,
        Icon::Hide,
        Icon::ChartOnly,
        Icon::Menu,
        Icon::Close,
        Icon::Pin,
    ];
}

impl Icon {
    /// The Lucide icon it is drawn with (`lines_icons::ICONS`).
    pub fn lucide(self) -> &'static str {
        match self {
            Icon::PopOut => "square-arrow-out-up-right",
            Icon::Reattach => "log-in",
            Icon::Dock => "panel-right",
            Icon::Hide => "eye-off",
            Icon::ChartOnly => "maximize",
            Icon::Menu => "ellipsis",
            Icon::Close => "x",
            Icon::Pin => "pin",
            Icon::Play => "play",
            Icon::Pause => "pause",
            Icon::SkipBack => "skip-back",
            Icon::SkipForward => "skip-forward",
        }
    }

    /// Its asset path (`icons/<name>.svg`): the key GPUI caches its rasterized tile by.
    fn path(self) -> &'static str {
        match self {
            Icon::PopOut => "icons/square-arrow-out-up-right.svg",
            Icon::Reattach => "icons/log-in.svg",
            Icon::Dock => "icons/panel-right.svg",
            Icon::Hide => "icons/eye-off.svg",
            Icon::ChartOnly => "icons/maximize.svg",
            Icon::Menu => "icons/ellipsis.svg",
            Icon::Close => "icons/x.svg",
            Icon::Pin => "icons/pin.svg",
            Icon::Play => "icons/play.svg",
            Icon::Pause => "icons/pause.svg",
            Icon::SkipBack => "icons/skip-back.svg",
            Icon::SkipForward => "icons/skip-forward.svg",
        }
    }
}

/// The app's assets: the vendored icons, by `icons/<name>.svg` (for GPUI's `svg()` element; a
/// painted [`Icon`] passes its bytes directly). Registered with `Application::with_assets`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"))
            .and_then(lines_icons::svg)
            .map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(lines_icons::ICONS
            .iter()
            .map(|i| SharedString::from(format!("icons/{}.svg", i.name)))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}

/// A vendored icon as GPUI's `svg()` element: `path` is its asset path (`icons/<name>.svg`,
/// served by [`Assets`]), `size` logical px square, in `color`, turned `turns` quarter turns
/// clockwise (a vertical line from Lucide's horizontal `minus`). For an icon that is its own
/// element (the drawing toolbar's), where a painted [`Icon`] doesn't fit.
pub fn lucide(path: &'static str, size: Pixels, color: Hsla, turns: u8) -> gpui::Svg {
    let s = gpui::svg()
        .path(path)
        .flex_none()
        .size(size)
        .text_color(color);
    match turns % 4 {
        0 => s,
        q => s.with_transformation(gpui::Transformation::rotate(gpui::radians(
            f32::from(q) * std::f32::consts::FRAC_PI_2,
        ))),
    }
}

/// Paint `icon` centered in `bounds` (logical px), [`SIZE`] px square (smaller if `bounds` is),
/// in `color`. GPUI rasterizes it once per size and caches the tile: for the hovered pane's
/// header controls, the tab bar and the replay bar.
pub fn paint(icon: Icon, bounds: Bounds<Pixels>, color: Hsla, window: &mut Window, cx: &App) {
    let side = f32::from(bounds.size.width)
        .min(f32::from(bounds.size.height))
        .min(SIZE);
    let x = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - side) * 0.5;
    let y = f32::from(bounds.origin.y) + (f32::from(bounds.size.height) - side) * 0.5;
    let b = Bounds::new(point(px(x), px(y)), size(px(side), px(side)));
    let svg = lines_icons::svg(icon.lucide());
    // Every icon is embedded and parses (tested below); a failure would draw nothing, so it's
    // logged once rather than every frame.
    if let Err(e) = window.paint_svg(
        b,
        SharedString::new_static(icon.path()),
        svg,
        TransformationMatrix::unit(),
        color,
        cx,
    ) {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| tracing::warn!("icon {icon:?} not drawn: {e}"));
    }
}

/// An icon that is only shown (a pinned tab's pin): [`HIT`] px square, in `color`.
pub fn glyph(icon: Icon, color: Hsla) -> Div {
    div().flex_none().w(px(HIT)).h(px(HIT)).child(
        canvas(
            |_, _, _| (),
            move |b, (), window, cx| paint(icon, b, color, window, cx),
        )
        .size_full(),
    )
}

/// A clickable icon: [`HIT`] px square, `text_dim` at rest, `text_main` under the pointer,
/// `accent` while `on` (e.g. chart only active). Carries its tooltip ("Pop out" with its key as caps,
/// after `tooltip::DELAY`), the button role and `name` as its accessible label. The caller
/// adds the click.
pub fn button(
    id: impl Into<ElementId>,
    icon: Icon,
    name: &str,
    cmd: Option<Command>,
    on: bool,
    t: &Theme,
) -> Stateful<Div> {
    let (dim, main, accent, hover_bg) = (t.text_dim, t.text_main, t.accent, t.ghost_hover);
    let el = div()
        .id(id)
        .flex_none()
        .w(px(HIT))
        .h(px(HIT))
        .rounded_sm()
        .cursor_pointer()
        // The hover style also makes the window repaint when the pointer enters or leaves.
        .hover(move |s| s.bg(hover_bg))
        .child(
            canvas(
                |_, _, _| (),
                move |b, (), window, cx| {
                    // The pointer's last position stays after it leaves the window (no exit
                    // event on Windows): only a hovered window lights an icon.
                    let hot = window.is_window_hovered() && b.contains(&window.mouse_position());
                    let c = if on {
                        accent
                    } else if hot {
                        main
                    } else {
                        dim
                    };
                    paint(icon, b, c, window, cx);
                },
            )
            .size_full(),
        );
    crate::tooltip::with_tip(el, name, cmd, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon is drawn with a vendored Lucide SVG, at its own asset path, and no two icons
    /// share one; the assets serve exactly what painting uses.
    #[test]
    fn every_icon_is_a_vendored_lucide_icon() {
        for (i, icon) in Icon::ALL.iter().enumerate() {
            let svg = lines_icons::svg(icon.lucide()).unwrap_or_else(|| panic!("{icon:?}"));
            assert_eq!(icon.path(), format!("icons/{}.svg", icon.lucide()));
            let served = Assets.load(icon.path()).unwrap().unwrap();
            assert_eq!(served.as_ref(), svg);
            for other in &Icon::ALL[i + 1..] {
                assert_ne!(icon.lucide(), other.lucide(), "{icon:?} vs {other:?}");
            }
        }
        // Every vendored SVG parses as GPUI will rasterize it (a malformed file would fail in
        // paint, drawing nothing).
        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(Assets));
        for f in lines_icons::ICONS {
            assert!(
                renderer.parse_svg(f.svg).is_ok(),
                "{} doesn't parse",
                f.name
            );
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
        assert!(Assets.load("fonts/x.ttf").unwrap().is_none());
        assert_eq!(
            Assets.list("icons/").unwrap().len(),
            lines_icons::ICONS.len()
        );
        const { assert!(HIT >= 20.0 && SIZE <= HIT) };
    }
}
