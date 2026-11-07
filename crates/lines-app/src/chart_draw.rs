//! A chart's drawings (roadmap 11): drawing mode, selection, dragging, the temporary
//! measure and note typing, and painting. The model is `lines_core::drawings`; the shared
//! per-product set is `crate::drawings_store`.
//!
//! Input is kept separable from the rest of the chart: drawings take a click only in a
//! drawing mode (a tool chosen) or on a drawing (its line, zone, note, or a selected one's
//! handle), and never a Ctrl+click (reserved for chart trading). Handlers record what was
//! pressed; the frame's `prepare` snaps the pointer (as the crosshair does), so previews and
//! drags always match what is drawn.

use std::sync::Arc;

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, fill, point, px, size};
use lines_core::drawings::{
    Anchor, Drawing, DrawingId, Hit, MeasureStats, Projected, Style, TimeAxis, Tool, measure, pick,
    project, snap_anchor,
};
use lines_core::instrument::Ratio;
use lines_core::{PriceFormat, UtcMillis, Viewport};

use crate::drawings_store::{MAX_NOTE, ProductDrawings};
use crate::theme::Theme;

/// Pointer tolerance for a line (logical px).
pub const HIT_TOL: f32 = 5.0;
/// Half the size of a selected drawing's handle squares (logical px).
pub const HANDLE_R: f32 = 5.0;
/// A press that moves less than this is a click (two-anchor tools: click, click).
const CLICK_SLOP: f32 = 4.0;

/// What a press on a drawing started.
#[derive(Clone, Debug)]
struct Drag {
    hit: Hit,
    /// The drawing as it was, and the anchor under the pointer at the press.
    original: Drawing,
    from: Anchor,
    /// Where it was pressed (plot px): a release within `CLICK_SLOP` is a click (select only).
    press: (f32, f32),
}

/// What a key did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Handled,
    Ignored,
}

/// The chart's drawing state.
pub struct ChartDrawings {
    data: Arc<ProductDrawings>,
    /// `data.revision()` last drawn.
    seen: u64,
    /// Hidden on this chart (saved with the pane): nothing drawn, nothing hit.
    pub hidden: bool,
    /// Drawing mode: the tool to place next.
    tool: Option<Tool>,
    /// A two-anchor tool's first anchor, and where it was pressed (plot px).
    pending: Option<(Anchor, (f32, f32))>,
    pub selected: Option<DrawingId>,
    drag: Option<Drag>,
    /// A drag's drawing this frame (committed on release as one undo step).
    dragged: Option<Drawing>,
    /// The temporary measure (pinned with Enter).
    measure: Option<(Anchor, Anchor)>,
    /// A note being typed at an anchor.
    note: Option<(Anchor, String)>,
    /// Style for new drawings.
    pub style: Style,
    /// "Delete all for <product>" waiting for Enter.
    pub confirm_clear: bool,
    /// The pointer, plot-local (for previews and drags; set by the chart each frame).
    pointer: Option<(f32, f32)>,
    // ---- this frame (prepare → paint) ----
    drawn: Vec<Projected>,
    /// Note texts, by drawn index (copied once per change, not per frame).
    notes: Vec<(DrawingId, String)>,
    preview: Option<Projected>,
    /// Pinned measures' readings, by drawn index.
    pinned: Vec<(usize, MeasureStats)>,
    measure_drawn: Option<(Projected, MeasureStats)>,
    note_drawn: Option<Projected>,
}

impl ChartDrawings {
    pub fn new(data: Arc<ProductDrawings>) -> ChartDrawings {
        ChartDrawings {
            data,
            seen: u64::MAX,
            hidden: false,
            tool: None,
            pending: None,
            selected: None,
            drag: None,
            dragged: None,
            measure: None,
            note: None,
            style: Style::DEFAULT,
            confirm_clear: false,
            pointer: None,
            drawn: Vec::with_capacity(64),
            notes: Vec::new(),
            preview: None,
            pinned: Vec::with_capacity(8),
            measure_drawn: None,
            note_drawn: None,
        }
    }

