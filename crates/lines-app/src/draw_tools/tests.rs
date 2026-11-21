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
    let one = drawn(
        &Pair,
        &ctx,
        at(1, 600),
        at(2, 610),
        tick,
        &lines_feed::levels::NoPrices,
    )
    .unwrap();
    let two = drawn(
        &Pair,
        &ctx,
        at(3, 620),
        at(4, 630),
        tick,
        &lines_feed::levels::NoPrices,
    )
    .unwrap();
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
    let (m, dropped) = merge(&Pair, &ctx, &mut parts, tick, None, None);
    let m = m.unwrap();
    assert!(!dropped);
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
    assert!(merge(&Pair, &ctx, &mut [], tick, None, None).0.is_none());
    if tick > 1 {
        // A tool whose own build skipped the grid check: refused here.
        let mut off = drawn(
            &Pair,
            &ctx,
            at(1, 600),
            at(2, 610),
            tick,
            &lines_feed::levels::NoPrices,
        )
        .unwrap();
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

// ---- optional seams: snapping to highs and lows, prices, saving, the strip and card ----

/// A second dummy (HG only, so no other test's chart lists it) that uses every optional seam.
const MARK_ID: &str = "test-mark";
static MARK_TYPES: [LevelType; 1] = [LevelType::line("mark", "Mark", "Mark", TypeStyle::OWN)];
/// What `restore` gives.
static RESTORE: Mutex<Vec<(Anchor, Anchor)>> = Mutex::new(Vec::new());
/// Every `changed` call's drawings.
static CHANGED: Mutex<Vec<Vec<(Anchor, Anchor)>>> = Mutex::new(Vec::new());
/// How many times it drew, and whether the last drawing saw price come back through its half.
static MARK_DRAWS: AtomicUsize = AtomicUsize::new(0);
static MARK_CROSSED: Mutex<Option<bool>> = Mutex::new(None);
/// Its overlay: a hollow point at this many ticks (0: none).
static OVERLAY_AT: AtomicUsize = AtomicUsize::new(0);

struct Mark;

impl DrawTool for Mark {
    fn id(&self) -> &'static str {
        MARK_ID
    }
    fn label(&self) -> &'static str {
        "Mark"
    }
    fn icon(&self) -> &'static str {
        "Mk"
    }
    fn serves(&self, product: &Product) -> bool {
        product.id == catalog::HG
    }
    fn types(&self) -> &'static [LevelType] {
        &MARK_TYPES
    }
    fn snap(&self) -> lines_feed::levels::DrawSnap {
        lines_feed::levels::DrawSnap::HighLow
    }
    fn stays_chosen(&self) -> bool {
        true
    }
    fn point_names(&self) -> Option<[&'static str; 2]> {
        Some(["A", "E"])
    }
    fn reads_prices(&self) -> bool {
        true
    }
    fn draw(&self, ctx: &DrawToolContext, a: Anchor, b: Anchor) -> Result<LevelMap, String> {
        self.draw_with(ctx, a, b, &lines_feed::levels::NoPrices)
    }
    fn draw_with(
        &self,
        ctx: &DrawToolContext,
        a: Anchor,
        b: Anchor,
        prices: &dyn DrawPrices,
    ) -> Result<LevelMap, String> {
        MARK_DRAWS.fetch_add(1, Ordering::Relaxed);
        let half = Price((a.price.0 + b.price.0) / 2);
        let up = b.price > a.price;
        *MARK_CROSSED.lock().unwrap() = Some(
            prices
                .first_beyond(b.time, UtcMillis(i64::MAX), half, !up)
                .is_some(),
        );
        let style = LevelStyle {
            rgba: 0x8888ccff,
            weight: 1,
            dashed: false,
        };
        LevelMap::builder(MARK_ID, ctx.product.id, a.time)
            .line("mark", LevelValue::Tick(a.price), Some("A"), style)
            .from(a.time)
            .build(ctx.product.price.tick)
            .map_err(|e| e.to_string())
    }
    fn describe(
        &self,
        _: &DrawToolContext,
        a: Anchor,
        b: Anchor,
        point: usize,
        _: &dyn DrawPrices,
    ) -> Vec<String> {
        let p = if point == 0 { a } else { b };
        vec![format!("point {point} at {}", p.price.0)]
    }
    fn restore(&self, _: &DrawToolContext) -> Vec<(Anchor, Anchor)> {
        RESTORE.lock().unwrap().clone()
    }
    fn overlay(
        &self,
        ctx: &DrawToolContext,
        view: &lines_feed::levels::DrawView,
    ) -> Option<LevelMap> {
        let at = OVERLAY_AT.load(Ordering::Relaxed) as i64;
        (at > 0).then(|| {
            LevelMap::builder(MARK_ID, ctx.product.id, view.visible.0)
                .marker(
                    view.visible.0,
                    LevelValue::Tick(Price(at * ctx.product.price.tick)),
                    Some("C"),
                )
                .hollow()
                .build(ctx.product.price.tick)
                .unwrap()
        })
    }
    fn overlay_key(&self, _: &DrawToolContext, _: &lines_feed::levels::DrawView) -> u64 {
        OVERLAY_AT.load(Ordering::Relaxed) as u64
    }
    fn changed(&self, _: &DrawToolContext, drawings: &[(Anchor, Anchor)]) {
        CHANGED.lock().unwrap().push(drawings.to_vec());
    }
}

