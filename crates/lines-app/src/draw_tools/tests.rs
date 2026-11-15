//! A dummy plugin drawing tool (tests only, never shipped): registration and the toolbar's
//! listing, `draw` called with the placed points and again while a handle drags (never per
//! frame), its levels in the chart's levels layer, Delete, and never saved.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use lines_core::drawings::{TimeAxis, snap_anchor};
use lines_core::instrument::Ratio;
use lines_core::levels::{LevelShape, LevelStyle, LevelType, LevelValue, TypeStyle};
use lines_core::{Bar, BarsView, Price, Product, Qty, UtcMillis, Viewport};
use lines_feed::catalog;

use super::*;
use crate::chart_draw::ChartDrawings;
use crate::drawings_store::{DrawingsFile, ProductDrawings};
use crate::levels_layer::{LevelsConfig, LevelsLayer};

/// The dummy's id: it serves ZN only, so the other tests' charts (ES, MES, NQ...) never see it.
const ID: &str = "test-pair";

static TYPES: [LevelType; 3] = [
    LevelType::line("pair-a", "Pair start", "Pair", TypeStyle::OWN),
    LevelType::line(
        "pair-b",
        "Pair end",
        "Pair",
        TypeStyle {
            shown: false,
            ..TypeStyle::OWN
        },
    )
    .hover_only(),
    LevelType::zone("pair-zone", "Pair box", "Pair", TypeStyle::OWN),
];

/// Every call's points, in order.
static CALLS: Mutex<Vec<(Anchor, Anchor)>> = Mutex::new(Vec::new());
/// Refuse points whose first price is this (`Err`: nothing drawn).
static REFUSE: AtomicUsize = AtomicUsize::new(0);

struct Pair;

impl DrawTool for Pair {
    fn id(&self) -> &'static str {
        ID
    }
    fn label(&self) -> &'static str {
        "Pair"
    }
    fn icon(&self) -> &'static str {
        "Pr"
    }
    fn serves(&self, product: &Product) -> bool {
        product.id == catalog::ZN
    }
    fn types(&self) -> &'static [LevelType] {
        &TYPES
    }
    fn draw(&self, ctx: &DrawToolContext, a: Anchor, b: Anchor) -> Result<LevelMap, String> {
        CALLS.lock().unwrap().push((a, b));
        if a.price.0 as usize == REFUSE.load(Ordering::Relaxed) {
            return Err("refused".into());
        }
        let style = LevelStyle {
            rgba: 0x8888ccff,
            weight: 1,
            dashed: false,
        };
        // A line from each point to the plot's right edge, and the box between them.
        let (lo, hi) = (a.price.min(b.price), a.price.max(b.price));
        LevelMap::builder(ID, ctx.product.id, a.time)
            .line("pair-a", LevelValue::Tick(a.price), Some("A"), style)
            .from(a.time)
            .line("pair-b", LevelValue::Tick(b.price), Some("B"), style)
            .from(b.time)
            .zone(
                "pair-zone",
                LevelValue::Tick(lo),
                LevelValue::Tick(hi),
                None,
                style,
            )
            .from(a.time.min(b.time))
            .to(a.time.max(b.time))
            .build(ctx.product.price.tick)
            .map_err(|e| e.to_string())
    }
}

/// The tests share the process-wide registry and the call log: one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static S: Mutex<()> = Mutex::new(());
    let g = S.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register_draw_tool(Arc::new(Pair)).unwrap();
    CALLS.lock().unwrap().clear();
    REFUSE.store(0, Ordering::Relaxed);
    g
}

fn calls() -> Vec<(Anchor, Anchor)> {
    CALLS.lock().unwrap().clone()
}

fn zn() -> Product {
    catalog::get(catalog::ZN).unwrap()
}

fn pair() -> Tool {
    tool_of(&Pair)
}

const M: i64 = 60_000;

struct Rig {
    bars: Vec<Bar>,
    vp: Viewport,
    tick: i64,
    d: ChartDrawings,
}

impl Rig {
    fn new() -> Rig {
        let p = zn();
        let tick = p.price.tick;
        let bars = (0..120)
            .map(|i| Bar {
                time: UtcMillis(1_800_000_000_000 + i * M),
                open: Price(600 * tick),
                high: Price(610 * tick),
                low: Price(590 * tick),
                close: Price(605 * tick),
                volume: Qty(3),
            })
            .collect();
        let mut vp = Viewport::default();
        vp.set_size(800.0, 400.0);
        vp.bar_px = 6.0;
        vp.right_index = 119.0;
        vp.price_lo = (560 * tick) as f64;
        vp.price_hi = (640 * tick) as f64;
        Rig {
            bars,
            vp,
            tick,
            d: ChartDrawings::new(ProductDrawings::ephemeral(p, Vec::new())),
        }
    }

