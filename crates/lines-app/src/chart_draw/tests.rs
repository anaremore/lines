use super::*;
use lines_core::{Bar, BarsView, Price, Qty, UtcMillis};
use lines_feed::catalog;

const M: i64 = 60_000;

fn bars() -> Vec<Bar> {
    (0..120)
        .map(|i| Bar {
            time: UtcMillis(1_800_000_000_000 + i * M),
            open: Price(600_000),
            high: Price(600_100),
            low: Price(599_900),
            close: Price(600_050),
            volume: Qty(3),
        })
        .collect()
}

fn vp() -> Viewport {
    let mut vp = Viewport::default();
    vp.set_size(800.0, 400.0);
    vp.bar_px = 6.0;
    vp.right_index = 119.0;
    vp.price_lo = 599_000.0;
    vp.price_hi = 601_000.0;
    vp
}

struct Rig {
    bars: Vec<Bar>,
    vp: Viewport,
    d: ChartDrawings,
}

impl Rig {
    fn new() -> Rig {
        let es = catalog::get(catalog::ES).unwrap();
        Rig {
            bars: bars(),
            vp: vp(),
            d: ChartDrawings::new(ProductDrawings::ephemeral(es, Vec::new())),
        }
    }

    fn anchor(&self, x: f32, y: f32) -> Option<Anchor> {
        let axis = TimeAxis::new(BarsView::fixed(&self.bars, M));
        snap_anchor(&axis, &self.vp, 25, false, f64::from(x), f64::from(y))
    }

    /// One frame with the pointer at (`x`, `y`).
    fn frame(&mut self, x: f32, y: f32) {
        self.d.set_pointer(Some((x, y)));
        let axis = TimeAxis::new(BarsView::fixed(&self.bars, M));
        let fmt = catalog::get(catalog::ES).unwrap().price;
        self.d
            .prepare(&axis, &self.vp, 25, false, &fmt, Ratio::new(50, 1));
    }

    fn down(&mut self, x: f32, y: f32) -> bool {
        self.frame(x, y);
        let a = self.anchor(x, y);
        self.d.on_down(x, y, false, a)
    }

    fn up(&mut self, x: f32, y: f32) -> bool {
        self.frame(x, y);
        let a = self.anchor(x, y);
        self.d.on_up(x, y, a)
    }

    fn items(&self) -> Vec<Drawing> {
        self.d.data().lock().items().to_vec()
    }
}

#[test]
fn two_anchor_tools_draw_by_click_click_or_drag() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Trend);
    assert!(r.down(100.0, 100.0));
    assert!(
        r.up(101.0, 100.0),
        "a click leaves the first anchor waiting"
    );
    assert!(r.items().is_empty());
    r.frame(300.0, 200.0);
    assert!(r.d.preview.is_some(), "the line follows the pointer");
    assert!(r.down(300.0, 200.0));
    let items = r.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool, Tool::Trend);
    assert_eq!(items[0].a, r.anchor(100.0, 100.0).unwrap());
    assert_eq!(items[0].b, r.anchor(300.0, 200.0).unwrap());
    assert_eq!(r.d.tool(), None, "drawing mode ends after one drawing");
    assert_eq!(r.d.selected, Some(items[0].id));

    // Press, drag, release.
    r.d.set_tool(Tool::Rect);
    assert!(r.down(200.0, 50.0));
    assert!(r.up(400.0, 150.0));
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.items()[1].tool, Tool::Rect);
}

#[test]
fn clicks_belong_to_drawings_only_in_drawing_mode_or_on_a_drawing() {
    let mut r = Rig::new();
    // No tool, nothing drawn: the chart gets the click (pan).
    assert!(!r.down(100.0, 100.0));
    r.d.set_tool(Tool::HLine);
    // Ctrl+click is reserved for chart trading, even in drawing mode.
    let a = r.anchor(100.0, 100.0);
    assert!(!r.d.on_down(100.0, 100.0, true, a));
    assert!(r.down(100.0, 100.0));
    assert_eq!(r.items().len(), 1);
    r.d.selected = None;
    // On an unselected line: it is selected and the chart still pans; once selected, the
    // line takes the press (drag); off it: the chart does.
    r.frame(0.0, 0.0);
    let y = r.d.drawn()[0].p0.1;
    assert!(!r.down(500.0, y + 2.0), "selects, doesn't capture");
    assert_eq!(r.d.selected, Some(r.items()[0].id));
    assert!(r.down(500.0, y + 2.0), "selected: drags");
    assert!(r.up(500.0, y + 2.0));
    assert!(!r.down(500.0, y + 40.0));
    assert_eq!(r.d.selected, None);
    // Hidden on this chart: nothing is hit.
    r.d.hidden = true;
    r.frame(0.0, 0.0);
    assert!(!r.down(500.0, y));
}