fn hg() -> Product {
    catalog::get(catalog::HG).unwrap()
}

fn mark() -> Tool {
    tool_of(&Mark)
}

fn mark_serial() -> std::sync::MutexGuard<'static, ()> {
    let g = serial();
    lines_feed::levels::register_draw_tool(Arc::new(Mark)).unwrap();
    RESTORE.lock().unwrap().clear();
    CHANGED.lock().unwrap().clear();
    MARK_DRAWS.store(0, Ordering::Relaxed);
    *MARK_CROSSED.lock().unwrap() = None;
    OVERLAY_AT.store(0, Ordering::Relaxed);
    g
}

/// One-minute HG bars, one per `(high, low)` in ticks.
fn hg_bars(rows: &[(i64, i64)]) -> Vec<Bar> {
    let tick = hg().price.tick;
    rows.iter()
        .enumerate()
        .map(|(i, &(hi, lo))| Bar {
            time: UtcMillis(1_800_000_000_000 + i as i64 * M),
            open: Price(lo * tick),
            high: Price(hi * tick),
            low: Price(lo * tick),
            close: Price(hi * tick),
            volume: Qty(1),
        })
        .collect()
}

fn hg_vp(n: usize) -> Viewport {
    let tick = hg().price.tick;
    let mut vp = Viewport::default();
    vp.set_size(800.0, 400.0);
    vp.bar_px = 20.0;
    vp.right_index = n as f64 - 1.0;
    vp.price_lo = (500 * tick) as f64;
    vp.price_hi = (700 * tick) as f64;
    vp
}

fn hg_frame(d: &mut ChartDrawings, bars: &[Bar], vp: &Viewport) {
    let axis = TimeAxis::new(BarsView::fixed(bars, M));
    let fmt = hg().price;
    d.prepare(&axis, vp, fmt.tick, false, &fmt, Ratio::new(1, 1));
}