    fn anchor(&self, x: f32, y: f32) -> Option<Anchor> {
        let axis = TimeAxis::new(BarsView::fixed(&self.bars, M));
        snap_anchor(&axis, &self.vp, self.tick, false, x.into(), y.into())
    }

    fn frame(&mut self, x: f32, y: f32) {
        self.d.set_pointer(Some((x, y)));
        let axis = TimeAxis::new(BarsView::fixed(&self.bars, M));
        let fmt = zn().price;
        self.d
            .prepare(&axis, &self.vp, self.tick, false, &fmt, Ratio::new(1000, 1));
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

    /// The levels layer's view of this chart's tools (render: their maps; paint: placed), and
    /// the kinds it draws handed to the drawings, as the chart does every frame.
    fn layer_kinds(&mut self, layer: &mut LevelsLayer) {
        layer.set_tool_maps(self.d.tool_maps());
        layer.snapshot(None);
        let store = lines_feed::store::ChartStore::new(
            zn(),
            None,
            lines_core::Capabilities::default(),
            "test",
            M,
            100,
        );
        layer.place(&store.read(), &self.vp);
        self.d.set_drawn_kinds(|id| layer.tool_kinds(id));
        self.frame(0.0, 0.0);
    }

    fn items(&self) -> Vec<lines_core::drawings::Drawing> {
        self.d.data().lock().items().to_vec()
    }

    fn map(&self) -> Option<Arc<LevelMap>> {
        self.d.tool_maps().map(ID).cloned()
    }
}

/// Registered tools serving the chart's product are listed for its toolbar (icon and name);
/// other products' charts, and option contracts, list none. Ids are checked as level sources'.
#[test]
fn registered_tools_are_listed_for_the_products_they_serve() {
    let _s = serial();
    let ctx = |product: Product| DrawToolContext {
        product,
        contract: None,
    };
    let listed: Vec<_> = ToolMaps::new(ctx(zn())).tools().collect();
    assert!(listed.contains(&(pair(), "Pr")), "{listed:?}");
    assert_eq!(pair().label(), "Pair");
    let es = catalog::get(catalog::ES).unwrap();
    assert!(
        ToolMaps::new(ctx(es)).tools().all(|(t, _)| t != pair()),
        "ES isn't served"
    );
    // Through the extension point.
    let ext = crate::Extensions::new().with_draw_tool(Arc::new(Pair));
    assert_eq!(ext.draw_tools()[0].id(), ID);

    struct Bad(&'static str, &'static str);
    impl DrawTool for Bad {
        fn id(&self) -> &'static str {
            self.0
        }
        fn label(&self) -> &'static str {
            "Bad"
        }
        fn icon(&self) -> &'static str {
            self.1
        }
        fn serves(&self, _: &Product) -> bool {
            true
        }
        fn types(&self) -> &'static [LevelType] {
            &[]
        }
        fn draw(&self, _: &DrawToolContext, _: Anchor, _: Anchor) -> Result<LevelMap, String> {
            Err("never".into())
        }
    }
    let register = |id, icon| lines_feed::levels::register_draw_tool(Arc::new(Bad(id, icon)));
    assert!(register("Bad Id", "B").is_err(), "not a slug");
    assert!(register("", "B").is_err());
    assert!(register("way-too-long-for-a-level-source-id", "B").is_err());
    assert!(
        register("bad-icon", "").is_err(),
        "an icon is 1-3 characters"
    );
    assert!(register("bad-icon", "ABCD").is_err());
    assert!(lines_feed::levels::draw_tool("bad-icon").is_none());
}