    /// Another product's drawings (the chart switched markets): anything in progress ends.
    pub fn set_data(&mut self, data: Arc<ProductDrawings>) {
        if !Arc::ptr_eq(&data, &self.data) {
            *self = ChartDrawings {
                hidden: self.hidden,
                style: self.style,
                ..ChartDrawings::new(data)
            };
        }
    }

    #[cfg(test)]
    pub fn data(&self) -> &Arc<ProductDrawings> {
        &self.data
    }

    /// New drawings or edits (from any chart of the product) since the last frame.
    pub fn changed(&mut self) -> bool {
        let r = self.data.revision();
        let changed = r != self.seen;
        self.seen = r;
        changed
    }

    pub fn tool(&self) -> Option<Tool> {
        self.tool
    }

    /// Typing a note: every key goes to it.
    pub fn typing(&self) -> bool {
        self.note.is_some() || self.confirm_clear
    }

    /// Choose a tool (again: leave drawing mode).
    pub fn set_tool(&mut self, tool: Tool) {
        self.pending = None;
        self.note = None;
        self.measure = None;
        self.tool = (self.tool != Some(tool)).then_some(tool);
        if self.tool.is_some() {
            self.hidden = false;
            self.selected = None;
        }
    }

    /// Show or hide the drawings on this chart. Hiding ends everything in progress (tool,
    /// note, measure, selection, drag), so nothing hidden can be deleted or moved.
    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden {
            self.tool = None;
            self.pending = None;
            self.note = None;
            self.measure = None;
            self.selected = None;
            self.drag = None;
            self.dragged = None;
            self.confirm_clear = false;
        }
        self.hidden = hidden;
    }

    /// The button was released where the chart didn't see it (another window took the
    /// pointer): a drag in progress is dropped, the drawing stays as it was.
    pub fn release_lost(&mut self) -> bool {
        self.dragged = None;
        self.drag.take().is_some()
    }

    /// Esc: cancel the tool, then the measure, then the selection.
    pub fn cancel(&mut self) -> bool {
        if self.tool.is_some() || self.pending.is_some() || self.note.is_some() {
            self.tool = None;
            self.pending = None;
            self.note = None;
        } else if self.confirm_clear {
            self.confirm_clear = false;
        } else if self.measure.is_some() {
            self.measure = None;
        } else if self.selected.is_some() || self.drag.is_some() {
            self.selected = None;
            self.drag = None;
            self.dragged = None;
        } else {
            return false;
        }
        true
    }

    /// The pointer this frame (plot-local), or `None` off the plot.
    pub fn set_pointer(&mut self, p: Option<(f32, f32)>) {
        self.pointer = p;
    }

    /// Whether a press at plot-local (`x`, `y`) belongs to the drawings (else the chart
    /// pans or scales as usual). `anchor`: the pointer snapped (grid or magnet). Ctrl+click
    /// never does: it is reserved for chart trading.
    pub fn on_down(&mut self, x: f32, y: f32, ctrl: bool, anchor: Option<Anchor>) -> bool {
        if ctrl || self.hidden {
            return false;
        }
        if self.note.is_some() {
            self.commit_note();
        }
        if let Some(tool) = self.tool {
            let Some(at) = anchor else {
                return true; // no bars yet: nothing to anchor to
            };
            match (tool.anchors(), self.pending) {
                (1, _) if tool == Tool::Note => {
                    self.note = Some((at, String::new()));
                    self.tool = None;
                }
                (1, _) => {
                    self.add(Drawing::new(tool, at, at, self.style));
                    self.tool = None;
                }
                (_, None) => self.pending = Some((at, (x, y))),
                (_, Some((first, _))) => self.finish(tool, first, at),
            }
            return true;
        }
        if let Some((id, hit)) = pick(&self.drawn, self.selected, x, y, HIT_TOL, HANDLE_R)
            && let (Some(d), Some(from)) = (self.data.lock().get(id).cloned(), anchor)
        {
            self.selected = Some(id);
            self.drag = Some(Drag {
                hit,
                original: d,
                from,
                press: (x, y),
            });
            return true;
        }
        // A click elsewhere: the measure goes, the selection too; the chart pans.
        self.measure = None;
        self.selected = None;
        false
    }

    /// The press ended at plot-local (`x`, `y`). Returns whether it was the drawings'.
    pub fn on_up(&mut self, x: f32, y: f32, anchor: Option<Anchor>) -> bool {
        if let (Some(tool), Some((first, at))) = (self.tool, self.pending) {
            // Press-drag-release draws too (a click leaves the first anchor waiting).
            let moved = (x - at.0).abs().max((y - at.1).abs()) > CLICK_SLOP;
            if moved && let Some(b) = anchor {
                self.finish(tool, first, b);
            }
            return true;
        }
        if let Some(drag) = self.drag.take() {
            let moved = (x - drag.press.0).abs().max((y - drag.press.1).abs()) > CLICK_SLOP;
            // A click on a drawing (or its handle) only selects it: nothing moves.
            if let Some(d) = self.dragged.take().filter(|_| moved) {
                self.data.change(|s| (s.edit(d), ()));
            }
            return true;
        }
        false
    }

    fn finish(&mut self, tool: Tool, a: Anchor, b: Anchor) {
        self.pending = None;
        self.tool = None;
        if tool == Tool::Measure {
            self.measure = Some((a, b));
        } else {
            self.add(Drawing::new(tool, a, b, self.style));
        }
    }

    fn add(&mut self, d: Drawing) {
        let id = self.data.change(|s| (true, s.add(d)));
        self.selected = Some(id);
    }

    fn commit_note(&mut self) {
        if let Some((at, text)) = self.note.take() {
            let text = text.trim();
            if !text.is_empty() {
                let mut d = Drawing::new(Tool::Note, at, at, self.style);
                d.text = text.chars().take(MAX_NOTE).collect();
                self.add(d);
            }
        }
    }

    /// Keys for the drawings: note typing, the clear confirmation, Enter to pin a measure,
    /// Delete for the selection, Esc. `text`: the key's character, without Ctrl/Alt/Cmd.
    pub fn key(&mut self, key: &str, text: Option<&str>) -> Key {
        if self.confirm_clear {
            match key {
                "enter" => {
                    self.confirm_clear = false;
                    self.selected = None;
                    self.data.change(|s| (s.clear(), ()));
                }
                "escape" => self.confirm_clear = false,
                _ => {}
            }
            return Key::Handled;
        }
        if let Some((_, s)) = self.note.as_mut() {
            match key {
                "enter" => self.commit_note(),
                "escape" => self.note = None,
                "backspace" => {
                    s.pop();
                }
                _ => {
                    if let Some(c) = text.filter(|c| !c.chars().any(char::is_control))
                        && s.chars().count() < MAX_NOTE
                    {
                        s.push_str(c);
                    }
                }
            }
            return Key::Handled;
        }
        match key {
            "escape" if self.cancel() => Key::Handled,
            "enter" if self.pin_measure() => Key::Handled,
            "delete" | "backspace" if self.delete_selected() => Key::Handled,
            _ => Key::Ignored,
        }
    }

    /// Keep the temporary measure as a drawing.
    pub fn pin_measure(&mut self) -> bool {
        let Some((a, b)) = self.measure.take() else {
            return false;
        };
        self.add(Drawing::new(Tool::Measure, a, b, self.style));
        true
    }

    pub fn delete_selected(&mut self) -> bool {
        let Some(id) = self.selected.take() else {
            return false;
        };
        self.drag = None;
        self.dragged = None;
        self.data.change(|s| {
            let r = s.remove(id);
            (r, r)
        })
    }

    pub fn undo(&mut self) -> bool {
        self.drag = None;
        self.dragged = None;
        self.data.change(|s| {
            let r = s.undo();
            (r, r)
        })
    }

    pub fn redo(&mut self) -> bool {
        self.drag = None;
        self.dragged = None;
        self.data.change(|s| {
            let r = s.redo();
            (r, r)
        })
    }

    /// Restyle the selected drawing (one undo step), or the style for new drawings.
    pub fn restyle(&mut self, f: impl Fn(Style) -> Style) {
        match self
            .selected
            .and_then(|id| self.data.lock().get(id).cloned())
        {
            Some(mut d) => {
                d.style = f(d.style).normalised();
                self.style = d.style;
                self.data.change(|s| (s.edit(d), ()));
            }
            None => self.style = f(self.style).normalised(),
        }
    }

    pub fn count(&self) -> usize {
        self.data.lock().items().len()
    }

    /// The drawing under the pointer (for the cursor).
    pub fn hover_hit(&self) -> Option<Hit> {
        if self.hidden || self.tool.is_some() {
            return None;
        }
        let (x, y) = self.pointer?;
        pick(&self.drawn, self.selected, x, y, HIT_TOL, HANDLE_R).map(|(_, h)| h)
    }

    /// Whether drawing mode or a drag owns the pointer (the chart shows a crosshair cursor
    /// and doesn't pan).
    pub fn active(&self) -> bool {
        self.tool.is_some() || self.drag.is_some()
    }

    // ---- per frame ------------------------------------------------------------------

    /// Under the store lock, after the viewport update: project this frame's drawings,
    /// preview, drag, measure and note. No allocation once the buffers have grown to the
    /// product's drawing count (notes' text is copied only when it changes).
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        axis: &TimeAxis<'_>,
        vp: &Viewport,
        tick: i64,
        magnet: bool,
        fmt: &PriceFormat,
        point_value: Ratio,
    ) {
        self.drawn.clear();
        self.pinned.clear();
        self.preview = None;
        self.measure_drawn = None;
        self.note_drawn = None;
        if self.hidden {
            return;
        }
        let pointer = self
            .pointer
            .and_then(|(x, y)| snap_anchor(axis, vp, tick, magnet, f64::from(x), f64::from(y)));
        // A drag's drawing follows the pointer.
        self.dragged = match (&self.drag, pointer) {
            (Some(drag), Some(at)) => Some(dragged(drag, at, axis, tick)),
            _ => self.dragged.take(),
        };
        let set = self.data.lock();
        if self.drawn.capacity() < set.items().len() {
            self.drawn.reserve(set.items().len() - self.drawn.len());
        }
        let notes_stale = set
            .items()
            .iter()
            .filter(|d| d.tool == Tool::Note)
            .map(|d| (d.id, d.text.as_str()))
            .ne(self.notes.iter().map(|(id, s)| (*id, s.as_str())));
        if notes_stale {
            self.notes = set
                .items()
                .iter()
                .filter(|d| d.tool == Tool::Note)
                .map(|d| (d.id, d.text.clone()))
                .collect();
        }
        for d in set.items() {
            let d = match &self.dragged {
                Some(x) if x.id == d.id => x,
                _ => d,
            };
            if let Some(p) = project(d, axis, vp) {
                if d.tool == Tool::Measure {
                    // Grows only with the number of pinned measures on screen.
                    self.pinned
                        .push((self.drawn.len(), measure(d.a, d.b, axis, fmt, point_value)));
                }
                self.drawn.push(p);
            }
        }
        drop(set);
        if let (Some(tool), Some((first, _)), Some(at)) = (self.tool, self.pending, pointer) {
            let d = Drawing::new(tool, first, at, self.style);
            self.preview = project(&d, axis, vp);
            if tool == Tool::Measure {
                self.measure_drawn = self
                    .preview
                    .map(|p| (p, measure(first, at, axis, fmt, point_value)));
                self.preview = None;
            }
        }
        if let Some((a, b)) = self.measure {
            let d = Drawing::new(Tool::Measure, a, b, self.style);
            self.measure_drawn =
                project(&d, axis, vp).map(|p| (p, measure(a, b, axis, fmt, point_value)));
        }
        if let Some((at, _)) = &self.note {
            self.note_drawn = project(&Drawing::new(Tool::Note, *at, *at, self.style), axis, vp);
        }
    }

    /// A note's box as drawn (the chart measures its text): hit-tests use it.
    pub fn set_note_box(&mut self, i: usize, (w, h): (f32, f32)) {
        if let Some(d) = self.drawn.get_mut(i) {
            d.p1 = (d.p0.0 + w, d.p0.1 + h);
        }
    }

    /// This frame's projected drawings (for notes, the chart's text and hit-tests).
    pub fn drawn(&self) -> &[Projected] {
        &self.drawn
    }

    pub fn note_text(&self, id: DrawingId) -> Option<&str> {
        self.notes
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s.as_str())
    }

    /// The note being typed: where, and its text so far.
    pub fn note_entry(&self) -> Option<(Projected, &str)> {
        Some((self.note_drawn?, self.note.as_ref()?.1.as_str()))
    }

    /// Pinned measures on screen: drawn index and reading.
    pub fn pinned_measures(&self) -> &[(usize, MeasureStats)] {
        &self.pinned
    }

    /// The measure shown (temporary or being drawn) and what it reads.
    pub fn measure_shown(&self) -> Option<(Projected, MeasureStats)> {
        self.measure_drawn
    }

    /// Lines, zones and handles (inside the plot's content mask, after the studies). Text
    /// (notes, the measure's readout) is the chart's: it owns the shaped-text cache.
    pub fn paint(&self, origin: (f32, f32), hair: f32, t: &Theme, window: &mut Window) {
        for d in self.drawn.iter().chain(self.preview.as_ref()) {
            let color = t.drawing[usize::from(d.style.color) % t.drawing.len()];
            paint_shape(d, color, origin, hair, window);
        }
        if let Some((m, stats)) = self.measure_drawn {
            let color = if stats.units >= 0 { t.up } else { t.down };
            paint_shape(&m, color, origin, hair, window);
        }
        if let Some(id) = self.selected
            && let Some(d) = self.drawn.iter().find(|d| d.id == id)
        {
            let color = t.drawing[usize::from(d.style.color) % t.drawing.len()];
            for &(x, y) in &d.handles[..usize::from(d.n_handles)] {
                let r = HANDLE_R - 1.0;
                let b = rect(origin, x - r, y - r, 2.0 * r, 2.0 * r);
                window.paint_quad(fill(b, t.bg));
                outline(window, b, color, hair.max(1.0));
            }
        }
    }
}