/// A tool that snaps to highs and lows places its points on the bar's high or low (the nearer
/// one) and stays chosen after a placement; other tools snap as before.
#[test]
fn points_snap_to_highs_and_lows_and_the_tool_stays_chosen() {
    let _s = mark_serial();
    let bars = hg_bars(&[(600, 550), (650, 560), (640, 520), (600, 580)]);
    let vp = hg_vp(bars.len());
    let tick = hg().price.tick;
    let mut d = ChartDrawings::new(ProductDrawings::ephemeral(hg(), Vec::new()));
    hg_frame(&mut d, &bars, &vp);
    d.set_tool(mark());
    let axis = TimeAxis::new(BarsView::fixed(&bars, M));
    let at = |i: f64, p: i64| {
        let (x, y) = (vp.index_to_x(i), vp.price_to_y((p * tick) as f64));
        (x as f32, y as f32)
    };
    // Near bar 1's high (650), then bar 2's low (520).
    let (x1, y1) = at(1.0, 640);
    let a = d
        .snap(&axis, &vp, tick, false, x1.into(), y1.into())
        .unwrap();
    assert_eq!(a.price, Price(650 * tick));
    assert!(d.on_down(x1, y1, false, Some(a)));
    d.on_up(x1, y1, Some(a));
    let (x2, y2) = at(2.0, 530);
    let b = d
        .snap(&axis, &vp, tick, false, x2.into(), y2.into())
        .unwrap();
    assert_eq!(b.price, Price(520 * tick));
    assert!(d.on_down(x2, y2, false, Some(b)));
    d.on_up(x2, y2, Some(b));
    let items = d.data().lock().items().to_vec();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].a, items[0].b), (a, b));
    assert_eq!(d.tool(), Some(mark()), "stays chosen for the next one");
    assert_eq!(
        d.card_point(),
        1,
        "the card starts on the point just placed"
    );
    // Another tool snaps as before (the grid), on the same chart.
    d.set_tool(Tool::HLine);
    let g = d
        .snap(&axis, &vp, tick, false, x1.into(), y1.into())
        .unwrap();
    assert_eq!(g.price, Price(640 * tick));
}

/// A tool that reads prices learns whether the chart traded back through a price after a
/// point, and draws again when the chart's bars change (not every frame).
#[test]
fn a_tool_reading_prices_draws_again_when_the_bars_change() {
    let _s = mark_serial();
    let tick = hg().price.tick;
    let mut bars = hg_bars(&[(600, 550), (650, 560), (640, 520), (600, 580)]);
    let vp = hg_vp(8);
    let mut d = ChartDrawings::new(ProductDrawings::ephemeral(hg(), Vec::new()));
    // A move up from 550 (bar 0) to 650 (bar 1): its half is 600.
    let a = Anchor {
        time: bars[0].time,
        price: Price(550 * tick),
    };
    let b = Anchor {
        time: bars[1].time,
        price: Price(650 * tick),
    };
    d.place(mark(), a, b);
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        *MARK_CROSSED.lock().unwrap(),
        Some(true),
        "bar 2's low 520 < 600"
    );
    let n = MARK_DRAWS.load(Ordering::Relaxed);
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        MARK_DRAWS.load(Ordering::Relaxed),
        n,
        "same bars: not drawn again"
    );
    // A chart whose bars after B never come back to 600: not crossed.
    bars.truncate(2);
    bars.push(hg_bars(&[(0, 0), (0, 0), (700, 610)])[2]);
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        MARK_DRAWS.load(Ordering::Relaxed),
        n + 1,
        "bars changed: drawn again"
    );
    assert_eq!(*MARK_CROSSED.lock().unwrap(), Some(false));
}