/// `draw` gets exactly the placed points (click, click), is called again while a handle drags
/// (each new position, before the release) and never on a frame where nothing moved.
#[test]
fn draw_is_called_with_the_placed_points_and_again_while_a_handle_drags() {
    let _s = serial();
    let mut r = Rig::new();
    r.d.set_tool(pair());
    assert!(r.down(100.0, 100.0));
    assert!(r.up(100.0, 100.0), "a click leaves point A waiting");
    // The placement's preview draws as the pointer moves.
    r.frame(300.0, 220.0);
    let a = r.anchor(100.0, 100.0).unwrap();
    assert_eq!(calls().last(), Some(&(a, r.anchor(300.0, 220.0).unwrap())));
    assert!(r.down(300.0, 250.0));
    let b = r.anchor(300.0, 250.0).unwrap();
    let items = r.items();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].tool, items[0].a, items[0].b), (pair(), a, b));
    assert_eq!(r.d.selected, Some(items[0].id), "placed: selected");
    assert_eq!(r.d.tool(), None, "drawing mode ends after one drawing");
    // Time snaps to the bar under the pointer, the price to the grid only (no magnet).
    assert_eq!(a.price.0 % r.tick, 0);
    r.frame(0.0, 0.0);
    assert_eq!(calls().last(), Some(&(a, b)));
    let map = r.map().expect("its levels");
    assert_eq!(map.levels.len(), 3);
    assert_eq!(
        map.levels[0].shape,
        LevelShape::Line(LevelValue::Tick(a.price))
    );

    // Steady frames call nothing.
    let n = calls().len();
    for _ in 0..5 {
        r.frame(0.0, 0.0);
    }
    assert_eq!(calls().len(), n, "never per frame");

    // Selected: two handles, as a two-anchor drawing; drag handle B.
    let p = r.d.drawn()[0];
    assert_eq!(p.n_handles, 2);
    let (hx, hy) = p.handles[1];
    assert!(r.down(hx, hy));
    r.frame(hx + 60.0, hy - 30.0);
    let moving = r.anchor(hx + 60.0, hy - 30.0).unwrap();
    assert_eq!(calls().last(), Some(&(a, moving)), "live while dragging");
    let live = r.map().unwrap();
    assert_eq!(
        live.levels[1].shape,
        LevelShape::Line(LevelValue::Tick(moving.price))
    );
    assert_eq!(r.items()[0].b, b, "not committed before the release");
    assert!(r.up(hx + 60.0, hy - 30.0));
    r.frame(0.0, 0.0);
    assert_eq!(r.items()[0].b, moving);
    // The release committed what the drag drew: no new call for the same points.
    assert_eq!(calls().last(), Some(&(a, moving)));
    // Undo puts B back, and its levels follow.
    assert!(r.d.undo());
    r.frame(0.0, 0.0);
    let back = r.map().unwrap();
    assert_eq!(
        back.levels[1].shape,
        LevelShape::Line(LevelValue::Tick(b.price))
    );
}

/// A drawing whose `draw` fails draws nothing (the others still do); Delete removes a drawing
/// and its levels; hidden drawings draw no levels.
#[test]
fn a_refused_drawing_draws_nothing_and_delete_removes_one() {
    let _s = serial();
    let mut r = Rig::new();
    let at = |r: &Rig, x, y| r.anchor(x, y).unwrap();
    let (a1, b1) = (at(&r, 100.0, 100.0), at(&r, 200.0, 150.0));
    let (a2, b2) = (at(&r, 300.0, 120.0), at(&r, 400.0, 180.0));
    r.d.place(pair(), a1, b1);
    r.d.place(pair(), a2, b2);
    r.frame(0.0, 0.0);
    let map = r.map().unwrap();
    assert_eq!(map.levels.len(), 6, "both drawings in one map");
    // Each drawing is one event (pointing at one highlights it).
    assert_eq!(map.groups.len(), 2);
    assert_ne!(map.levels[0].group, map.levels[3].group);

    // The second is selected (placed last): Delete removes it and its levels.
    assert_eq!(r.d.key("delete", None), crate::chart_draw::Key::Handled);
    r.frame(0.0, 0.0);
    assert_eq!(r.items().len(), 1);
    assert_eq!(r.map().unwrap().levels.len(), 3);

    // Refused: nothing for it, the other stays.
    REFUSE.store(a2.price.0 as usize, Ordering::Relaxed);
    r.d.place(pair(), a2, b2);
    r.frame(0.0, 0.0);
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.map().unwrap().levels.len(), 3);

    // Hidden: no levels; shown again: back.
    r.d.set_hidden(true);
    r.frame(0.0, 0.0);
    assert!(r.map().is_none());
    r.d.set_hidden(false);
    r.frame(0.0, 0.0);
    assert_eq!(r.map().unwrap().levels.len(), 3);

    // All deleted: no map at all (the layer drops the tool).
    r.d.selected = Some(r.items()[0].id);
    assert!(r.d.delete_selected());
    r.d.selected = Some(r.items()[0].id);
    assert!(r.d.delete_selected());
    r.frame(0.0, 0.0);
    assert!(r.map().is_none());
}

