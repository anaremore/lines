//! Layout recording for the layout lint (`lines_core::layout_lint`, UI checks layer 1).
//!
//! In screenshot builds, while a recording is on (`start` … `take`, around the frame
//! screenshot mode reads back), panes record the rectangle of every text run, chip, button,
//! hover slot, popup and reserved region they draw, with the clip in force. Everywhere else
//! (the shipped app, the benchmark, any frame not being recorded) this costs nothing: without
//! the `screenshot` feature the functions are empty and inlined away, and with it they check a
//! thread-local flag and return.

use gpui::{Bounds, Pixels, Window};
use lines_core::layout_lint::{Item, Kind, Rect};

/// Layer for popups (menus, lists) drawn over the panes.
pub const POPUP: u8 = 1;

#[cfg(feature = "screenshot")]
mod imp {
    use std::cell::RefCell;

    use lines_core::layout_lint::Item;

    thread_local! {
        static REC: RefCell<Option<Vec<Item>>> = const { RefCell::new(None) };
    }

    pub fn on() -> bool {
        REC.with(|r| r.borrow().is_some())
    }

    pub fn push(i: Item) {
        REC.with(|r| {
            if let Some(v) = r.borrow_mut().as_mut() {
                v.push(i);
            }
        });
    }

    pub fn start() {
        REC.with(|r| *r.borrow_mut() = Some(Vec::new()));
    }

    pub fn take() -> Vec<Item> {
        REC.with(|r| r.borrow_mut().take().unwrap_or_default())
    }
}

#[cfg(not(feature = "screenshot"))]
mod imp {
    use lines_core::layout_lint::Item;

    #[inline(always)]
    pub fn on() -> bool {
        false
    }

    #[inline(always)]
    pub fn push(_: Item) {}
}

#[cfg(feature = "screenshot")]
pub use imp::{start, take};

/// Whether a frame is being recorded (callers skip building labels otherwise).
#[inline(always)]
pub fn on() -> bool {
    imp::on()
}

pub fn rect(b: Bounds<Pixels>) -> Rect {
    Rect::new(
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

/// What to record about one rectangle.
pub struct Spec<'a> {
    pub kind: Kind,
    pub label: &'a str,
    pub layer: u8,
    pub group: &'a str,
    pub owner: &'a str,
}

impl<'a> Spec<'a> {
    pub fn new(kind: Kind, label: &'a str) -> Spec<'a> {
        Spec {
            kind,
            label,
            layer: 0,
            group: "",
            owner: "",
        }
    }

    pub fn text(label: &'a str) -> Spec<'a> {
        Spec::new(Kind::Text, label)
    }

    pub fn layer(mut self, layer: u8) -> Self {
        self.layer = layer;
        self
    }

    pub fn group(mut self, group: &'a str) -> Self {
        self.group = group;
        self
    }

    pub fn owner(mut self, owner: &'a str) -> Self {
        self.owner = owner;
        self
    }
}

/// Record `bounds` (window coordinates) with the clip in force now.
#[inline]
pub fn record(window: &Window, bounds: Bounds<Pixels>, spec: Spec<'_>) {
    if !on() {
        return;
    }
    let clip = rect(window.content_mask().bounds);
    imp::push(Item {
        kind: spec.kind,
        label: spec.label.to_owned(),
        rect: rect(bounds),
        clip: Some(clip),
        layer: spec.layer,
        group: spec.group.to_owned(),
        owner: spec.owner.to_owned(),
    });
}

/// Record a rectangle given as left/top/width/height (logical px, window coordinates).
#[inline]
pub fn record_xywh(window: &Window, (x, y, w, h): (f32, f32, f32, f32), spec: Spec<'_>) {
    if !on() {
        return;
    }
    record(
        window,
        Bounds::new(
            gpui::point(gpui::px(x), gpui::px(y)),
            gpui::size(gpui::px(w), gpui::px(h)),
        ),
        spec,
    );
}

/// Record a shaped line painted at `origin` (its run: shaped width × line height).
#[inline]
pub fn line(
    window: &Window,
    line: &gpui::ShapedLine,
    origin: gpui::Point<Pixels>,
    line_h: Pixels,
    owner: &str,
) {
    if !on() {
        return;
    }
    record(
        window,
        Bounds::new(origin, gpui::size(line.width, line_h)),
        Spec::text(line.text.as_ref()).owner(owner),
    );
}

#[cfg(feature = "screenshot")]
thread_local! {
    static OWNER: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

/// The reserved region text painted from now on belongs to (`""`: none). Glyph text (DOM,
/// T&S) records it, so a column's own numbers may sit in its reserved rectangle.
#[inline(always)]
pub fn set_owner(_owner: &'static str) {
    #[cfg(feature = "screenshot")]
    OWNER.with(|o| o.set(_owner));
}

#[inline(always)]
pub fn owner() -> &'static str {
    #[cfg(feature = "screenshot")]
    {
        OWNER.with(|o| o.get())
    }
    #[cfg(not(feature = "screenshot"))]
    {
        ""
    }
}

/// A reserved region (`name`), drawn by its owner only.
#[inline]
pub fn reserved(window: &Window, xywh: (f32, f32, f32, f32), name: &str) {
    if !on() {
        return;
    }
    record_xywh(
        window,
        xywh,
        Spec::new(
            Kind::Reserved {
                name: name.to_owned(),
            },
            name,
        )
        .owner(name),
    );
}

/// An element that records its own laid-out bounds: added as an absolutely positioned,
/// full-size child of the element to record (which must be `relative`). Built only while
/// recording; otherwise `None` (nothing added to the tree).
pub fn marker(
    kind: Kind,
    label: impl Into<String>,
    layer: u8,
    group: &'static str,
) -> Option<gpui::AnyElement> {
    bordered(kind, label, layer, group, 0.0)
}

/// A header's text run (its parent must be `relative`).
pub fn text_marker(label: &str) -> Option<gpui::AnyElement> {
    // Checked before the label is copied: nothing is allocated unless recording.
    if !on() {
        return None;
    }
    marker(Kind::Text, label.to_owned(), 0, "")
}

/// [`marker`] for an element with a `border` (logical px): an absolutely positioned child
/// spans its parent's padding box, so the marker reaches out over the border to record the
/// whole element.
pub fn bordered(
    kind: Kind,
    label: impl Into<String>,
    layer: u8,
    group: &'static str,
    border: f32,
) -> Option<gpui::AnyElement> {
    use gpui::{IntoElement, Styled};
    if !on() {
        return None;
    }
    let label = label.into();
    Some(
        gpui::canvas(
            move |bounds, window, _| {
                record(
                    window,
                    bounds,
                    Spec::new(kind, &label).layer(layer).group(group),
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top(gpui::px(-border))
        .left(gpui::px(-border))
        .right(gpui::px(-border))
        .bottom(gpui::px(-border))
        .into_any_element(),
    )
}
