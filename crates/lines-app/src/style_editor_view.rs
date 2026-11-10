//! Rendering of the style editor (`crate::style_editor` is the model): a popover over the
//! chart (`crate::popover::floating`: a press outside closes it and does nothing else), render
//! time only. Every control is clickable and maps to the same model calls as the keys.
//!
//! State never relies on color alone (WCAG 1.4.1, 1.4.11): a selected choice has a check mark,
//! a 1 px accent border and a stronger fill; a selected swatch a 2 px ring and a dot; on/off
//! options are checkboxes; a color row names its focused (else selected) swatch beside its
//! label. Keyboard focus is a separate 2 px accent ring around the control.
//!
//! Placement ([`place`]): beside the edited drawing (right, left, below, above, in that order),
//! never over it when there is room; a study's at the plot's left under the study legend
//! ([`place_study`]), away from the newest bars; always inside the chart's plot area (above the time axis)
//! and, when the pane is wide enough to spare it, clear of the price axis (the last-price tag
//! stays visible); its body scrolling with a visible (display-only) scrollbar when the area is
//! short, the tabs pinned above it and "Save as default" pinned in the footer. Every key that
//! moves the focus, to another row or within one, scrolls the focused control into view
//! ([`EditorScroll`], [`body_child`], [`follow_span`]). The box is never taller than the height it was
//! placed with ([`content_height`], an upper bound of what it draws at that width: the lines
//! that could wrap are counted, the rest don't wrap), so it can't spill.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, ScrollHandle, StatefulInteractiveElement, Styled, div,
    prelude::*, px,
};

use crate::chart_view::{ChartView, axis_w};
use crate::style_editor::{
    Control, Row, StyleEditor, Warning, color_name, control_label, is_toggle,
};
use crate::theme::{Theme, user_color};
use lines_core::style::Color;

/// Widest the editor gets (logical px).
pub const W: f32 = 440.0;
/// Kept clear of the pane's edges and of the drawing (logical px).
pub const MARGIN: f32 = 8.0;
const GAP: f32 = 12.0;
/// The label column.
const LABEL_W: f32 = 64.0;
/// Every control is at least this tall and wide (hit target).
const HIT: f32 = 20.0;
/// The narrowest the editor gets for the price axis's sake: a pane that can't spare the axis
/// and still give the editor this much lets it cover the axis.
const MIN_BESIDE_AXIS: f32 = 280.0;
/// The color picker's square and sliders (logical px).
const SQUARE_H: f32 = 96.0;
const SLIDER_H: f32 = 12.0;

/// The custom color picker's height under its row: the square, the hue slider, the opacity
/// slider (a part with an opacity), and the swatch, hex field and dropper row.
fn picker_height(opacity: bool) -> f32 {
    4.0 + SQUARE_H + 8.0 + SLIDER_H + 8.0 + if opacity { SLIDER_H + 8.0 } else { 0.0 } + 24.0 + 4.0
}

/// Where the editor goes, chart-local: its box is at most `h` tall.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Place {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The right edge the editor keeps to in an `area` this wide: left of the price axis (so the
/// last-price tag stays visible) when that leaves it [`MIN_BESIDE_AXIS`], else the pane's.
fn right_edge(area_w: f32) -> f32 {
    if area_w - axis_w() - 2.0 * MARGIN >= MIN_BESIDE_AXIS {
        area_w - axis_w() - MARGIN
    } else {
        area_w - MARGIN
    }
}

/// The editor's width in an `area` this wide.
pub fn width(area_w: f32) -> f32 {
    W.min((right_edge(area_w) - MARGIN).max(0.0))
}

/// The editor's spot in an `area` (w, h: the chart's width and the height above its time
/// axis) for content `h` tall, beside `target` (the drawing's extent, x0, y0, x1, y1) when
/// there is room: right of it, left, below, above; else the top right (clear of the price
/// axis). Never past any edge of the area: narrower, shorter and scrolling when it must be.
pub fn place(area: (f32, f32), target: Option<(f32, f32, f32, f32)>, h: f32) -> Place {
    let (pw, ph) = area;
    let right = right_edge(pw);
    let w = width(pw);
    let h = h.min((ph - 2.0 * MARGIN).max(0.0));
    let clamp_x = |x: f32| x.min(right - w).max(MARGIN);
    let clamp_y = |y: f32| y.min(ph - h - MARGIN).max(MARGIN);
    let at = |x: f32, y: f32| Place { x, y, w, h };
    if let Some((x0, y0, x1, y1)) = target {
        if x1 + GAP + w <= right {
            return at(x1 + GAP, clamp_y(y0));
        }
        if x0 - GAP - w >= MARGIN {
            return at(x0 - GAP - w, clamp_y(y0));
        }
        if y1 + GAP + h <= ph - MARGIN {
            return at(clamp_x(x0), y1 + GAP);
        }
        if y0 - GAP - h >= MARGIN {
            return at(clamp_x(x0), y0 - GAP - h);
        }
    }
    at(clamp_x(right - w), clamp_y(MARGIN + 16.0))
}

/// A study's editor in an `area` (as [`place`]) for content `h` tall, touching the bottom of
/// the study legend it opens from (`below`), away from the newest bars and the price axis on
/// the right; moved up (over the legend) only when it wouldn't fit below.
///
/// At the plot's left, unless the study edited is drawn there (`left_clear`: the right edge
/// of the left-anchored volume profile's widest row, 0 for others): then right of it, or at
/// the plot's center, when either keeps the editor in the plot's left three quarters (clear
/// of the newest bars); else at the left (a narrow pane). All in the chart's coordinates,
/// which are the plot's: the plot starts at the chart's top-left (the price axis is on its
/// right, the time axis below), and the legend's bottom is measured from there.
pub fn place_study(area: (f32, f32), below: f32, left_clear: f32, h: f32) -> Place {
    let (pw, ph) = area;
    let w = width(pw);
    let h = h.min((ph - 2.0 * MARGIN).max(0.0));
    let y = below.min(ph - h - MARGIN).max(MARGIN);
    let newest = (pw - axis_w()).max(0.0) * 0.75;
    let x = if left_clear <= MARGIN {
        MARGIN
    } else if left_clear + GAP + w <= newest {
        left_clear + GAP
    } else if ((pw - axis_w()) - w) * 0.5 >= left_clear && ((pw - axis_w()) + w) * 0.5 <= newest {
        ((pw - axis_w()) - w) * 0.5
    } else {
        MARGIN
    };
    Place { x, y, w, h }
}

/// GPUI's default line height for text `size` px (1.618 em).
fn line_h(size: f32) -> f32 {
    (size * 1.618).ceil()
}

/// Lines a text of `chars` characters at `size` px wraps to in `w` px (an upper bound: wide
/// average glyphs).
fn text_lines(chars: usize, size: f32, w: f32) -> f32 {
    let per_line = (w / (size * 0.62)).floor().max(1.0);
    (chars as f32 / per_line).ceil().max(1.0)
}

/// Lines a flex-wrapped row of items `widths` (px, gap 4) takes in `w` px.
fn wrapped_lines(widths: impl Iterator<Item = f32>, w: f32) -> f32 {
    let mut lines = 0.0;
    let mut used = f32::INFINITY;
    for iw in widths {
        if used + 4.0 + iw > w {
            lines += 1.0;
            used = iw;
        } else {
            used += 4.0 + iw;
        }
    }
    f32::max(lines, 1.0)
}