/// The levels go to the chart's levels layer as a source keyed by the tool's id: listed for
/// the Studies menu with its kinds (named as the tool declares them), styled by its types (a
/// type off by default isn't drawn), no legend row; gone with the drawings.
#[test]
fn the_levels_go_to_the_levels_layer_under_the_tools_id() {
    let _s = serial();
    let mut r = Rig::new();
    let (a, b) = (
        r.anchor(100.0, 100.0).unwrap(),
        r.anchor(200.0, 150.0).unwrap(),
    );
    r.d.place(pair(), a, b);
    r.frame(0.0, 0.0);
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.set_tool_maps(r.d.tool_maps());
    layer.snapshot(None);
    let sources = layer.sources();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].0, ID);
    assert_eq!(sources[0].1, "Pair");
    assert_eq!(
        sources[0].2,
        vec![
            "pair-a".to_owned(),
            "pair-b".to_owned(),
            "pair-zone".to_owned()
        ]
    );
    assert_eq!(layer.kind_label(ID, "pair-a"), Some("Pair start"));
    assert_eq!(layer.kind_label(ID, "pair-b"), Some("Pair end"));
    assert!(layer.kind_styled_shown(ID, "pair-a"));
    assert!(!layer.kind_styled_shown(ID, "pair-b"), "off by default");
    assert!(layer.kind_hover_only(ID, "pair-b"));
    assert!(layer.legend().is_empty(), "drawings have no legend rows");
    assert_eq!(layer.map(0).map(|m| m.levels.len()), Some(3));

    // A handle moved: the layer takes the new map.
    let rev = layer.revision();
    let b2 = r.anchor(260.0, 150.0).unwrap();
    let id = r.items()[0].id;
    r.d.data().change(|s| {
        let d = s.get(id).unwrap().with_handle(1, b2);
        (s.edit(d), ())
    });
    r.frame(0.0, 0.0);
    layer.set_tool_maps(r.d.tool_maps());
    layer.snapshot(None);
    assert_ne!(layer.revision(), rev);
    assert_eq!(layer.map(0).unwrap().levels[1].from, Some(b2.time));

    // Deleted: the tool leaves the layer.
    r.d.selected = Some(id);
    assert!(r.d.delete_selected());
    r.frame(0.0, 0.0);
    layer.set_tool_maps(r.d.tool_maps());
    layer.snapshot(None);
    assert!(layer.sources().is_empty());
}

/// Plugin drawings are never saved: the drawings file leaves them out (a built-in drawing
/// beside them is saved as always).
#[test]
fn plugin_drawings_are_never_in_the_drawings_file() {
    let _s = serial();
    let mut r = Rig::new();
    let (a, b) = (
        r.anchor(100.0, 100.0).unwrap(),
        r.anchor(200.0, 150.0).unwrap(),
    );
    r.d.place(pair(), a, b);
    r.d.place(Tool::Trend, a, b);
    assert_eq!(r.items().len(), 2);
    let file = DrawingsFile::from_drawings(&zn(), &r.items(), &HashMap::new());
    assert_eq!(file.drawings.len(), 1);
    assert_eq!(file.drawings[0].tool, "trend");
    let text = String::from_utf8(file.encode()).unwrap();
    assert!(!text.contains(ID), "{text}");
    // Read back: only the trend line.
    let (items, skipped) = file.to_drawings(zn().price.tick);
    assert!(skipped.is_empty());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool, Tool::Trend);
}

/// Merging keeps kinds by slug and gives each drawing its own events; a map off the chart's
/// tick grid is refused (as a level source's would be).
#[test]
fn merged_maps_keep_kinds_and_refuse_off_grid_levels() {
    let _s = serial();
    let p = zn();
    let tick = p.price.tick;
    let ctx = DrawToolContext {
        product: p.clone(),
        contract: None,
    };
    let at = |t: i64, ticks: i64| Anchor {
        time: UtcMillis(t),
        price: Price(ticks * tick),
    };
    let one = drawn(&Pair, &ctx, at(1, 600), at(2, 610), tick).unwrap();
    let two = drawn(&Pair, &ctx, at(3, 620), at(4, 630), tick).unwrap();
    let mut parts = [
        Cached {
            id: 1,
            a: at(1, 600),
            b: at(2, 610),
            map: Some(one),
            merged: 0,
        },
        Cached {
            id: 2,
            a: at(3, 620),
            b: at(4, 630),
            map: Some(two),
            merged: 0,
        },
    ];
    let m = merge(&Pair, &ctx, &mut parts, tick).unwrap();
    assert_eq!(
        (parts[0].merged, parts[1].merged),
        (3, 3),
        "all of both merged"
    );
    assert_eq!(m.source, ID);
    assert_eq!(
        m.kinds,
        vec![
            "pair-a".to_owned(),
            "pair-b".to_owned(),
            "pair-zone".to_owned()
        ]
    );
    assert_eq!(m.levels.len(), 6);
    assert_eq!(m.levels[3].kind, 0);
    assert_eq!(m.groups, vec!["d1".to_owned(), "d2".to_owned()]);
    assert!(merge(&Pair, &ctx, &mut [], tick).is_none());
    if tick > 1 {
        // A tool whose own build skipped the grid check: refused here.
        let mut off = drawn(&Pair, &ctx, at(1, 600), at(2, 610), tick).unwrap();
        off.levels[0].shape = LevelShape::Line(LevelValue::Tick(Price(600 * tick + 1)));
        assert!(checked(off, tick).is_err());
    }
}