/// Dragging a drawing moves it by whole bars and ticks, as one undo step; a handle reshapes
/// one anchor.
#[test]
fn dragging_moves_and_reshapes_as_one_undo_step() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Trend);
    r.down(100.0, 100.0);
    r.down(300.0, 200.0);
    let before = r.items()[0].clone();
    // Body: from the middle, 60 px right (10 bars) and 40 px down.
    r.frame(0.0, 0.0);
    assert!(r.down(200.0, 150.0));
    r.frame(230.0, 170.0);
    r.frame(260.0, 190.0);
    assert!(r.up(260.0, 190.0));
    let after = r.items()[0].clone();
    assert_eq!(after.a.time.0 - before.a.time.0, 10 * M);
    assert_eq!(after.b.time.0 - before.b.time.0, 10 * M);
    let dprice = after.a.price.0 - before.a.price.0;
    assert_eq!(dprice, after.b.price.0 - before.b.price.0);
    assert!(dprice < 0 && dprice % 25 == 0);
    // Handle 1 (the drawing is selected): only `b` moves.
    r.frame(0.0, 0.0);
    let (hx, hy) = r.d.drawn()[0].handles[1];
    assert!(r.down(hx, hy));
    assert!(r.up(hx + 60.0, hy));
    let reshaped = r.items()[0].clone();
    assert_eq!(reshaped.a, after.a);
    assert_eq!(reshaped.b.time.0 - after.b.time.0, 10 * M);
    // Two undo steps back to the original, then redo.
    assert!(r.d.undo());
    assert_eq!(r.items()[0], after);
    assert!(r.d.undo());
    assert_eq!(r.items()[0], before);
    assert!(r.d.redo());
    assert_eq!(r.items()[0], after);
}

#[test]
fn notes_measures_delete_and_clear() {
    let mut r = Rig::new();
    // A note: click, type, Enter.
    r.d.set_tool(Tool::Note);
    assert!(r.down(100.0, 100.0));
    assert!(r.d.typing());
    for c in ["H", "i", "!"] {
        assert_eq!(r.d.key(c, Some(c)), Key::Handled);
    }
    r.d.key("backspace", None);
    r.d.key("enter", None);
    assert!(!r.d.typing());
    let note = r.items()[0].clone();
    assert_eq!((note.tool, note.text.as_str()), (Tool::Note, "Hi"));
    r.frame(0.0, 0.0);
    assert_eq!(r.d.note_text(note.id), Some("Hi"));

    // A measure is temporary until pinned with Enter; a click elsewhere drops it.
    r.d.set_tool(Tool::Measure);
    r.down(200.0, 300.0);
    r.down(400.0, 100.0);
    assert_eq!(r.items().len(), 1, "not saved");
    r.frame(0.0, 0.0);
    let (_, stats) = r.d.measure_shown().unwrap();
    assert!(stats.ticks > 0 && stats.bars > 0);
    assert_eq!(r.d.key("enter", None), Key::Handled);
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.items()[1].tool, Tool::Measure);

    // Delete removes the selection; Esc cancels a tool.
    assert_eq!(r.d.key("delete", None), Key::Handled);
    assert_eq!(r.items().len(), 1);
    r.d.set_tool(Tool::Trend);
    assert_eq!(r.d.key("escape", None), Key::Handled);
    assert_eq!(r.d.tool(), None);
    assert_eq!(r.d.key("escape", None), Key::Ignored);

    // Delete all waits for Enter (Esc keeps them), and undo brings them back.
    r.d.confirm_clear = true;
    assert_eq!(r.d.key("escape", None), Key::Handled);
    assert_eq!(r.items().len(), 1);
    r.d.confirm_clear = true;
    r.d.key("enter", None);
    assert!(r.items().is_empty());
    assert!(r.d.undo());
    assert_eq!(r.items().len(), 1);
}

#[test]
fn restyling_applies_to_the_selection_or_new_drawings() {
    let mut r = Rig::new();
    r.d.restyle(|s| Style { dashed: true, ..s });
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    let d = r.items()[0].clone();
    assert!(d.style.dashed);
    r.d.restyle(|s| Style {
        color: s.color + 1,
        width: 2,
        ..s
    });
    assert_eq!(
        r.items()[0].style,
        Style {
            color: 1,
            width: 2,
            dashed: true
        }
    );
    assert!(r.d.undo());
    assert_eq!(r.items()[0].style, d.style);
}

/// Every chart of a product shares its drawings: a change on one is seen by the other.
#[test]
fn charts_of_a_product_share_drawings() {
    let mut a = Rig::new();
    let mut b = ChartDrawings::new(a.d.data().clone());
    assert!(b.changed());
    assert!(!b.changed());
    a.d.set_tool(Tool::VLine);
    a.down(100.0, 100.0);
    assert!(b.changed());
    assert_eq!(b.count(), 1);
}