/// A tool's kept drawings are placed when the product is first shown, never written to the
/// drawings file, and every later change of them is reported to it (the set a chart first
/// sees is its baseline, not a change).
#[test]
fn a_tool_restores_its_drawings_and_hears_of_changes() {
    let _s = mark_serial();
    let tick = hg().price.tick;
    let p = |m: i64, t: i64| Anchor {
        time: UtcMillis(1_800_000_000_000 + m * M),
        price: Price(t * tick),
    };
    RESTORE.lock().unwrap().push((p(0, 550), p(1, 650)));
    let mut all = crate::drawings_store::Drawings::new(None, None);
    let (data, _) = all.product(&hg());
    let items = data.lock().items().to_vec();
    assert_eq!(items.len(), 1);
    assert_eq!(
        (items[0].tool, items[0].a, items[0].b),
        (mark(), p(0, 550), p(1, 650))
    );
    assert!(
        DrawingsFile::from_drawings(&hg(), &items, &HashMap::new())
            .drawings
            .is_empty(),
        "the tool's to save, not the drawings file's"
    );
    let bars = hg_bars(&[(600, 550), (650, 560), (640, 520), (600, 580)]);
    let vp = hg_vp(bars.len());
    let mut d = ChartDrawings::new(data);
    hg_frame(&mut d, &bars, &vp);
    assert!(
        CHANGED.lock().unwrap().is_empty(),
        "the first set is the baseline"
    );
    d.place(mark(), p(2, 520), p(3, 600));
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        CHANGED.lock().unwrap().last().cloned(),
        Some(vec![(p(0, 550), p(1, 650)), (p(2, 520), p(3, 600))])
    );
    // Swapping the selected drawing's points (the card's other name) is a change too.
    assert!(d.swap_selected_points());
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        CHANGED.lock().unwrap().last().cloned(),
        Some(vec![(p(0, 550), p(1, 650)), (p(3, 600), p(2, 520))])
    );
    assert!(d.delete_selected());
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        CHANGED.lock().unwrap().last().cloned(),
        Some(vec![(p(0, 550), p(1, 650))])
    );
    let n = CHANGED.lock().unwrap().len();
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(
        CHANGED.lock().unwrap().len(),
        n,
        "nothing changed: nothing reported"
    );
}

/// The point card: the tool's lines for the point and its names; none for a tool that names
/// no points.
#[test]
fn the_point_card_describes_the_point() {
    let _s = mark_serial();
    let tick = hg().price.tick;
    let ctx = DrawToolContext {
        product: hg(),
        contract: None,
    };
    let a = Anchor {
        time: UtcMillis(0),
        price: Price(550 * tick),
    };
    let b = Anchor {
        time: UtcMillis(M),
        price: Price(650 * tick),
    };
    let none = &lines_feed::levels::NoPrices;
    let c = crate::tool_panel::card(&Mark, &ctx, (a, b), 1, none).unwrap();
    assert_eq!(c.names, ["A", "E"]);
    assert_eq!(c.point, 1);
    assert_eq!(c.lines, vec![format!("point 1 at {}", 650 * tick)]);
    assert!(crate::tool_panel::card(&Pair, &ctx, (a, b), 0, none).is_none());
}

/// More drawings than one map holds: those in view are merged first, so marking a new session
/// never loses its drawings to older ones; with none in view the first ones fit.
#[test]
fn over_the_limits_the_drawings_in_view_are_kept() {
    let _s = serial();
    let p = zn();
    let tick = p.price.tick;
    let ctx = DrawToolContext {
        product: p.clone(),
        contract: None,
    };
    let at = |t: i64, ticks: i64| Anchor {
        time: UtcMillis(t * M),
        price: Price(ticks * tick),
    };
    // Three levels each: 400 drawings don't fit MAX_LEVELS.
    let mut parts: Vec<Cached> = (0..400)
        .map(|i| {
            let (a, b) = (at(2 * i, 600), at(2 * i + 1, 610));
            Cached {
                id: i as DrawingId + 1,
                a,
                b,
                map: drawn(&Pair, &ctx, a, b, tick, &lines_feed::levels::NoPrices),
                merged: 0,
            }
        })
        .collect();
    let (_, dropped) = merge(&Pair, &ctx, &mut parts, tick, None, None);
    assert!(dropped);
    assert_eq!(parts[0].merged, 3);
    assert_eq!(parts[399].merged, 0, "the last left out");
    let view = Some((UtcMillis(798 * M), UtcMillis(799 * M)));
    let (m, dropped) = merge(&Pair, &ctx, &mut parts, tick, view, None);
    assert!(dropped);
    assert_eq!(parts[399].merged, 3, "in view: first in");
    assert!(m.unwrap().groups.contains(&"d400".to_owned()));
    assert_ne!(in_view_key(&parts, None), in_view_key(&parts, view));
}