/// One drawing placed from plot (100, 100) to (300, 250), not selected, and its points as
/// drawn: A's line runs from A to the plot's right edge, B's from B, the box spans A to B.
fn placed_one(r: &mut Rig) -> (DrawingId, (f32, f32), (f32, f32)) {
    let (a, b) = (
        r.anchor(100.0, 100.0).unwrap(),
        r.anchor(300.0, 250.0).unwrap(),
    );
    r.d.place(pair(), a, b);
    r.d.selected = None;
    r.frame(0.0, 0.0);
    let p = r.d.drawn()[0];
    (r.items()[0].id, p.handles[0], p.handles[1])
}

/// A click (press and release, no move) at plot (`x`, `y`): what is selected after it.
fn click(r: &mut Rig, x: f32, y: f32) -> Option<DrawingId> {
    r.down(x, y);
    r.up(x, y);
    r.frame(x, y);
    r.d.selected
}

/// A plugin drawing is picked on what it draws: its levels where they are drawn (a line far
/// to the right of both points, the line starting at B, inside its box), not only its points.
/// Empty chart beside them picks nothing; a click elsewhere drops the selection.
#[test]
fn a_plugin_drawing_is_selected_by_clicking_its_levels() {
    let _s = serial();
    let mut r = Rig::new();
    let (id, ha, hb) = placed_one(&mut r);
    // The kinds the levels layer draws (B's line is a hover-only kind: hit, as hovering it
    // reveals it).
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    r.layer_kinds(&mut layer);
    // A's line, far right of both points.
    assert_eq!(click(&mut r, 760.0, ha.1 + 2.0), Some(id), "A's line");
    // Empty chart: right of the box, between the two lines; then left of the drawing.
    assert_eq!(click(&mut r, 700.0, (ha.1 + hb.1) * 0.5), None, "empty");
    assert_eq!(click(&mut r, 40.0, 380.0), None, "empty");
    // B's line, right of B.
    assert_eq!(click(&mut r, 600.0, hb.1 - 2.0), Some(id), "B's line");
    r.d.selected = None;
    // Inside the box, far from its edges, both lines and the connector: a click selects it.
    let inside = (hb.0 - 20.0, ha.1 + 20.0);
    assert_eq!(
        click(&mut r, inside.0, inside.1),
        Some(id),
        "inside the box"
    );
    // The box's edge (left, below A) is a strong hit: the cursor shows it.
    r.d.selected = None;
    let left = ha.0 - (r.vp.bar_px * 0.5) as f32;
    r.frame(left, ha.1 + 30.0);
    assert_eq!(
        r.d.hover_hit(),
        Some(lines_core::drawings::Hit::Body),
        "edge"
    );
    assert_eq!(click(&mut r, left, ha.1 + 30.0), Some(id), "the box's edge");
    r.d.selected = None;
    // Inside only: weak, so no hover cursor, no highlight, no right-click.
    r.frame(inside.0, inside.1);
    assert_eq!(r.d.hover_hit(), None, "the chart's cursor inside the box");
    assert_eq!(r.d.plugin_at(inside.0, inside.1), None);
    assert!(!r.d.right_click(inside.0, inside.1), "price items kept");
    assert_eq!(
        r.d.hit_at(inside.0, inside.1),
        None,
        "the Shift ruler starts here"
    );
    // Hovering any of it: the hover cursor's hit and the drawing to highlight.
    r.d.selected = None;
    r.frame(760.0, ha.1);
    assert_eq!(r.d.hover_hit(), Some(lines_core::drawings::Hit::Body));
    assert_eq!(r.d.plugin_at(760.0, ha.1), Some((ID, id)));
    assert_eq!(r.d.plugin_at(700.0, (ha.1 + hb.1) * 0.5), None);
    // Right-click selects it too (the pane menu then offers Delete).
    assert!(r.d.right_click(600.0, hb.1));
    assert_eq!(r.d.selected, Some(id));
}

