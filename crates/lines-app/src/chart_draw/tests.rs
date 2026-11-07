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
    // On the line: the drawings take it (select, drag); off it: the chart does.
    r.frame(0.0, 0.0);
    let y = r.d.drawn()[0].p0.1;
    assert!(r.down(500.0, y + 2.0));
    assert_eq!(r.d.selected, Some(r.items()[0].id));
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