/// A tool's overlay is drawn with no drawings at all, asked again only when its key moves,
/// and goes when the tool has none.
#[test]
fn a_tools_overlay_is_drawn_and_follows_its_key() {
    let _s = mark_serial();
    let bars = hg_bars(&[(600, 550), (650, 560)]);
    let vp = hg_vp(bars.len());
    let mut d = ChartDrawings::new(ProductDrawings::ephemeral(hg(), Vec::new()));
    hg_frame(&mut d, &bars, &vp);
    assert!(d.tool_maps().map(MARK_ID).is_none(), "nothing to draw");
    OVERLAY_AT.store(600, Ordering::Relaxed);
    hg_frame(&mut d, &bars, &vp);
    let m = d.tool_maps().map(MARK_ID).cloned().unwrap();
    assert_eq!(m.markers.len(), 1);
    assert!(m.markers[0].hollow);
    let rev = d.tool_maps().revision();
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(d.tool_maps().revision(), rev, "same key: not merged again");
    OVERLAY_AT.store(0, Ordering::Relaxed);
    hg_frame(&mut d, &bars, &vp);
    assert!(d.tool_maps().map(MARK_ID).is_none());
}

/// Swapping a drawing's points keeps the point card on the same physical point (flips
/// `card_point`), and starting a new placement clears the prior selection so the old card
/// doesn't linger while placing the next drawing.
#[test]
fn swap_flips_card_point_and_new_placement_clears_selection() {
    let _s = mark_serial();
    let bars = hg_bars(&[(600, 550), (650, 560), (640, 520), (600, 580)]);
    let vp = hg_vp(bars.len());
    let tick = hg().price.tick;
    let mut d = ChartDrawings::new(ProductDrawings::ephemeral(hg(), Vec::new()));
    hg_frame(&mut d, &bars, &vp);
    d.set_tool(mark());
    let a = Anchor {
        time: bars[0].time,
        price: Price(550 * tick),
    };
    let b = Anchor {
        time: bars[1].time,
        price: Price(650 * tick),
    };
    d.place(mark(), a, b);
    assert_eq!(d.card_point(), 1);
    assert!(d.swap_selected_points());
    assert_eq!(
        d.card_point(),
        0,
        "card stays on the same physical point, now role 0"
    );
    assert!(d.swap_selected_points());
    assert_eq!(d.card_point(), 1);
    // Starting the next placement with a stays-chosen tool drops the old selection.
    assert!(d.selected.is_some());
    assert!(d.on_down(100.0, 100.0, false, Some(a)));
    assert!(d.selected.is_none());
}

/// `update` (judging prices) runs before `report` (`changed`), so a tool that reads prices has
/// its verdict ready on the very first `changed` notification; `overlay_changed` reports an
/// async overlay update on an idle chart.
#[test]
fn update_precedes_report_and_overlay_changed_detects_async_updates() {
    let _s = mark_serial();
    let tick = hg().price.tick;
    let bars = hg_bars(&[(600, 550), (650, 560), (640, 520), (600, 580)]);
    let vp = hg_vp(bars.len());
    let mut d = ChartDrawings::new(ProductDrawings::ephemeral(hg(), Vec::new()));
    hg_frame(&mut d, &bars, &vp);
    assert!(!d.overlay_changed());
    // Place a move whose bar 2 crosses its midpoint: `draw_with` runs before `changed`.
    let a = Anchor {
        time: bars[0].time,
        price: Price(550 * tick),
    };
    let b = Anchor {
        time: bars[1].time,
        price: Price(650 * tick),
    };
    d.place(mark(), a, b);
    hg_frame(&mut d, &bars, &vp);
    assert_eq!(*MARK_CROSSED.lock().unwrap(), Some(true));
    assert_eq!(CHANGED.lock().unwrap().len(), 1);
    // Async overlay update moves `overlay_key` -> `overlay_changed` wakes an idle chart.
    OVERLAY_AT.store(610, Ordering::Relaxed);
    assert!(d.overlay_changed());
    hg_frame(&mut d, &bars, &vp);
    assert!(!d.overlay_changed());
}