/// The connector between A and B is the drawing's visible spine and is hit too; levels of a
/// kind the levels layer doesn't draw (hidden in the Studies menu, levels off) are not.
#[test]
fn the_connector_is_hit_and_undrawn_levels_are_not() {
    let _s = serial();
    let mut r = Rig::new();
    // The layer draws none of the tool's kinds: only the points and the connector are left.
    r.d.set_drawn_kinds(|_| 0);
    let (id, ha, hb) = placed_one(&mut r);
    let mid = ((ha.0 + hb.0) * 0.5, (ha.1 + hb.1) * 0.5);
    assert_eq!(click(&mut r, 760.0, ha.1), None, "A's line isn't drawn");
    assert_eq!(click(&mut r, hb.0 - 20.0, ha.1 + 20.0), None, "nor the box");
    assert_eq!(click(&mut r, mid.0, mid.1 + 2.0), Some(id), "the connector");
    // Only the box drawn (kind 2 of the merged map): inside it hits again, the lines don't.
    r.d.set_drawn_kinds(|_| 1 << 2);
    r.d.selected = None;
    r.frame(0.0, 0.0);
    assert_eq!(click(&mut r, 760.0, ha.1), None);
    assert_eq!(click(&mut r, hb.0 - 20.0, ha.1 + 20.0), Some(id));
}

/// Selected, Delete (or Backspace) removes the drawing and its levels; undo brings both back.
/// Dragging its body moves the whole drawing, as a built-in two-point drawing's does.
#[test]
fn a_selected_plugin_drawing_deletes_undoes_and_drags() {
    let _s = serial();
    let mut r = Rig::new();
    let (id, ha, _) = placed_one(&mut r);
    assert_eq!(click(&mut r, 760.0, ha.1), Some(id));
    assert_eq!(r.d.key("delete", None), crate::chart_draw::Key::Handled);
    r.frame(0.0, 0.0);
    assert!(r.items().is_empty());
    assert!(r.map().is_none(), "its levels go with it");
    assert!(r.d.undo());
    r.frame(0.0, 0.0);
    assert_eq!(r.items().len(), 1);
    assert_eq!(r.map().unwrap().levels.len(), 3, "and come back");
    // Backspace too.
    assert_eq!(click(&mut r, 760.0, ha.1), Some(id));
    assert_eq!(r.d.key("backspace", None), crate::chart_draw::Key::Handled);
    assert!(r.items().is_empty());
    assert!(r.d.undo());
    r.frame(0.0, 0.0);

    // Drag the selected drawing by A's line: both points move by the same bars and ticks.
    let before = r.items()[0].clone();
    assert_eq!(click(&mut r, 760.0, ha.1), Some(id));
    assert!(r.down(760.0, ha.1), "a selected drawing's body drags");
    r.frame(700.0, ha.1 + 40.0);
    assert!(r.up(700.0, ha.1 + 40.0));
    r.frame(0.0, 0.0);
    let after = r.items()[0].clone();
    assert!(after.a.time < before.a.time && after.a.price < before.a.price);
    assert_eq!(
        after.b.time.0 - after.a.time.0,
        before.b.time.0 - before.a.time.0
    );
    assert_eq!(
        after.b.price.0 - after.a.price.0,
        before.b.price.0 - before.a.price.0
    );
}

/// Two drawings whose levels overlap: a click there picks the newer (drawn on top), as for
/// built-in drawings.
#[test]
fn overlapping_plugin_drawings_pick_the_newer() {
    let _s = serial();
    let mut r = Rig::new();
    let (first, ha, _) = placed_one(&mut r);
    // A second drawing whose A is at the same price, a little later: the A lines overlap.
    let (a, b) = (
        r.anchor(150.0, 100.0).unwrap(),
        r.anchor(350.0, 300.0).unwrap(),
    );
    r.d.place(pair(), a, b);
    r.d.selected = None;
    r.frame(0.0, 0.0);
    let second = r.items()[1].id;
    assert_ne!(first, second);
    assert_eq!(click(&mut r, 760.0, ha.1), Some(second), "the newer wins");
    // Where only the older one is drawn (its A line, before the newer's A): the older.
    r.d.selected = None;
    assert_eq!(click(&mut r, 115.0, ha.1), Some(first));
}