/// The drawing being dragged, with the pointer at `at`.
fn dragged(drag: &Drag, at: Anchor, axis: &TimeAxis<'_>, tick: i64) -> Drawing {
    match drag.hit {
        Hit::Handle(i) => drag.original.with_handle(i, at),
        Hit::Body => {
            // Move by whole bars of this chart and whole ticks. Each anchor keeps its offset
            // inside its bar's bucket, so a 1-minute anchor moved on a 15-minute chart keeps
            // its minute (and a drawing never collapses onto bucket starts).
            let slot = |t| axis.index_of(t).map(|i| i.floor() as i64);
            let dslot = match (slot(at.time), slot(drag.from.time)) {
                (Some(a), Some(b)) => a - b,
                _ => 0,
            };
            let dprice = (at.price.0 - drag.from.price.0) / tick.max(1) * tick.max(1);
            let shift = |a: Anchor| Anchor {
                time: slot(a.time)
                    .and_then(|s| Some((s, axis.slot_time(s)?, axis.slot_time(s + dslot)?)))
                    .map_or(a.time, |(_, start, to)| {
                        UtcMillis(to.0 + (a.time.0 - start.0))
                    }),
                price: lines_core::Price(a.price.0 + dprice),
            };
            let mut d = drag.original.clone();
            d.a = shift(d.a);
            d.b = if d.tool.anchors() == 2 {
                shift(d.b)
            } else {
                d.a
            };
            d
        }
    }
}