/// A control's drawn width (its focus ring included).
fn control_w(c: Control, on: bool, names: &[&'static str]) -> f32 {
    let label = |s: String| s.chars().count() as f32 * 12.0 * 0.62;
    4.0 + match c {
        Control::Color(_) | Control::FillColor(_) => HIT,
        c if is_toggle(c) => 8.0 + 14.0 + 8.0 + label(control_label(c, names)),
        c => (16.0 + label(control_label(c, names)) + if on { 16.0 } else { 0.0 }).max(HIT),
    }
}

/// The tabs by group, in the parts' order (one group, `None`, unless the parts are grouped: a
/// level source's types).
pub fn tab_groups(e: &StyleEditor) -> Vec<(Option<&'static str>, Vec<usize>)> {
    let mut out: Vec<(Option<&'static str>, Vec<usize>)> = Vec::new();
    for (i, p) in e.parts.iter().enumerate() {
        match out.iter_mut().find(|(g, _)| *g == p.group) {
            Some((_, v)) => v.push(i),
            None => out.push((p.group, vec![i])),
        }
    }
    out
}

/// Whether the tabs are listed under group headings (each group a row that wraps).
fn grouped(e: &StyleEditor) -> bool {
    e.parts.iter().any(|p| p.group.is_some())
}

/// A tab's drawn width (its padding; semibold when selected, so the wide estimate).
fn tab_w(title: &str) -> f32 {
    16.0 + title.chars().count() as f32 * 12.0 * 0.62
}

/// The pinned tab strip (none for one part) at editor width `w`: one row, or a level source's
/// grouped tabs, each group's row wrapping beside its heading.
fn tabs_height(e: &StyleEditor, w: f32) -> f32 {
    if e.parts.len() <= 1 {
        return 0.0;
    }
    if !grouped(e) {
        return 4.0 + 24.0 + 4.0 + 1.0;
    }
    let tabs_w = (w - 2.0 - 24.0 - LABEL_W).max(1.0);
    let groups = tab_groups(e);
    let mut h = 4.0 + 4.0 + 1.0 + 2.0 * (groups.len() as f32 - 1.0);
    for (_, tabs) in &groups {
        let lines = wrapped_lines(tabs.iter().map(|&i| tab_w(e.parts[i].title)), tabs_w);
        h += lines * 24.0 + (lines - 1.0) * 4.0;
    }
    h
}

/// What doesn't scroll: the title bar, tabs and footer ("Save as default") and the borders, at
/// editor width `w`.
fn chrome_height(e: &StyleEditor, w: f32) -> f32 {
    // The title is the panel's title bar (`popover::floating`), above the box.
    let title = crate::popover::title_h();
    let save = if e.default_label.is_some() {
        4.0 + 28.0
    } else {
        0.0
    };
    // The footer holds only "Save as default" (no key hints: the title bar carries the panel).
    let pad = if save > 0.0 { 4.0 } else { 0.0 };
    2.0 + title + tabs_height(e, w) + 1.0 + save + pad
}

/// Whether `row` is drawn in the footer ("Save as default"), outside the scroll body.
fn in_footer(row: &Row) -> bool {
    matches!(row.controls.as_slice(), [(Control::SaveDefault, _)])
}

/// The scroll body's child holding model row `r` (rows are direct children so focus can
/// scroll them into view; "Save as default" is in the footer: `None`).
pub fn body_child(rows: &[Row], r: usize) -> Option<usize> {
    if in_footer(rows.get(r)?) {
        return None;
    }
    Some(rows[..r].iter().filter(|row| !in_footer(row)).count())
}

/// How many children [`render`] gives the scroll body: one per row outside the footer.
pub fn body_len(rows: &[Row]) -> usize {
    rows.iter().filter(|row| !in_footer(row)).count()
}

/// The focus a follow is for: (tab, row, col).
pub type FocusKey = (usize, usize, usize);

/// Frames a follow waits for the layout it needs (the body's first, or the focused row's
/// controls) before it settles for what it has.
const FOLLOW_TRIES: u8 = 3;

/// Which focus the body last scrolled to show (pure, so it is tested): a key that moves the
/// focus, to another row or within one, follows again; a wheel scroll alone doesn't snap back.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Follow {
    done: Option<FocusKey>,
    /// The focus the tries count for: a new focus starts them again.
    trying: Option<FocusKey>,
    tries: u8,
}

impl Follow {
    /// Whether the focus `key` still has to be scrolled into view.
    pub fn pending(&self, key: FocusKey) -> bool {
        self.done != Some(key)
    }

    /// A frame followed `key`: `exact` when it had the layout it needs. Not exact, it follows
    /// again next frame, [`FOLLOW_TRIES`] times at most, then settles. Returns whether another
    /// frame is needed.
    pub fn step(&mut self, key: FocusKey, exact: bool) -> bool {
        if self.trying != Some(key) {
            self.trying = Some(key);
            self.tries = 0;
        }
        if self.done != Some(key) && !exact && self.tries < FOLLOW_TRIES {
            self.tries += 1;
            return true;
        }
        self.done = Some(key);
        self.tries = 0;
        false
    }
}

/// The span (content top, height) to keep in view for the focused row at `row` in a body
/// `view` px tall: the whole row when it fits; else, with the focused control's measured span,
/// from the row's top (a color row's name line) through the control when that fits, else the
/// control alone; unmeasured, the row (shown from its top).
pub fn follow_span(view: f32, row: (f32, f32), control: Option<(f32, f32)>) -> (f32, f32) {
    if row.1 <= view {
        return row;
    }
    match control {
        Some((top, h)) if top + h - row.0 <= view => (row.0, top + h - row.0),
        Some(c) => c,
        None => row,
    }
}

/// The focused row's controls as last laid out: (tab, row) and each control's (content top,
/// height), measured in prepaint so a key that moves within the row scrolls to its control.
type Measured = Rc<RefCell<Option<((usize, usize), Vec<(f32, f32)>)>>>;

/// The editor body's scroll state: GPUI's handle (its offset, the rows' layout), what the
/// focus last followed and where the focused row's controls are.
#[derive(Clone, Default)]
pub struct EditorScroll {
    pub handle: ScrollHandle,
    follow: Follow,
    controls: Measured,
    /// A level source's grouped tab strip (it scrolls on its own when capped), and the tab it
    /// last brought into view: a tab the keys move to is scrolled into view.
    tabs: ScrollHandle,
    tab_shown: std::rc::Rc<std::cell::Cell<Option<usize>>>,
    /// The color picker's square, hue and opacity sliders as last painted (window px): where
    /// a press or drag lands in them.
    picker: Rc<RefCell<[gpui::Bounds<gpui::Pixels>; 3]>>,
}

impl EditorScroll {
    /// Scroll the editor's focus into view if a key moved it (row or column) since the last
    /// follow. The offset comes from the last frame's layout (a focus move doesn't move rows),
    /// so the scrollbar drawn at render shows it; a row that fits is also asked of GPUI
    /// (`scroll_to_item`, in prepaint, from this frame's layout), so a hex entry that closes on
    /// the same click (or warnings that change with it) can't leave the row hidden. Returns whether
    /// it scrolled (or is still waiting for the layout it needs: the body's, the focused
    /// row's controls), so the pane renders once more and its scrollbar shows the offset.
    pub fn follow(&mut self, e: &StyleEditor) -> bool {
        let key = (e.tab, e.row, e.col);
        if !self.follow.pending(key) {
            return false;
        }
        let rows = e.rows();
        let Some(ix) = body_child(&rows, e.row) else {
            self.follow.step(key, true); // the footer: always shown
            return false;
        };
        let scroll = &self.handle;
        let view = scroll.bounds();
        let exact = match scroll.bounds_for_item(ix) {
            Some(b) if view.size.height > px(0.0) => {
                let view_h = f32::from(view.size.height);
                let row = (f32::from(b.top() - view.top()), f32::from(b.size.height));
                let control = match &*self.controls.borrow() {
                    Some((at, spans)) if *at == (e.tab, e.row) => spans.get(e.col).copied(),
                    _ => None,
                };
                let (top, h) = follow_span(view_h, row, control);
                let to = keep_visible(view_h, top, h, -f32::from(scroll.offset().y))
                    .clamp(0.0, f32::from(scroll.max_offset().y));
                scroll.set_offset(gpui::point(scroll.offset().x, px(-to)));
                let fits = row.1 <= view_h;
                if fits {
                    // GPUI's own follow shows a row taller than the body from its top, which
                    // would undo the control's; it is asked only for rows that fit.
                    scroll.scroll_to_item(ix);
                }
                fits || control.is_some()
            }
            _ => {
                scroll.scroll_to_item(ix);
                false
            }
        };
        self.follow.step(key, exact);
        true
    }
}

/// How tall the editor draws at width `w` (logical px), as an upper bound: rows that wrap,
/// warnings that wrap, the hex entry, header and footer. [`place`] places this height and the
/// box never grows past it; when the area is shorter the body scrolls.
pub fn content_height(
    e: &StyleEditor,
    warnings: &[Warning],
    w: f32,
    names: &[&'static str],
) -> f32 {
    let inner = (w - 2.0 - 24.0).max(1.0);
    let controls_w = (inner - LABEL_W).max(1.0);
    let mut h = chrome_height(e, w) + 8.0; // the body's padding
    for r in e.rows() {
        if let [(Control::SaveDefault, _)] = r.controls.as_slice() {
            continue; // in the footer
        }
        let widths = r.controls.iter().map(|&(c, on)| control_w(c, on, names));
        if is_color_row(&r) {
            let main = r.label == "Color";
            let lines = wrapped_lines(widths, controls_w);
            h += 4.0 + line_h(12.0) + lines * 24.0 + (lines - 1.0) * 4.0 + 2.0;
            if let Some(note) = e.parts.get(e.tab).and_then(|p| p.note).filter(|_| main) {
                h += 4.0 + text_lines(note.chars().count(), 11.0, inner) * line_h(11.0);
            }
            if let Some(p) = e.custom.as_ref().filter(|p| p.fill != main) {
                h += picker_height(p.opacity.is_some());
            }
            if main && !warnings.is_empty() {
                // Inside the box: margins, border, padding, the bar and the marker.
                let text_w = (inner - 2.0 - 16.0 - 3.0 - 8.0 - 16.0 - 8.0).max(1.0);
                h += 8.0 + 2.0 + 8.0 + 4.0 * (warnings.len() as f32 - 1.0);
                for wn in warnings {
                    h += text_lines(wn.text.chars().count(), 12.0, text_w) * line_h(12.0);
                }
            }
        } else {
            let lines = wrapped_lines(widths, controls_w);
            h += 4.0 + lines * 24.0 + (lines - 1.0) * 4.0;
        }
    }
    h.ceil()
}

/// A row of color swatches with a name line: the tab's color ("Color") or a level type's zone
/// fill ("Fill").
fn is_color_row(r: &Row) -> bool {
    r.label == "Color" || r.label == "Fill"
}

/// The custom color picker under its color row (`crate::color_picker`): the square
/// (saturation across, brightness down), the hue and opacity sliders, then the color as it
/// will be, its hex and the eyedropper. The keyboard's part has the accent ring.
fn picker(
    e: &StyleEditor,
    p: &crate::color_picker::Custom,
    t: &Theme,
    es: &EditorScroll,
    cx: &mut Context<ChartView>,
) -> gpui::Div {
    use crate::chart_view::PickerPart;
    use crate::color_picker::{Zone, from_hsv};
    use gpui::{MouseMoveEvent, canvas, linear_color_stop, linear_gradient, relative};
    use lines_core::style::Rgb;
    // The gradients are the colors being picked (user data, decision 0026), not the UI's.
    let white = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    let black = Rgb { r: 0, g: 0, b: 0 };
    let rgb = p.color();
    let edge = |on: bool| if on { t.accent } else { t.axis_line };
    // Records where a part is painted, for the pointer.
    let probe = |k: usize| {
        let rects = es.picker.clone();
        canvas(move |b, _, _| rects.borrow_mut()[k] = b, |_, _, _, _| {})
            .absolute()
            .size_full()
    };
    // A press or a drag (button down) on a part: its fractions.
    let frac = |b: gpui::Bounds<gpui::Pixels>, at: gpui::Point<gpui::Pixels>| {
        let w = f32::from(b.size.width).max(1.0);
        let h = f32::from(b.size.height).max(1.0);
        (
            (f32::from(at.x - b.origin.x) / w).clamp(0.0, 1.0),
            (f32::from(at.y - b.origin.y) / h).clamp(0.0, 1.0),
        )
    };
    let pointer =
        |el: gpui::Stateful<gpui::Div>, part: PickerPart, k: usize, cx: &mut Context<ChartView>| {
            let (down, moved) = (es.picker.clone(), es.picker.clone());
            el.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view: &mut ChartView, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let (x, y) = frac(down.borrow()[k], ev.position);
                    view.picker_at(part, x, y, cx);
                }),
            )
            .on_mouse_move(cx.listener(
                move |view: &mut ChartView, ev: &MouseMoveEvent, _, cx| {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        let (x, y) = frac(moved.borrow()[k], ev.position);
                        view.picker_at(part, x, y, cx);
                    }
                },
            ))
        };
    let on_square = matches!(p.zone, Zone::Saturation | Zone::Brightness);
    let square = div()
        .id("picker-square")
        .relative()
        .w_full()
        .h(crate::zoom::ui(SQUARE_H))
        .rounded_sm()
        .border_2()
        .border_color(edge(on_square))
        .bg(user_color(from_hsv(p.h, 100, 100), 100))
        .child(div().absolute().size_full().bg(linear_gradient(
            90.0,
            linear_color_stop(user_color(white, 100), 0.0),
            linear_color_stop(user_color(white, 0), 1.0),
        )))
        .child(div().absolute().size_full().bg(linear_gradient(
            180.0,
            linear_color_stop(user_color(black, 0), 0.0),
            linear_color_stop(user_color(black, 100), 1.0),
        )))
        .child(
            div()
                .absolute()
                .left(relative(f32::from(p.s) / 100.0))
                .top(relative(1.0 - f32::from(p.v) / 100.0))
                .ml(crate::zoom::ui(-6.0))
                .mt(crate::zoom::ui(-6.0))
                .size(crate::zoom::ui(12.0))
                .rounded_full()
                .border_2()
                .border_color(t.text_main)
                .bg(user_color(rgb, 100)),
        )
        .child(probe(0));
    let square = pointer(square, PickerPart::Square, 0, cx);
    // A slider: its track (any element), a thumb at fraction `at`.
    let slider = |id: &'static str, track: gpui::Div, at: f32, on: bool| {
        div()
            .id(id)
            .relative()
            .w_full()
            .h(crate::zoom::ui(SLIDER_H))
            .rounded_sm()
            .border_1()
            .border_color(edge(on))
            .child(track.absolute().size_full().rounded_sm())
            .child(
                div()
                    .absolute()
                    .left(relative(at))
                    .ml(crate::zoom::ui(-3.0))
                    .top(crate::zoom::ui(-3.0))
                    .w(crate::zoom::ui(6.0))
                    .h(crate::zoom::ui(SLIDER_H + 4.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(t.bg)
                    .bg(t.text_main),
            )
    };
    let mut hues = div().flex().flex_row();
    for i in 0..6u16 {
        hues = hues.child(div().flex_1().h_full().bg(linear_gradient(
            90.0,
            linear_color_stop(user_color(from_hsv(i * 60, 100, 100), 100), 0.0),
            linear_color_stop(user_color(from_hsv((i + 1) * 60 % 360, 100, 100), 100), 1.0),
        )));
    }
    let hue = slider(
        "picker-hue",
        hues,
        f32::from(p.h) / 360.0,
        p.zone == Zone::Hue,
    )
    .child(probe(1));
    let hue = pointer(hue, PickerPart::Hue, 1, cx);
    let mut col = div()
        .pl(crate::zoom::ui(12.0 + LABEL_W))
        .pr(crate::zoom::ui(12.0))
        .pt(crate::zoom::ui(4.0))
        .pb(crate::zoom::ui(4.0))
        .flex()
        .flex_col()
        .gap(crate::zoom::ui(8.0))
        .child(square)
        .child(hue);
    if let Some(o) = p.opacity {
        let track = div().bg(linear_gradient(
            90.0,
            linear_color_stop(user_color(rgb, 0), 0.0),
            linear_color_stop(user_color(rgb, 100), 1.0),
        ));
        let op = slider(
            "picker-opacity",
            track,
            f32::from(o) / 100.0,
            p.zone == Zone::Opacity,
        )
        .child(probe(2));
        col = col.child(pointer(op, PickerPart::Opacity, 2, cx));
    }
    let hex = if p.typing {
        format!("#{}▏", p.hex.to_ascii_uppercase())
    } else {
        format!("#{}", p.hex.to_ascii_uppercase())
    };
    let row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(crate::zoom::ui(8.0))
        .text_size(crate::zoom::ui(12.0))
        .child(
            div()
                .size(crate::zoom::ui(24.0))
                .rounded_sm()
                .border_1()
                .border_color(t.control_edge)
                .bg(user_color(rgb, p.opacity.unwrap_or(100))),
        )
        .child(
            div()
                .id("picker-hex")
                .h(crate::zoom::ui(24.0))
                .px(crate::zoom::ui(8.0))
                .flex()
                .items_center()
                .rounded_sm()
                .border_1()
                .border_color(edge(p.zone == Zone::Hex))
                .text_color(t.text_main)
                .child(hex)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        view.picker_zone(Zone::Hex, cx);
                    }),
                ),
        )
        .children(
            p.opacity
                .map(|o| div().text_color(t.text_dim).child(format!("{o}%"))),
        )
        .child(
            div()
                .id("picker-dropper")
                .h(crate::zoom::ui(24.0))
                .px(crate::zoom::ui(8.0))
                .flex()
                .items_center()
                .rounded_sm()
                .border_1()
                .border_color(edge(p.zone == Zone::Dropper))
                .when(e.dropper, |d| d.bg(t.accent).text_color(t.bg))
                .when(!e.dropper, |d| d.text_color(t.text_main))
                .hover(|s| s.bg(t.accent.opacity(0.12)))
                // Armed, the button says how out: the editor may cover any part of the plot,
                // so the hint stays here rather than on a plot banner.
                .child(if e.dropper {
                    "Picking… Esc cancels"
                } else {
                    "Pick from chart"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        view.picker_zone(Zone::Dropper, cx);
                    }),
                ),
        );
    col.child(row)
}