/// Inside a plugin drawing's box is a weak hit: a press-drag there pans the chart (the
/// drawing never moves, selected or not, and isn't selected by the pan); only a click
/// selects it.
#[test]
fn a_press_drag_inside_the_box_pans_and_never_moves_it() {
    let _s = serial();
    let mut r = Rig::new();
    let (id, ha, hb) = placed_one(&mut r);
    let before = r.items()[0].clone();
    let inside = (hb.0 - 20.0, ha.1 + 20.0);
    let drag = |r: &mut Rig| {
        assert!(
            !r.down(inside.0, inside.1),
            "the chart takes the press (pans)"
        );
        r.frame(inside.0 - 80.0, inside.1 + 40.0);
        r.up(inside.0 - 80.0, inside.1 + 40.0);
        r.frame(0.0, 0.0);
    };
    drag(&mut r);
    assert_eq!(r.d.selected, None, "a pan selects nothing");
    assert_eq!(r.items()[0], before);
    // Selected: a press-drag inside still pans, never a body drag.
    assert_eq!(click(&mut r, inside.0, inside.1), Some(id));
    drag(&mut r);
    assert_eq!(r.items()[0], before, "the anchors are unchanged");
    assert_eq!(r.d.selected, Some(id), "still selected");
}

/// A built-in drawing inside a newer plugin drawing's box is still picked by clicking it: any
/// drawing's strong hit beats a weak one, whatever the order.
#[test]
fn an_older_trend_line_inside_a_newer_box_is_still_picked() {
    let _s = serial();
    let mut r = Rig::new();
    let (t0, t1) = (
        r.anchor(240.0, 120.0).unwrap(),
        r.anchor(280.0, 140.0).unwrap(),
    );
    r.d.place(Tool::Trend, t0, t1);
    let trend = r.items()[0].id;
    let (a, b) = (
        r.anchor(100.0, 100.0).unwrap(),
        r.anchor(300.0, 250.0).unwrap(),
    );
    r.d.place(pair(), a, b);
    let plugin = r.items()[1].id;
    r.d.selected = None;
    r.frame(0.0, 0.0);
    let tr = r.d.drawn()[0];
    let on_trend = ((tr.p0.0 + tr.p1.0) * 0.5, (tr.p0.1 + tr.p1.1) * 0.5);
    assert_eq!(
        click(&mut r, on_trend.0, on_trend.1),
        Some(trend),
        "the older trend"
    );
    r.d.selected = None;
    // Elsewhere inside the box: the plugin drawing.
    let p = r.d.drawn()[1];
    let inside = (p.handles[1].0 - 20.0, p.handles[0].1 + 60.0);
    assert_eq!(click(&mut r, inside.0, inside.1), Some(plugin));
}

/// The kinds the levels layer draws, as the drawings are told them: a bit per merged kind,
/// hover-only kinds included; none with the tool's levels off, without a hidden kind's bit,
/// and none before the layer has the tool at all.
#[test]
fn tool_kinds_are_the_drawn_kinds() {
    let _s = serial();
    let mut r = Rig::new();
    placed_one(&mut r);
    let layer = LevelsLayer::new(LevelsConfig::default());
    assert_eq!(layer.tool_kinds(ID), 0, "no slot yet: nothing drawn");
    let kinds = |config: LevelsConfig, r: &mut Rig| {
        let mut layer = LevelsLayer::new(config);
        r.layer_kinds(&mut layer);
        layer.tool_kinds(ID)
    };
    // pair-a, pair-b (hover-only, off by default: latent, included), pair-zone.
    assert_eq!(kinds(LevelsConfig::default(), &mut r), 0b111);
    let mut c = LevelsConfig::default();
    c.toggle_kind(ID, "pair-zone");
    assert_eq!(kinds(c, &mut r), 0b011, "a hidden kind");
    let mut c = LevelsConfig::default();
    c.toggle_source(ID);
    assert_eq!(kinds(c, &mut r), 0, "the tool's levels off");
    // And through them, hit-testing: the box hidden is no box to click.
    let (_, ha, hb) = {
        let p = r.d.drawn()[0];
        (p.id, p.handles[0], p.handles[1])
    };
    let mut c = LevelsConfig::default();
    c.toggle_kind(ID, "pair-zone");
    let mut layer = LevelsLayer::new(c);
    r.layer_kinds(&mut layer);
    assert_eq!(click(&mut r, hb.0 - 20.0, ha.1 + 20.0), None);
}