fn rect(origin: (f32, f32), x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(
        point(px(origin.0 + x), px(origin.1 + y)),
        size(px(w.max(0.0)), px(h.max(0.0))),
    )
}

fn outline(window: &mut Window, b: Bounds<Pixels>, color: Hsla, w: f32) {
    let (x, y) = (f32::from(b.origin.x), f32::from(b.origin.y));
    let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
    let q = |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
    window.paint_quad(fill(q(x, y, bw, w), color));
    window.paint_quad(fill(q(x, y + bh - w, bw, w), color));
    window.paint_quad(fill(q(x, y, w, bh), color));
    window.paint_quad(fill(q(x + bw - w, y, w, bh), color));
}

/// A line from `a` to `b` (plot-local), solid or dashed: axis-aligned ones as quads (crisp),
/// others as a stroked path.
/// Calls `f(start, end)` for each dash along a line `len` long (or once, solid). Generic, so
/// nothing is boxed per line per frame.
fn dash_segments(len: f32, dashed: bool, mut f: impl FnMut(f32, f32)) {
    if dashed {
        let d = crate::theme::DRAWING_DASH;
        let mut s = 0.0;
        while s < len {
            f(s, (s + d.on).min(len));
            s += d.on + d.off;
        }
    } else {
        f(0.0, len);
    }
}