/// The focus ring around a control: 2 px accent when focused, else the same space clear (so
/// focus never moves anything).
fn ring(t: &Theme, focused: bool) -> gpui::Div {
    div()
        .flex_none()
        .rounded_md()
        .border_2()
        .border_color(if focused {
            t.accent
        } else {
            gpui::transparent_black()
        })
}

/// A color row's name line (row `r`), for drawings and studies alike: the focused swatch's
/// name while the focus is on the row, then " · selected: <name>" when another one is
/// selected; else the selected one's (focus and selection are never told by color alone).
pub fn color_row_name(
    e: &StyleEditor,
    row: &Row,
    r: usize,
    names: &[&'static str],
) -> Option<String> {
    let name = |c: &Control| match c {
        Control::Color(Color::Own) => e.own_name().to_owned(),
        Control::FillColor(Color::Own) => e.fill_own_name().to_owned(),
        Control::Color(c) | Control::FillColor(c) => color_name(*c, names),
        c => control_label(*c, names),
    };
    let selected = row.controls.iter().find(|(_, on)| *on);
    match row.controls.get(e.col).filter(|_| e.row == r) {
        Some((c, false)) => Some(match selected {
            Some((s, _)) => format!("{} · selected: {}", name(c), name(s)),
            None => name(c),
        }),
        Some((c, true)) => Some(name(c)),
        None => selected.map(|(c, _)| name(c)),
    }
}

/// The theme color names an editor's palette swatches go by (a study's: the study colors).
pub fn names<'t>(e: &StyleEditor, t: &'t Theme) -> &'t [&'static str; 4] {
    if e.study {
        &t.study_names
    } else {
        &t.drawing_names
    }
}