/// A level source's event inside a plugin drawing's box keeps its hover: the box is a weak
/// hit, so the drawings name no drawing there for the layer to highlight instead.
#[test]
fn inside_the_box_a_level_sources_hover_is_kept() {
    let _s = serial();
    let mut r = Rig::new();
    let (_, ha, hb) = placed_one(&mut r);
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    r.layer_kinds(&mut layer);
    let inside = (hb.0 - 20.0, ha.1 + 20.0);
    layer.set_hot_drawing(r.d.plugin_at(inside.0, inside.1));
    layer.set_pointer(Some(inside));
    // Nothing of the drawing claims the hover: what the layer finds there is its own
    // `group_at` (here the drawing's box itself, a time-bounded zone, as before).
    assert_eq!(
        layer.hot_groups(),
        layer
            .group_at(inside.0, inside.1)
            .map(|(s, g)| (s, g, g + 1))
    );
    // On A's line (strong): the drawing.
    layer.set_hot_drawing(r.d.plugin_at(760.0, ha.1));
    layer.set_pointer(Some((760.0, ha.1)));
    assert_eq!(layer.hot_groups(), Some((0, 0, 1)));
}

/// The levels layer highlights a hovered plugin drawing's every group, found from its id.
#[test]
fn a_hovered_plugin_drawing_is_highlighted_in_the_levels_layer() {
    let _s = serial();
    let mut r = Rig::new();
    placed_one(&mut r);
    let (a, b) = (
        r.anchor(150.0, 100.0).unwrap(),
        r.anchor(350.0, 300.0).unwrap(),
    );
    r.d.place(pair(), a, b);
    r.frame(0.0, 0.0);
    let second = r.items()[1].id;
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.set_tool_maps(r.d.tool_maps());
    layer.snapshot(None);
    layer.set_hot_drawing(Some((ID, second)));
    layer.set_pointer(Some((10.0, 10.0)));
    assert_eq!(
        layer.hot_groups(),
        Some((0, 1, 2)),
        "the second drawing's group"
    );
    layer.set_hot_drawing(None);
    layer.set_pointer(Some((10.0, 10.0)));
    assert_eq!(layer.hot_groups(), None);
    // Pointer away: nothing highlighted, whatever the drawings said.
    layer.set_hot_drawing(Some((ID, second)));
    layer.set_pointer(None);
    assert_eq!(layer.hot_groups(), None);
}

/// On a real chart (GPUI's test platform, which `screenshot` builds): the pane's saved form
/// (the workspace file's) says nothing of a plugin drawing, and the product's drawings file
/// leaves it out. `cargo test -p lines-app --features screenshot --lib draw_tools`.
#[cfg(feature = "screenshot")]
#[gpui::test]
fn a_chart_with_a_plugin_drawing_saves_none_of_it(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext;
    use lines_feed::cqg::mock::MockConfig;
    use lines_feed::{FeedConfig, Source};

    use crate::chart_view::{ChartView, Fixture};
    use crate::feeds::PaneFeed;
    use crate::layout::{LinkGroup, PaneKind};
    use crate::pane::Pane;
    use crate::wsdoc::{PaneDoc, TargetDoc};

    let _s = serial();
    cx.update(|cx| cx.set_global(crate::picker_view::AppSettings::ephemeral()));
    let config = FeedConfig::new(Source::CqgMock(MockConfig::default()));
    let avail = lines_feed::availability::availability(&config.source).unwrap();
    let target = TargetDoc {
        product: catalog::ZN.0,
        symbol: None,
        contract: None,
    };
    let feed = PaneFeed::unavailable_because(&config, &avail, Some(&target), "test".into());
    let fixture = Fixture {
        plugin_drawing: Some(format!(
            "{ID} 2026-11-05T19:30:00Z@{} 2026-11-05T20:15:00Z@{}",
            "110.5", "111"
        )),
        ..Default::default()
    };
    let c = cx.new(|cx| {
        ChartView::new(
            feed,
            crate::theme::Theme::dark(Default::default()),
            true,
            fixture,
            cx,
        )
    });
    let (doc, items) = c.read_with(cx, |c, _| {
        let mut doc = PaneDoc::new(PaneKind::Chart, LinkGroup::default(), Some(target.clone()));
        c.save_state(&mut doc);
        (doc, c.drawings_now())
    });
    assert_eq!(items.len(), 1, "placed from the fixture");
    assert_eq!(items[0].tool, pair());
    let json = serde_json::to_string(&doc).unwrap();
    assert!(!json.contains(ID) && !json.contains("Pair"), "{json}");
    let file = DrawingsFile::from_drawings(&zn(), &items, &HashMap::new());
    assert!(file.drawings.is_empty());
}