/// A horizontal or vertical line's band across its length: (thickness, start), both on the
/// device-pixel grid (`hair` = one device pixel), centred on `at` as nearly as the grid allows.
pub fn crisp_band(at: f32, width: f32, hair: f32) -> (f32, f32) {
    let hair = if hair > 0.0 { hair } else { 1.0 };
    let w = (width / hair).round().max(1.0) * hair;
    let start = ((at - w * 0.5) / hair).round() * hair;
    (w, start)
}

#[allow(clippy::too_many_arguments)]
fn paint_line(
    a: (f32, f32),
    b: (f32, f32),
    width: f32,
    dashed: bool,
    color: Hsla,
    origin: (f32, f32),
    hair: f32,
    window: &mut Window,
) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    if dy.abs() < 0.01 || dx.abs() < 0.01 {
        // Crisp at every DPI: a whole number of device pixels wide, on device-pixel edges.
        let horizontal = dy.abs() < 0.01;
        let (w, cross) = crisp_band(if horizontal { a.1 } else { a.0 }, width, hair);
        dash_segments(len, dashed, |s, e| {
            let (x0, y0) = (a.0 + ux * s, a.1 + uy * s);
            let (x1, y1) = (a.0 + ux * e, a.1 + uy * e);
            let b = if horizontal {
                rect(origin, x0.min(x1), cross, (x1 - x0).abs(), w)
            } else {
                rect(origin, cross, y0.min(y1), w, (y1 - y0).abs())
            };
            window.paint_quad(fill(b, color));
        });
        return;
    }
    let mut path = PathBuilder::stroke(px(width));
    dash_segments(len, dashed, |s, e| {
        path.move_to(point(
            px(origin.0 + a.0 + ux * s),
            px(origin.1 + a.1 + uy * s),
        ));
        path.line_to(point(
            px(origin.0 + a.0 + ux * e),
            px(origin.1 + a.1 + uy * e),
        ));
    });
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