fn swatch(t: &Theme, fill: Hsla, on: bool) -> gpui::Div {
    // Selected: a 2 px ring in the text color and a dot inside (never color alone).
    div()
        .size(crate::zoom::hit(HIT))
        .p(crate::zoom::ui(2.0))
        .rounded_sm()
        .border_2()
        .border_color(if on {
            t.text_main
        } else {
            gpui::transparent_black()
        })
        .child(
            div()
                .size_full()
                .rounded_sm()
                .bg(fill)
                .border_1()
                .border_color(t.control_edge)
                .flex()
                .items_center()
                .justify_center()
                .when(on, |d| {
                    d.child(
                        div()
                            .size(crate::zoom::ui(4.0))
                            .rounded_full()
                            .bg(t.on_fill(fill)),
                    )
                }),
        )
}

fn checkbox(t: &Theme, on: bool, label: String) -> gpui::Div {
    div()
        .h(crate::zoom::hit(HIT))
        .px(crate::zoom::ui(4.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(crate::zoom::ui(8.0))
        .text_size(crate::zoom::ui(12.0))
        .text_color(t.text_main)
        .child(
            div()
                .size(crate::zoom::ui(14.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(if on { t.accent } else { t.control_edge })
                .when(on, |d| {
                    d.bg(t.accent)
                        .text_color(t.on_last)
                        .text_size(crate::zoom::ui(11.0))
                        .child("✓")
                }),
        )
        .child(label)
}

fn choice(t: &Theme, on: bool, label: String) -> gpui::Div {
    // Selected: a check, a 1 px accent border and a stronger fill (WCAG 1.4.11).
    div()
        .h(crate::zoom::hit(HIT))
        .min_w(crate::zoom::hit(HIT))
        .px(crate::zoom::ui(8.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(crate::zoom::ui(4.0))
        .rounded_sm()
        .border_1()
        .border_color(if on { t.accent } else { t.control_edge })
        .text_size(crate::zoom::ui(12.0))
        .text_color(t.text_main)
        .when(on, |d| {
            d.bg(t.element_selected)
                .font_weight(FontWeight::SEMIBOLD)
                .child("✓")
        })
        .child(label)
}

/// A warning's marker on a neutral chip (the chart's background, an edge that reads): the
/// color with a meaning it looks like, else (low contrast) half text, half background.
fn marker(t: &Theme, swatch: Option<Hsla>) -> gpui::Div {
    let chip = div()
        .mt(crate::zoom::ui(3.0))
        .size(crate::zoom::ui(16.0))
        .flex_none()
        .p(crate::zoom::ui(2.0))
        .rounded_sm()
        .bg(t.bg)
        .border_1()
        .border_color(t.control_edge);
    match swatch {
        Some(c) => chip.child(div().size_full().rounded_sm().bg(c)),
        None => chip.child(
            div()
                .size_full()
                .flex()
                .flex_row()
                .rounded_sm()
                .border_1()
                .border_color(t.text_main)
                .child(div().w_1_2().h_full().bg(t.text_main)),
        ),
    }
}

/// The readability warnings: a neutral surface with a warn-colored bar on the left (never
/// text on the warn color, so the markers keep their contrast).
fn warning_box(t: &Theme, warnings: &[Warning]) -> gpui::Div {
    let mut list = div()
        .flex_1()
        .min_w(px(0.0))
        .px(crate::zoom::ui(8.0))
        .py(crate::zoom::ui(4.0))
        .flex()
        .flex_col()
        .gap(crate::zoom::ui(4.0))
        .text_size(crate::zoom::ui(12.0))
        .text_color(t.text_main);
    for w in warnings {
        list = list.child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_start()
                .gap(crate::zoom::ui(8.0))
                .child(marker(t, w.swatch.map(|rgb| user_color(rgb, 100))))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .whitespace_normal()
                        .child(w.text.clone()),
                ),
        );
    }
    div()
        .mx(crate::zoom::ui(12.0))
        .my(crate::zoom::ui(4.0))
        .flex()
        .flex_row()
        .rounded_sm()
        .bg(t.bg)
        .border_1()
        .border_color(t.control_edge)
        .child(div().w(crate::zoom::ui(3.0)).flex_none().bg(t.warn))
        .child(list)
}

/// How far down a body `view` px tall must be scrolled (from `scrolled`) to show the row at
/// `top` (content px, from the body's top) `h` tall: the least move that shows all of it, or
/// its top when it is taller than the body (a color row's name line stays with its swatches).
pub fn keep_visible(view: f32, top: f32, h: f32, scrolled: f32) -> f32 {
    if h > view || top < scrolled {
        top
    } else if top + h > scrolled + view {
        top + h - view
    } else {
        scrolled
    }
}

/// The scrollbar's thumb (top, height) in a body `view` px tall showing `content` px, scrolled
/// `scrolled` px down; `None` when everything fits.
pub fn thumb(view: f32, content: f32, scrolled: f32) -> Option<(f32, f32)> {
    if content <= view + 0.5 || view <= 0.0 {
        return None;
    }
    let h = (view * view / content).max(16.0_f32.min(view));
    let max = content - view;
    let top = (scrolled.clamp(0.0, max) / max) * (view - h);
    Some((top, h))
}

/// The editor at `at`. `content_h`: [`content_height`] at this width; `es`: the body's scroll
/// state (its scrollbar shows whenever the body doesn't fit; the focus is followed before
/// render, [`EditorScroll::follow`]).
pub fn render(
    e: &StyleEditor,
    t: &Theme,
    warnings: &[Warning],
    at: Place,
    content_h: f32,
    es: &EditorScroll,
    cx: &mut Context<ChartView>,
) -> AnyElement {
    let scroll = &es.handle;
    let rows = e.rows();
    let focus = (e.row, e.col);
    let names = names(e, t);
    // Each row (a color row with its name line, hex entry and warnings) is one direct child of
    // the scroll container, keeping its own height (flex_none).
    let mut groups: Vec<gpui::Div> = Vec::with_capacity(rows.len());
    let mut tabs_strip = None;
    if e.parts.len() > 1 {
        let tab = |i: usize| {
            let on = i == e.tab;
            div()
                .id(("style-tab", i))
                .h(crate::zoom::ui(24.0))
                .px(crate::zoom::ui(8.0))
                .flex()
                .flex_none()
                .items_center()
                .text_size(crate::zoom::ui(12.0))
                .whitespace_nowrap()
                .text_color(if on { t.text_main } else { t.text_dim })
                .when(on, |d| d.font_weight(FontWeight::SEMIBOLD))
                .border_b_2()
                .border_color(if on {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .hover(|s| s.text_color(t.text_main))
                .child(e.parts[i].title)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        view.style_tab(i, cx);
                    }),
                )
        };
        let strip = div()
            .flex_none()
            .px(crate::zoom::ui(12.0))
            .pt(crate::zoom::ui(4.0))
            .pb(crate::zoom::ui(4.0))
            .border_b_1()
            .border_color(t.axis_line);
        tabs_strip = Some(if grouped(e) {
            // A level source's types: one row per group, its heading beside tabs that wrap.
            // Never taller than leaves the body a few rows (it scrolls past that).
            let cap = (at.h - chrome_height(e, at.w) + tabs_height(e, at.w) - 96.0).max(32.0);
            let mut col = div()
                .id("style-tabs")
                .max_h(px(cap))
                .overflow_y_scroll()
                .track_scroll(&es.tabs)
                .flex()
                .flex_col()
                .gap(crate::zoom::ui(2.0));
            let groups = tab_groups(e);
            // The focused tab's group row in view (Tab / Shift+Tab move along the strip).
            if es.tab_shown.get() != Some(e.tab)
                && let Some(gi) = groups.iter().position(|(_, t)| t.contains(&e.tab))
            {
                es.tabs.scroll_to_item(gi);
                es.tab_shown.set(Some(e.tab));
            }
            for (g, tabs) in groups {
                let mut row = div()
                    .flex()
                    .flex_1()
                    .min_w(crate::zoom::ui(0.0))
                    .flex_row()
                    .flex_wrap()
                    .gap(crate::zoom::ui(4.0));
                for i in tabs {
                    row = row.child(tab(i));
                }
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .child(
                            div()
                                .w(crate::zoom::ui(LABEL_W))
                                .flex_none()
                                .h(crate::zoom::ui(24.0))
                                .flex()
                                .items_center()
                                .text_size(crate::zoom::ui(11.0))
                                .text_color(t.text_dim)
                                .whitespace_nowrap()
                                .truncate()
                                .child(g.unwrap_or("")),
                        )
                        .child(row),
                );
            }
            strip.child(col)
        } else {
            let mut row = strip.flex().flex_row().gap(crate::zoom::ui(4.0));
            for i in 0..e.parts.len() {
                row = row.child(tab(i));
            }
            row
        });
    }
    let mut footer_save = None;
    for (r, row) in rows.iter().enumerate() {
        let click = |k: usize| {
            cx.listener(move |view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                view.style_click(r, k, cx);
            })
        };
        if let [(Control::SaveDefault, saved)] = row.controls.as_slice() {
            let focused = focus == (r, 0);
            let label = if *saved {
                "Saved as the default".to_owned()
            } else {
                e.default_label
                    .clone()
                    .unwrap_or_else(|| control_label(Control::SaveDefault, names))
            };
            footer_save = Some(
                ring(t, focused).child(
                    div()
                        .id(("style-ctl", r * 64))
                        .h(crate::zoom::ui(24.0))
                        .px(crate::zoom::ui(8.0))
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(t.control_edge)
                        .text_size(crate::zoom::ui(12.0))
                        .text_color(if *saved { t.text_dim } else { t.text_main })
                        .hover(|s| s.bg(t.element_hover))
                        .child(label)
                        .on_mouse_down(MouseButton::Left, click(0)),
                ),
            );
            continue;
        }
        let is_color = is_color_row(row);
        let main = row.label == "Color";
        let mut group = div().flex_none().flex().flex_col();
        let mut controls = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(crate::zoom::ui(4.0));
        if e.row == r {
            // Where the focused row's controls are (content px, from this frame's scrolled
            // bounds), for a key that moves within the row (`EditorScroll::follow`).
            let (handle, measured, at_key) = (scroll.clone(), es.controls.clone(), (e.tab, r));
            controls = controls.on_children_prepainted(move |bounds, _, _| {
                let view = handle.bounds();
                let scrolled = handle.offset().y;
                let spans = bounds
                    .iter()
                    .map(|b| {
                        let top = b.top() - view.top() - scrolled;
                        (f32::from(top), f32::from(b.size.height))
                    })
                    .collect();
                *measured.borrow_mut() = Some((at_key, spans));
            });
        }
        for (k, &(c, on)) in row.controls.iter().enumerate() {
            let focused = focus == (r, k);
            let inner = match c {
                Control::Color(color) => swatch(t, t.swatch_color(e, color), on),
                Control::FillColor(color) => swatch(t, t.fill_swatch_color(e, color), on),
                c if is_toggle(c) => checkbox(t, on, control_label(c, names)),
                c => choice(t, on, control_label(c, names)),
            };
            controls = controls.child(
                ring(t, focused).child(
                    inner
                        .id(("style-ctl", r * 64 + k))
                        .on_mouse_down(MouseButton::Left, click(k)),
                ),
            );
        }
        let label = div()
            .w(crate::zoom::ui(LABEL_W))
            .flex_none()
            .text_size(crate::zoom::ui(11.0))
            .text_color(t.text_dim)
            .child(row.label);
        if is_color {
            let named = color_row_name(e, row, r, names);
            group = group.child(
                div()
                    .px(crate::zoom::ui(12.0))
                    .pt(crate::zoom::ui(4.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(label)
                    .when_some(named, |d, n| {
                        // One line: a long name truncates (the height estimate counts one).
                        d.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .whitespace_nowrap()
                                .truncate()
                                .text_size(crate::zoom::ui(12.0))
                                .text_color(t.text_main)
                                .child(n),
                        )
                    }),
            );
            group = group.child(
                div()
                    .pl(crate::zoom::ui(12.0 + LABEL_W))
                    .pr(crate::zoom::ui(12.0))
                    .pb(crate::zoom::ui(2.0))
                    .child(controls),
            );
        } else {
            group = group.child(
                div()
                    .px(crate::zoom::ui(12.0))
                    .py(crate::zoom::ui(2.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(label)
                    .child(controls),
            );
        }
        if is_color {
            // A study part's note (how its colors apply), the custom color entry, then the
            // readability warnings, right under the colors.
            if let Some(note) = e.parts.get(e.tab).and_then(|p| p.note).filter(|_| main) {
                group = group.child(
                    div()
                        .px(crate::zoom::ui(12.0))
                        .pb(crate::zoom::ui(4.0))
                        .text_size(crate::zoom::ui(11.0))
                        .text_color(t.text_dim)
                        .whitespace_normal()
                        .child(note),
                );
            }
            if let Some(p) = e.custom.as_ref().filter(|p| p.fill != main) {
                group = group.child(picker(e, p, t, es, cx));
            }
            if main && !warnings.is_empty() {
                group = group.child(warning_box(t, warnings));
            }
        }
        groups.push(group);
    }
    debug_assert_eq!(
        groups.len(),
        body_len(&rows),
        "one body child per row (`body_child`)"
    );
    let footer = footer_save.map(|s| {
        div()
            .flex_none()
            .border_t_1()
            .border_color(t.axis_line)
            .child(
                div()
                    .px(crate::zoom::ui(12.0))
                    .py(crate::zoom::ui(4.0))
                    .flex()
                    .flex_row()
                    .child(s),
            )
    });
    // The scrollbar: from the body's last layout, else (first frame) from the estimate.
    let view_h = f32::from(scroll.bounds().size.height);
    let (view_h, body_h) = if view_h > 0.0 {
        (view_h, view_h + f32::from(scroll.max_offset().y))
    } else {
        let view = (at.h - chrome_height(e, at.w)).max(0.0);
        (view, (content_h - chrome_height(e, at.w)).max(0.0))
    };
    let bar = thumb(view_h, body_h, -f32::from(scroll.offset().y)).map(|(top, h)| {
        div()
            .absolute()
            .top(px(top))
            .right(crate::zoom::ui(2.0))
            .w(crate::zoom::ui(4.0))
            .h(px(h))
            .rounded_full()
            .bg(t.text_dim)
    });
    let body = div()
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .id("style-editor-body")
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(crate::zoom::ui(4.0))
                .children(groups),
        )
        .when_some(bar, |d, b| d.child(b));
    div()
        .absolute()
        .left(px(at.x))
        .top(px(at.y))
        .child(
            div()
                .id("style-editor")
                .occlude()
                .w(px(at.w))
                .max_h(px((at.h - crate::popover::title_h()).max(0.0)))
                .overflow_hidden()
                .flex()
                .flex_col()
                .bg(t.panel)
                .border_1()
                .border_color(t.axis_line)
                .rounded_b_md()
                .text_size(crate::zoom::ui(13.0))
                .children(tabs_strip)
                .child(body)
                .children(footer),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P4: a study's editor names its focused swatch and, apart, the selected one, as a
    /// drawing's does ("Teal · selected: Theme").
    #[test]
    fn a_study_editor_names_focus_and_selection_apart() {
        use crate::study_style::{FOOTPRINT, Look, editor};
        let t = crate::theme::Theme::dark(crate::theme::Palette::Standard);
        let mut e = editor(
            "Footprint style".into(),
            &FOOTPRINT,
            &Look::defaults(&FOOTPRINT),
            None,
        );
        let names = names(&e, &t);
        let r = e.rows().iter().position(|r| r.label == "Color").unwrap();
        let rows = e.rows();
        assert_eq!(
            color_row_name(&e, &rows[r], r, names).as_deref(),
            Some("Theme")
        );
        // Focus the second study color (Teal): named first, the selection after it.
        e.row = r;
        e.col = 2;
        let rows = e.rows();
        assert_eq!(
            color_row_name(&e, &rows[r], r, names).as_deref(),
            Some("Teal · selected: Theme")
        );
    }

    /// P2: a study's editor opens at the left under the legend, clear of the newest bars at
    /// the right, and never past the plot's bottom.
    #[test]
    fn a_study_editor_opens_left_under_the_legend() {
        let area = (1400.0, 760.0);
        let p = place_study(area, 120.0, 0.0, 400.0);
        // Touching the legend it opens from.
        assert_eq!((p.x, p.y), (MARGIN, 120.0));
        assert!(p.x + p.w < area.0 * 0.5, "left half: {p:?}");
        inside(p, area);
        // Too tall to fit below the legend: moved up, inside the area.
        let p = place_study(area, 500.0, 0.0, 400.0);
        assert!(p.y + p.h <= area.1 - MARGIN && p.y >= MARGIN, "{p:?}");
        inside(p, area);
        // Taller than the area: as tall as fits, scrolling.
        let p = place_study((600.0, 300.0), 120.0, 0.0, 900.0);
        assert!(p.y >= MARGIN && p.y + p.h <= 300.0 - MARGIN + 0.01, "{p:?}");
        inside(p, (600.0, 300.0));
    }

    /// The volume profile's editor never covers the profile (its POC included) when the plot
    /// has room: right of its widest row, else at the plot's center; always clear of the
    /// newest bars (the plot's right quarter), inside the area.
    #[test]
    fn the_profile_editor_sits_clear_of_the_profile() {
        let area = (1400.0, 760.0);
        let plot_w = area.0 - crate::chart_view::AXIS_W_100;
        let newest = plot_w * 0.75;
        // A quarter-wide profile: right of it.
        let p = place_study(area, 120.0, plot_w * 0.25, 400.0);
        assert_eq!(p.x, plot_w * 0.25 + GAP);
        assert!(p.x + p.w <= newest, "{p:?}");
        inside(p, area);
        // A wide profile: the editor can't fit right of it before the newest bars, so it
        // sits at the plot's center (still right of the profile)... or, when even that
        // overlaps the profile, at the left.
        for clear in [plot_w * 0.4, plot_w * 0.6] {
            let p = place_study(area, 120.0, clear, 400.0);
            assert!(p.x + p.w <= newest + 0.01, "{p:?}");
            inside(p, area);
            if p.x > MARGIN {
                assert!(p.x >= clear, "{p:?} covers the profile ({clear})");
            }
        }
        // A narrow pane: at the left, inside.
        let narrow = (520.0, 400.0);
        let p = place_study(narrow, 60.0, 100.0, 300.0);
        inside(p, narrow);
    }

    fn inside(p: Place, pane: (f32, f32)) {
        assert!(p.x >= MARGIN - 0.01 && p.y >= MARGIN - 0.01, "{p:?}");
        assert!(p.x + p.w <= pane.0 - MARGIN + 0.01, "{p:?} in {pane:?}");
        assert!(p.h <= pane.1 - 2.0 * MARGIN + 0.01, "{p:?}");
        assert!(p.y + p.h <= pane.1 - MARGIN + 0.01, "{p:?} in {pane:?}");
    }

    fn overlaps(p: Place, h: f32, (x0, y0, x1, y1): (f32, f32, f32, f32)) -> bool {
        let h = h.min(p.h);
        p.x < x1 && p.x + p.w > x0 && p.y < y1 && p.y + h > y0
    }

    /// Beside the drawing when there's room (right, then left, below, above), never over it.
    #[test]
    fn the_editor_sits_beside_its_drawing() {
        let pane = (1600.0, 900.0);
        let h = 300.0;
        // A note on the left: to its right.
        let note = (100.0, 200.0, 220.0, 224.0);
        let p = place(pane, Some(note), h);
        assert!(p.x >= 220.0 && !overlaps(p, h, note));
        // A note near the right edge: to its left.
        let note = (1300.0, 200.0, 1420.0, 224.0);
        let p = place(pane, Some(note), h);
        assert!(p.x + p.w <= 1300.0 && !overlaps(p, h, note));
        // A horizontal line across the chart: below it, or above when it is low.
        let line = (0.0, 300.0, 1528.0, 300.0);
        let p = place(pane, Some(line), h);
        assert!(p.y > 300.0 && !overlaps(p, h, line));
        let line = (0.0, 800.0, 1528.0, 800.0);
        let p = place(pane, Some(line), h);
        assert!(p.y + h < 800.0 && !overlaps(p, h, line));
        for p in [
            place(pane, Some(note), h),
            place(pane, Some(line), h),
            place(pane, None, h),
        ] {
            inside(p, pane);
        }
    }

    /// A narrow or short pane: never past the left edge, narrower, and its body scrolls.
    #[test]
    fn the_editor_fits_narrow_and_short_panes() {
        for pane in [
            (300.0, 900.0),
            (500.0, 200.0),
            (220.0, 160.0),
            (1600.0, 240.0),
        ] {
            for target in [None, Some((10.0, 10.0, 200.0, 40.0))] {
                let p = place(pane, target, 520.0);
                inside(p, pane);
                assert!(p.w <= W);
            }
        }
        assert_eq!(place((300.0, 900.0), None, 400.0).w, 300.0 - 2.0 * MARGIN);
    }

    /// A pane that can spare it keeps the price axis clear (the last-price tag stays
    /// visible), beside any drawing; a narrower one lets the editor cover it.
    #[test]
    fn the_editor_keeps_the_price_axis_clear() {
        for pane in [(1600.0, 900.0), (700.0, 400.0), (420.0, 300.0)] {
            for target in [
                None,
                Some((10.0, 10.0, 60.0, 40.0)),
                Some((
                    pane.0 - crate::chart_view::AXIS_W_100 - 40.0,
                    100.0,
                    pane.0 - crate::chart_view::AXIS_W_100 - 10.0,
                    120.0,
                )),
                Some((0.0, 200.0, pane.0 - crate::chart_view::AXIS_W_100, 200.0)),
            ] {
                let p = place(pane, target, 300.0);
                inside(p, pane);
                assert!(
                    p.x + p.w <= pane.0 - crate::chart_view::AXIS_W_100 - MARGIN + 0.01,
                    "{p:?} in {pane:?}"
                );
                assert!(p.w >= MIN_BESIDE_AXIS.min(W) - 0.01, "{p:?}");
            }
        }
        // Too narrow to spare it: the whole pane's width.
        assert_eq!(width(330.0), 330.0 - 2.0 * MARGIN);
    }

    /// Every row the keys reach is its own child of the scroll body (so focus can scroll it
    /// into view), in order; "Save as default" is in the footer, outside it.
    #[test]
    fn each_focusable_row_is_a_child_of_the_scroll_body() {
        use lines_core::drawings::{Style, Tool};
        for tool in [Tool::Rect, Tool::Note, Tool::Trend, Tool::HLine] {
            let mut e = StyleEditor::for_drawing(tool, &Style::DEFAULT);
            e.default_label = Some("Save as default".into());
            for tab in 0..e.parts.len() {
                e.select_tab(tab);
                let rows = e.rows();
                let mut next = 0;
                for (r, row) in rows.iter().enumerate() {
                    let footer = matches!(row.controls.as_slice(), [(Control::SaveDefault, _)]);
                    match body_child(&rows, r) {
                        None => assert!(footer, "{tool:?} {tab} row {r}"),
                        Some(ix) => {
                            assert!(!footer);
                            assert_eq!(ix, next, "{tool:?} {tab} row {r}");
                            next += 1;
                        }
                    }
                }
                assert_eq!(body_child(&rows, rows.len()), None);
                // `render` gives the body exactly this many children (it asserts it too).
                assert_eq!(body_len(&rows), next, "{tool:?} {tab}");
                assert_eq!(
                    body_len(&rows),
                    rows.iter()
                        .filter(|r| !matches!(r.controls.as_slice(), [(Control::SaveDefault, _)]))
                        .count()
                );
            }
        }
    }

    /// Every key that moves the focus follows it, within a row too (Left, Right, Home, End),
    /// and across rows and tabs; a key that doesn't move it (or a wheel scroll) doesn't snap
    /// the body back.
    #[test]
    fn every_focus_move_follows() {
        use lines_core::drawings::{Style, Tool};
        let mut e = StyleEditor::for_drawing(Tool::Rect, &Style::DEFAULT);
        let mut f = Follow::default();
        let key = |e: &StyleEditor| (e.tab, e.row, e.col);
        assert!(f.pending(key(&e)), "opened: follows");
        assert!(!f.step(key(&e), true));
        assert!(!f.pending(key(&e)), "followed: a wheel scroll stays");
        for k in ["right", "right", "end", "left", "home", "down", "up", "tab"] {
            let before = key(&e);
            e.key(k, false, None);
            assert_ne!(key(&e), before, "{k} moves the focus");
            assert!(f.pending(key(&e)), "{k} follows");
            assert!(!f.step(key(&e), true));
        }
        // At the first control, Left and Home don't move it: nothing to follow.
        e.key("home", false, None);
        f.step(key(&e), true);
        e.key("left", false, None);
        assert!(!f.pending(key(&e)));
    }

    /// A follow without the layout it needs waits a few frames for it, then settles; once
    /// settled the same focus doesn't follow again, another one does (fresh tries).
    #[test]
    fn a_follow_waits_for_its_layout_then_settles() {
        let mut f = Follow::default();
        let a = (0, 2, 0);
        for _ in 0..FOLLOW_TRIES {
            assert!(f.step(a, false), "waits");
            assert!(f.pending(a));
        }
        assert!(!f.step(a, false), "gives up: settles");
        assert!(!f.pending(a));
        let b = (0, 2, 5);
        assert!(f.pending(b));
        assert!(f.step(b, false), "a new focus waits again");
        assert!(!f.step(b, true), "laid out: settles");
        assert!(!f.pending(b));
        assert_eq!(
            f,
            Follow {
                done: Some(b),
                trying: Some(b),
                tries: 0
            }
        );
        // A focus that moves while another still waits gets fresh tries of its own.
        let mut f = Follow::default();
        assert!(f.step(a, false));
        assert!(f.step(a, false));
        let c = (0, 4, 1);
        for _ in 0..FOLLOW_TRIES {
            assert!(f.step(c, false), "fresh tries for the new focus");
        }
        assert!(!f.step(c, false), "then settles");
    }

    /// A row that fits shows whole; a taller one shows its focused control, with the row's
    /// top (a color row's name line) when both fit; not yet measured, its top.
    #[test]
    fn a_tall_row_follows_its_focused_control() {
        let view = 100.0;
        assert_eq!(
            follow_span(view, (40.0, 60.0), Some((70.0, 24.0))),
            (40.0, 60.0)
        );
        let row = (40.0, 300.0);
        assert_eq!(follow_span(view, row, None), row);
        assert_eq!(follow_span(view, row, Some((60.0, 24.0))), (40.0, 44.0));
        assert_eq!(follow_span(view, row, Some((240.0, 24.0))), (240.0, 24.0));
        // Scrolled to the top, Right to a swatch on the fifth line brings it into view, and
        // Home back to the first line.
        let (top, h) = follow_span(view, row, Some((40.0 + 18.0 + 4.0 * 28.0, 24.0)));
        let to = keep_visible(view, top, h, 0.0);
        assert!(top >= to && top + h <= to + view, "{to}");
        let (top, h) = follow_span(view, row, Some((60.0, 24.0)));
        let back = keep_visible(view, top, h, to);
        assert!(
            top >= back && top + h <= back + view && back <= 40.0,
            "{back}"
        );
    }

    /// The tallest editor: a zone's line tab with the hex entry open and every warning the
    /// guard can give (each meaning, named in the longest way, and low contrast).
    fn tallest() -> (StyleEditor, Vec<Warning>) {
        use lines_core::drawings::{Style, Tool};
        let mut e = StyleEditor::for_drawing(Tool::Rect, &Style::DEFAULT);
        let rows = e.rows();
        let custom = rows[0].controls.len() - 1;
        e.click(0, custom);
        e.open_custom(false, lines_core::style::Rgb::hex(0x808080)); // the picker
        assert!(e.custom.is_some());
        let long = " in the dark theme, colorblind palette; the light theme, standard palette; and the light theme, colorblind palette";
        let mut w: Vec<Warning> = [
            "up",
            "down",
            "last-price",
            "warning",
            "order target",
            "order stop",
        ]
        .iter()
        .map(|n| Warning {
            text: format!("Looks like the {n} color{long}; may be misread as {n}."),
            swatch: Some(lines_core::style::Rgb::hex(0x3fcf9a)),
        })
        .collect();
        w.push(Warning {
            text: "Low contrast in the dark theme (1.2:1) and the light theme (1.4:1); lines need 3:1."
                .into(),
            swatch: None,
        });
        (e, w)
    }

    /// The placed box (at most the height it was placed with) stays inside the chart's plot
    /// area for the tallest content, in wide, narrow, short and tiled panes, beside any
    /// drawing; when the content fits, it isn't cut.
    #[test]
    fn the_tallest_editor_stays_inside_the_plot_area() {
        let names = crate::theme::Theme::dark(crate::theme::Palette::Standard).drawing_names;
        let (e, warnings) = tallest();
        for area in [
            (1600.0, 876.0),
            (1400.0, 360.0),
            (420.0, 276.0),
            (300.0, 900.0),
            (900.0, 200.0),
            (220.0, 160.0),
            (800.0, 400.0),
        ] {
            let w = width(area.0);
            let h = content_height(&e, &warnings, w, &names);
            for target in [
                None,
                Some((10.0, 10.0, 200.0, 40.0)),
                Some((0.0, area.1 - 12.0, area.0 - 72.0, area.1 - 12.0)),
                Some((area.0 * 0.4, area.1 - 30.0, area.0 * 0.5, area.1 - 10.0)),
            ] {
                let p = place(area, target, h);
                inside(p, area);
                if h <= area.1 - 2.0 * MARGIN {
                    assert_eq!(p.h, h, "fits: not cut ({area:?})");
                }
            }
        }
        // Narrower: the rows and warnings wrap, so it's taller.
        let wide = content_height(&e, &warnings, W, &names);
        let narrow = content_height(&e, &warnings, 260.0, &names);
        assert!(narrow > wide, "{narrow} > {wide}");
        assert!(wide > content_height(&e, &[], W, &names) + 7.0 * 19.0);
    }

    /// A level source's editor with types: its tabs are grouped (the source's own first), the
    /// grouped strip is counted in the height (taller at narrow widths, where groups wrap), the
    /// box stays inside the plot area, and every row is a body child.
    #[test]
    fn a_level_editor_groups_its_types_and_fits() {
        use crate::study_style::level_editor;
        use lines_core::levels::LabelMode;
        let names = crate::theme::Theme::dark(crate::theme::Palette::Standard).study_names;
        let mut e = level_editor(
            "Levels · Demo style".into(),
            &Default::default(),
            &lines_feed::levels::demo::TYPES,
            LabelMode::Name,
            None,
            |_| None,
        );
        let groups = tab_groups(&e);
        assert_eq!(
            groups.iter().map(|(g, _)| *g).collect::<Vec<_>>(),
            [
                Some(crate::study_style::SOURCE_GROUP),
                Some("Lines"),
                Some("Zones")
            ]
        );
        assert!(tabs_height(&e, W) > 2.0 * 24.0, "a row per group");
        assert!(tabs_height(&e, 280.0) >= tabs_height(&e, W));
        for tab in 0..e.parts.len() {
            e.select_tab(tab);
            let rows = e.rows();
            assert_eq!(body_len(&rows), rows.len(), "tab {tab}: no footer row");
            for area in [(1600.0, 876.0), (420.0, 276.0), (900.0, 200.0)] {
                let w = width(area.0);
                let h = content_height(&e, &[], w, &names);
                let p = place_study(area, 60.0, 0.0, h);
                inside(p, area);
            }
        }
        // The zone type's tab has its fill rows.
        e.select_tab(e.parts.iter().position(|p| p.title == "Zone").unwrap());
        assert!(e.rows().iter().any(|r| r.label == "Fill"));
        assert!(e.rows().iter().any(|r| r.label == "Fill opacity"));
    }

    /// Focus moved to a row out of view scrolls the least that shows it; a row taller than
    /// the body shows from its top; a row in view doesn't scroll.
    #[test]
    fn the_focused_row_scrolls_into_view() {
        // Body 100 px tall, scrolled 50.
        assert_eq!(keep_visible(100.0, 80.0, 24.0, 50.0), 50.0, "in view");
        assert_eq!(
            keep_visible(100.0, 140.0, 24.0, 50.0),
            64.0,
            "below: to its bottom"
        );
        assert_eq!(
            keep_visible(100.0, 20.0, 24.0, 50.0),
            20.0,
            "above: to its top"
        );
        assert_eq!(keep_visible(100.0, 130.0, 24.0, 50.0), 54.0, "partly below");
        assert_eq!(
            keep_visible(100.0, 200.0, 180.0, 0.0),
            200.0,
            "taller: its top"
        );
        assert_eq!(
            keep_visible(100.0, 0.0, 180.0, 60.0),
            0.0,
            "taller, partly above"
        );
        for (top, h, scrolled) in [(0.0, 24.0, 300.0), (500.0, 24.0, 0.0), (90.0, 30.0, 0.0)] {
            let to = keep_visible(100.0, top, h, scrolled);
            assert!(
                top >= to && top + h <= to + 100.0,
                "{top} {h} {scrolled}: {to}"
            );
        }
    }

    /// The scrollbar shows exactly when the body doesn't fit, its thumb inside the track and
    /// at the bottom when scrolled to the end.
    #[test]
    fn the_scrollbar_shows_when_the_body_scrolls() {
        assert_eq!(thumb(200.0, 180.0, 0.0), None);
        let (top, h) = thumb(100.0, 400.0, 0.0).unwrap();
        assert_eq!((top, h), (0.0, 25.0));
        let (top, h) = thumb(100.0, 400.0, 300.0).unwrap();
        assert!((top + h - 100.0).abs() < 0.01);
        let (top, _) = thumb(100.0, 400.0, 9999.0).unwrap();
        assert!(top <= 75.0 + 0.01);
    }
}