/// Over the map limits, an earlier drawing whose ruler levels extend into the visible range
/// stays prioritized as in-view even when its two anchors sit before the visible window.
#[test]
fn over_the_limits_keeps_drawings_whose_levels_span_view() {
    let _s = serial();
    let p = zn();
    let tick = p.price.tick;
    let ctx = DrawToolContext {
        product: p.clone(),
        contract: None,
    };
    let at = |t: i64, ticks: i64| Anchor {
        time: UtcMillis(t * M),
        price: Price(ticks * tick),
    };
    let mut parts: Vec<Cached> = (0..400)
        .map(|i| {
            let (a, b) = (at(2 * i, 600), at(2 * i + 1, 610));
            let mut map = drawn(&Pair, &ctx, a, b, tick, &lines_feed::levels::NoPrices).unwrap();
            let to = if i == 399 {
                UtcMillis(2_100 * M)
            } else {
                UtcMillis((2 * i + 2) * M)
            };
            for l in &mut map.levels {
                l.to = Some(to);
            }
            Cached {
                id: i as DrawingId + 1,
                a,
                b,
                map: Some(map),
                merged: 0,
            }
        })
        .collect();
    // Viewing afternoon bars well after drawing 399's anchors: its bounded ruler (`to: 2100*M`)
    // spans the view while earlier sessions' rulers ended earlier, so drawing 399 is kept first.
    let view = Some((UtcMillis(2_000 * M), UtcMillis(2_100 * M)));
    assert_eq!(view_rank(&parts[0], view), 2);
    assert_eq!(view_rank(&parts[399], view), 1);
    let (_, dropped) = merge(&Pair, &ctx, &mut parts, tick, view, None);
    assert!(dropped);
    assert_eq!(parts[399].merged, 3, "spanning session ruler kept first");
}

/// `BarsPrices::covered` only claims coverage on trade-built bars (never coarse time bars),
/// and never lets a far-future market clock stretch coverage past an open session's last bar.
#[test]
fn bars_prices_covered_requires_trade_bars_and_respects_open_session() {
    use lines_core::ticks::{TickBars, TickReset};
    let bars = hg_bars(&[(600, 550), (650, 560), (640, 580)]);
    let axis = TimeAxis::new(BarsView::fixed(&bars, M));
    let coarse = BarsPrices(&axis, Some(UtcMillis(i64::MAX)));
    assert!(
        coarse.covered().is_none(),
        "time bars must not claim to settle sub-bar crosses"
    );
    let t0 = 1_800_000_000_000i64;
    let mut tb = TickBars::new(2, TickReset::Session, None, 64);
    tb.push(UtcMillis(t0), Price(600_000), Qty(1), None);
    tb.push(UtcMillis(t0 + 1_000), Price(600_100), Qty(1), None);
    tb.push(UtcMillis(t0 + 2_000), Price(600_050), Qty(1), None);
    tb.push(UtcMillis(t0 + 3_000), Price(600_075), Qty(1), None);
    let axis = TimeAxis::new(tb.view());
    // Forming/done bar with no session close: clock far ahead does not extend `hi`.
    let open = BarsPrices(&axis, Some(UtcMillis(t0 + 5 * 3_600_000)));
    assert_eq!(open.covered(), Some((UtcMillis(t0), UtcMillis(t0 + 2_000))));
}