fn paint_shape(d: &Projected, color: Hsla, origin: (f32, f32), hair: f32, window: &mut Window) {
    let width = f32::from(d.style.width).max(hair);
    match d.tool {
        Tool::HLine | Tool::HRay | Tool::VLine | Tool::Trend | Tool::Ray => {
            paint_line(
                d.p0,
                d.p1,
                width,
                d.style.dashed,
                color,
                origin,
                hair,
                window,
            );
        }
        Tool::Rect | Tool::Measure => {
            let (x0, x1) = (d.p0.0.min(d.p1.0), d.p0.0.max(d.p1.0));
            let (y0, y1) = (d.p0.1.min(d.p1.1), d.p0.1.max(d.p1.1));
            window.paint_quad(fill(
                rect(origin, x0, y0, x1 - x0, y1 - y0),
                color.opacity(0.12),
            ));
            for (a, b) in [
                ((x0, y0), (x1, y0)),
                ((x0, y1), (x1, y1)),
                ((x0, y0), (x0, y1)),
                ((x1, y0), (x1, y1)),
            ] {
                paint_line(a, b, width, d.style.dashed, color, origin, hair, window);
            }
        }
        Tool::Note => {
            // The chart draws the note's box and text; the drawing color marks its edge.
            let (x, y) = d.p0;
            window.paint_quad(fill(rect(origin, x, y, 2.0, d.p1.1 - y), color));
        }
    }
}

#[cfg(test)]
mod tests;