/// A drawing made on 1m and moved on a 15m chart keeps each anchor's minute inside its
/// bucket: it moves by whole 15m bars, never collapsing onto bucket starts.
#[test]
fn moving_on_a_higher_timeframe_keeps_minutes() {
    use lines_core::{BarSeries, Derived, Timeframe};
    let mut series = BarSeries::new(M, 1_000);
    for b in bars() {
        series.upsert(b);
    }
    let mut fifteen = Derived::new(Timeframe::Minutes(15), series.capacity());
    fifteen.update(&series, None);
    let axis = TimeAxis::new(fifteen.view());
    let t0 = 1_800_000_000_000; // a 15-minute boundary
    let at = |min: i64, p: i64| Anchor {
        time: UtcMillis(t0 + min * M),
        price: Price(p),
    };
    let original = Drawing::new(Tool::Trend, at(8, 600_000), at(22, 600_100), Style::DEFAULT);
    // Pressed on bar 0 (its start), released on bar 1, 2 ticks higher.
    let drag = Drag {
        hit: Hit::Body,
        original: original.clone(),
        from: at(0, 600_000),
        press: (0.0, 0.0),
    };
    let moved = dragged(&drag, at(15, 600_050), &axis, 25);
    assert_eq!(moved.a, at(23, 600_050));
    assert_eq!(moved.b, at(37, 600_150));
    // Not moved in time: the anchors keep their exact minutes.
    let still = dragged(&drag, at(0, 600_000), &axis, 25);
    assert_eq!((still.a, still.b), (original.a, original.b));
}

/// A click on a selected drawing's handle (no drag) selects; it never moves the anchor.
#[test]
fn a_click_on_a_handle_moves_nothing() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Trend);
    r.down(100.0, 100.0);
    r.down(300.0, 200.0);
    let before = r.items()[0].clone();
    r.frame(0.0, 0.0);
    let (hx, hy) = r.d.drawn()[0].handles[0];
    assert!(r.down(hx + 3.0, hy + 3.0));
    assert!(r.up(hx + 3.0, hy + 3.0));
    assert_eq!(r.items()[0], before);
    assert!(!r.d.data().lock().can_redo());
}

/// Choosing a tool clears the temporary measure, so a new measure's preview shows.
#[test]
fn a_new_tool_clears_the_temporary_measure() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Measure);
    r.down(200.0, 300.0);
    r.down(400.0, 100.0);
    r.frame(0.0, 0.0);
    let old = r.d.measure_shown().unwrap().0;
    r.d.set_tool(Tool::Measure);
    r.down(100.0, 50.0);
    r.frame(150.0, 80.0);
    let shown = r.d.measure_shown().unwrap().0;
    assert_ne!(
        shown.p0, old.p0,
        "the new measure's preview, not the old one"
    );
}

/// Hiding drawings ends everything in progress: a hidden drawing can't stay selected and be
/// deleted unseen. A lost release drops a drag without moving the drawing.
#[test]
fn hiding_clears_the_selection_and_a_lost_release_drops_the_drag() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    r.d.set_tool(Tool::Measure);
    r.down(200.0, 300.0);
    r.down(400.0, 150.0);
    r.frame(0.0, 0.0);
    let y = r.d.drawn()[0].p0.1;
    assert!(
        !r.down(500.0, y),
        "selects the line (the chart pans); the measure stays"
    );
    assert!(r.d.selected.is_some());
    r.d.set_hidden(true);
    assert_eq!(r.d.selected, None);
    assert_eq!(r.d.key("delete", None), Key::Ignored);
    assert_eq!(r.items().len(), 1, "nothing deleted while hidden");
    r.d.set_hidden(false);

    let before = r.items()[0].clone();
    r.frame(0.0, 0.0);
    r.down(500.0, y); // select
    assert!(r.down(500.0, y));
    r.frame(500.0, y + 60.0);
    assert!(r.d.release_lost());
    assert!(!r.up(500.0, y + 60.0), "nothing left to release");
    assert_eq!(r.items()[0], before);
}

/// Horizontal and vertical drawing lines sit on whole device pixels at DPI 1 and 2, so they
/// never blur into two faint rows.
#[test]
fn axis_aligned_lines_snap_to_device_pixels() {
    for (hair, width) in [(1.0, 1.0), (1.0, 2.0), (0.5, 1.0), (0.5, 2.0)] {
        for at in [10.0, 10.3, 10.5, 10.75, 211.49] {
            let (w, start) = crisp_band(at, width, hair);
            let dev = |v: f32| (v / hair - (v / hair).round()).abs() < 1e-4;
            assert!(dev(w) && dev(start), "hair {hair} width {width} at {at}");
            assert!((w - width).abs() < 1e-4);
            assert!(
                (start + w * 0.5 - at).abs() <= hair * 0.5 + 1e-4,
                "centred on {at}"
            );
        }
    }
}

/// Inside a zone the chart pans: only its edges select it, and only a selected zone drags.
#[test]
fn the_chart_pans_inside_a_zone() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Rect);
    r.down(100.0, 100.0);
    r.down(500.0, 300.0);
    r.d.selected = None;
    r.frame(0.0, 0.0);
    assert!(!r.down(300.0, 200.0), "inside: the chart's");
    assert_eq!(r.d.selected, None);
    let (x0, _) = r.d.drawn()[0].p0;
    assert!(
        !r.down(x0, 200.0),
        "edge of an unselected zone: selects only"
    );
    assert!(r.d.selected.is_some());
    assert!(r.down(x0, 200.0), "edge of the selected zone: drags");
}
