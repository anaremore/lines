use super::*;
use lines_core::{Aggressor, Capabilities, ContractKey, Month, Price, Qty};
use lines_feed::catalog;
use lines_feed::levels::demo::DemoLevels;
use lines_feed::provider::MarketEvent;

#[test]
fn config_defaults_toggles_and_round_trip() {
    let mut c = LevelsConfig::default();
    assert!(c.is_default());
    assert!(c.source_shown("demo") && c.kind_shown("demo", "zone"));
    assert_eq!(serde_json::to_string(&c).unwrap(), "{}");
    c.toggle_source("demo");
    c.toggle_kind("demo", "zone");
    c.toggle_kind("other", "pivot"); // a source missing from this build: kept as is
    c.hide_labels = true;
    assert!(!c.source_shown("demo"));
    assert!(!c.kind_shown("demo", "zone") && c.kind_shown("demo", "pivot"));
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(
        json,
        r#"{"hide-labels":true,"hidden-sources":["demo"],"hidden-kinds":["demo/zone","other/pivot"]}"#
    );
    assert_eq!(serde_json::from_str::<LevelsConfig>(&json).unwrap(), c);
    // Toggling again restores exactly.
    c.toggle_source("demo");
    c.toggle_kind("demo", "zone");
    c.toggle_kind("other", "pivot");
    c.hide_labels = false;
    assert!(c.is_default());
    // All levels hidden hides every source.
    let all = LevelsConfig {
        hidden: true,
        ..LevelsConfig::default()
    };
    assert!(!all.source_shown("demo"));
}

/// Tests on the demo source share one runtime task (same source, product and contract):
/// one test's fetch writing the shared cell makes the other's `try_read` miss (by design: a
/// snapshot keeps the previous state then). One at a time.
static DEMO: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn es_store() -> Arc<ChartStore> {
    let es = catalog::get(catalog::ES).unwrap();
    let z26 = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
    let store = Arc::new(ChartStore::new(
        es,
        Some(z26),
        Capabilities::default(),
        "test",
        60_000,
        1000,
    ));
    store.update(|d| {
        d.apply(&[MarketEvent::Trade {
            time: UtcMillis::parse_iso("2026-11-05T14:30:00").unwrap(),
            price: Price(600_100),
            qty: Qty(1),
            aggressor: Some(Aggressor::Buy),
        }])
    });
    store
}

fn vp() -> Viewport {
    let mut v = Viewport::default();
    v.set_size(800.0, 400.0);
    (v.price_lo, v.price_hi) = (598_000.0, 604_000.0);
    v
}

/// Regression (adapters thread): the legend was built at render time from the previous
/// frame's source status while the levels drew from the current one, so a single-frame
/// screenshot said "missing" over drawn levels. Both now read one per-frame snapshot.
#[test]
fn legend_and_levels_describe_the_same_frame() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let i = layer
        .sources()
        .iter()
        .position(|(id, ..)| id == "demo")
        .expect("demo serves ES");
    // The very first frame, as a screenshot draws it: render (snapshot, legend), then paint.
    let d = store.read();
    layer.snapshot(d.market_time);
    let rows = layer.legend();
    layer.prepare(
        &d,
        BarsView::fixed(d.series.bars(), d.series.period_ms()),
        &vp(),
    );
    assert_eq!(
        rows[i].text, "as of 13:00 UTC",
        "the legend sees the map drawn"
    );
    assert!(!rows[i].warn);
    assert!(layer.placed_per_source()[i] > 0);
    // The draw path never re-reads the sources: what the snapshot says is what is drawn,
    // even if the source has moved on since (here: a snapshot saying missing).
    layer.force_status(i, LevelStatus::Missing);
    layer.prepare(
        &d,
        BarsView::fixed(d.series.bars(), d.series.period_ms()),
        &vp(),
    );
    assert_eq!(layer.legend()[i].text, "missing");
    assert_eq!(layer.placed_per_source()[i], 0, "missing: nothing drawn");
    // The next frame's snapshot brings both back together.
    layer.snapshot(d.market_time);
    layer.prepare(
        &d,
        BarsView::fixed(d.series.bars(), d.series.period_ms()),
        &vp(),
    );
    assert_eq!(layer.legend()[i].text, "as of 13:00 UTC");
    assert!(layer.placed_per_source()[i] > 0);
}

/// The demo map on an ES chart: placement, per-kind and per-source visibility, labels off
/// the last-price row, legend freshness, stale and missing.
#[test]
fn layer_places_filters_and_reports_freshness() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let i = layer
        .sources()
        .iter()
        .position(|(id, ..)| id == "demo")
        .expect("demo serves ES");
    // One frame: snapshot (render), then prepare (paint).
    let prepare = |layer: &mut LevelsLayer, _: Option<f32>| {
        let d = store.read();
        layer.snapshot(d.market_time);
        let bars = BarsView::fixed(d.series.bars(), d.series.period_ms());
        layer.prepare(&d, bars, &vp());
    };
    prepare(&mut layer, None);
    // 3 lines + 2 zones + the anchored line (the chart has the 14:30 bar): all on screen.
    // The VWAP band waits for a VWAP (this store has no indicator feed): withheld.
    assert_eq!(layer.placed_per_source()[i], 6);
    let (_, _, kinds) = &layer.sources()[i];
    assert_eq!(
        kinds,
        &["pivot", "resist", "support", "zone", "band", "anchor"]
    );
    let rows = layer.legend();
    assert_eq!(rows[i].text, "as of 13:00 UTC");
    assert!(!rows[i].warn);

    // Kinds and sources hide; labels stay on their levels.
    layer.config.toggle_kind("demo", "zone");
    prepare(&mut layer, None);
    assert_eq!(layer.placed_per_source()[i], 4);
    layer.config.toggle_kind("demo", "zone");
    // The pivot (6010.00) sits at (604000 - 601000) / 15 = 200 px; its label stays there
    // (a last price on that row draws over it, never moves it).
    prepare(&mut layer, None);
    let pivot = layer.placed(i).iter().find(|p| p.index == 0).unwrap();
    assert_eq!(pivot.top, 200.0);
    assert_eq!(pivot.label_y, Some(200.0));
    layer.config.toggle_source("demo");
    prepare(&mut layer, None);
    assert_eq!(layer.placed_per_source()[i], 0);
    assert!(layer.legend().iter().all(|r| r.label != "Demo levels"));
    layer.config.toggle_source("demo");

    // Stale: dimmed and labelled; missing: nothing drawn, "missing".
    prepare(&mut layer, None);
    layer.force_status(i, LevelStatus::Stale { age_ms: 300_000 });
    let rows = layer.legend();
    assert_eq!(
        (rows[i].text.as_str(), rows[i].warn),
        ("stale · 5 min", true)
    );
    layer.force_status(i, LevelStatus::Missing);
    assert_eq!(layer.legend()[i].text, "missing");
}

#[test]
fn legend_reason_does_not_repeat_the_source_name() {
    use super::without_name;
    // A source's report read "Acme missing · Acme: not translated: …".
    assert_eq!(
        without_name("Acme", "Acme: not translated: unknown symbol"),
        "not translated: unknown symbol"
    );
    assert_eq!(
        without_name("Acme", "acme : timeout"),
        "timeout",
        "any case, spaces before the colon"
    );
    // Other errors, or the name not followed by a colon, are kept whole.
    assert_eq!(without_name("Acme", "timeout"), "timeout");
    assert_eq!(
        without_name("Acme", "Acmes unavailable"),
        "Acmes unavailable"
    );
    assert_eq!(
        without_name("Lévels", "Lé"),
        "Lé",
        "no panic on short or non-ASCII"
    );
}

/// A source whose products are learned at run time (from its service): it serves nothing
/// until told otherwise.
struct LateServing;

static LATE_SERVES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct NoFrames;

impl lines_feed::levels::LevelTransport for NoFrames {
    fn fetch(
        &mut self,
    ) -> lines_feed::levels::BoxFuture<'_, anyhow::Result<Vec<lines_feed::levels::LevelFrame>>>
    {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl lines_feed::levels::LevelSource for LateServing {
    fn id(&self) -> &'static str {
        "late-serving"
    }
    fn label(&self) -> &'static str {
        "Late"
    }
    fn serves(&self, _: &Product) -> bool {
        LATE_SERVES.load(std::sync::atomic::Ordering::Acquire)
    }
    fn refresh(&self) -> lines_feed::levels::RefreshMode {
        lines_feed::levels::RefreshMode::OneShot
    }
    fn stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
    fn connect(
        &self,
        _: &LevelContext,
    ) -> lines_feed::levels::BoxFuture<
        'static,
        anyhow::Result<Box<dyn lines_feed::levels::LevelTransport>>,
    > {
        Box::pin(async { Ok(Box::new(NoFrames) as Box<dyn lines_feed::levels::LevelTransport>) })
    }
    fn decode(
        &self,
        _: &LevelContext,
        _: &dyn lines_feed::levels::ChartPrices,
        _: Option<&LevelMap>,
        _: &[lines_feed::levels::LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        Ok(None)
    }
}

/// Puts `LateServing` back to serving nothing (and tells charts) however the test ends, so a
/// failure can't leave it on other tests' charts. (Sources can't be unregistered; one that
/// serves nothing has no slot anywhere.)
struct LateGuard;

impl Drop for LateGuard {
    fn drop(&mut self) {
        LATE_SERVES.store(false, std::sync::atomic::Ordering::Release);
        lines_feed::levels::serving_changed();
    }
}

/// A source that doesn't serve the chart's product has no slot: nothing drawn, no legend or
/// menu row. When its answer changes (`levels::serving_changed`), the next snapshot joins it,
/// and drops it again when it stops serving, without a feed change. A source still serving
/// keeps its slot through it: the same map, never "missing" for a frame.
#[test]
fn sources_join_and_leave_when_their_serving_changes() {
    use std::sync::atomic::Ordering;
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _reset = LateGuard;
    LATE_SERVES.store(false, Ordering::Release);
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    lines_feed::levels::register(Arc::new(LateServing)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let has_late = |layer: &LevelsLayer| {
        let menu = layer.sources().iter().any(|(id, ..)| id == "late-serving");
        let legend = layer.legend().iter().any(|r| r.label == "Late");
        assert_eq!(menu, legend, "menu and legend agree");
        menu
    };
    let demo = |layer: &LevelsLayer| {
        let i = layer
            .sources()
            .iter()
            .position(|(id, ..)| id == "demo")
            .expect("demo serves ES");
        let map = layer.map(i).cloned().expect("demo map");
        assert_ne!(layer.legend()[i].text, "missing");
        map
    };
    layer.snapshot(None);
    assert!(!has_late(&layer), "not served: no row at all");
    let demo_map = demo(&layer);

    let before = layer.revision();
    LATE_SERVES.store(true, Ordering::Release);
    lines_feed::levels::serving_changed();
    assert_ne!(layer.revision(), before, "the chart repaints");
    layer.snapshot(None);
    assert!(has_late(&layer), "joined without a feed change");
    assert!(Arc::ptr_eq(&demo(&layer), &demo_map), "demo kept its slot");

    LATE_SERVES.store(false, Ordering::Release);
    lines_feed::levels::serving_changed();
    layer.snapshot(None);
    assert!(!has_late(&layer), "left again");
    assert!(Arc::ptr_eq(&demo(&layer), &demo_map), "demo kept its slot");
}

/// Re-attaching to the same market (a link group, a `set_feed` to the same store) joins the
/// running tasks: the same map, never "missing", no refetch.
#[test]
fn reattaching_to_the_same_market_keeps_the_map() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let demo = |layer: &LevelsLayer| {
        let i = layer
            .sources()
            .iter()
            .position(|(id, ..)| id == "demo")
            .expect("demo serves ES");
        let map = layer.map(i).cloned();
        let text = layer.legend()[i].text.clone();
        (map, text)
    };
    layer.snapshot(None);
    let (map, text) = demo(&layer);
    let map = map.expect("demo map");
    assert_ne!(text, "missing");
    layer.attach(&store);
    // Before any new snapshot the slot is the old one, map included.
    let (again, text) = demo(&layer);
    assert!(again.is_some_and(|m| Arc::ptr_eq(&m, &map)));
    assert_ne!(text, "missing");
    layer.snapshot(None);
    let (again, text) = demo(&layer);
    assert!(again.is_some_and(|m| Arc::ptr_eq(&m, &map)), "same task");
    assert_ne!(text, "missing");
}

/// An option chart has no level sources (decision 0028 puts options on the underlying's
/// product: its levels are in another market's prices): no slot, no legend row, no error.
/// The underlying's own chart still gets them.
#[test]
fn option_charts_get_no_level_sources() {
    use lines_core::{CivilDate, Right};
    use std::sync::atomic::Ordering;
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _reset = LateGuard;
    LATE_SERVES.store(true, Ordering::Release); // serves every product
    lines_feed::levels::register(Arc::new(LateServing)).unwrap();
    let spy = catalog::get(catalog::SPY).unwrap();
    let store_for = |contract: ContractKey| {
        Arc::new(ChartStore::new(
            spy.clone(),
            Some(contract),
            Capabilities::default(),
            "test",
            60_000,
            1000,
        ))
    };
    let option = ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 12, 18).unwrap(),
        Price(60_000),
        Right::Call,
    );
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store_for(option));
    layer.snapshot(None);
    assert!(layer.sources().is_empty(), "no slot on an option chart");
    assert!(layer.legend().is_empty(), "no legend row");
    // Even after a serving change.
    lines_feed::levels::serving_changed();
    layer.snapshot(None);
    assert!(layer.sources().is_empty());
    // The shares chart does.
    layer.attach(&store_for(ContractKey::equity(catalog::SPY)));
    layer.snapshot(None);
    assert!(layer.sources().iter().any(|(id, ..)| id == "late-serving"));
}

/// Per-type styles on the chart (decision 0026): each kind the source declares resolves to
/// its type's look (the demo's: its levels' own colors), a type the chart hides isn't
/// placed, a restyle re-resolves without a new map, and the label mode switches the texts to
/// `name : price` (in the product's format) and back.
#[test]
fn types_resolve_hide_and_label_with_the_price() {
    use lines_core::levels::LabelMode;
    use lines_core::style::Color;
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let i = layer
        .sources()
        .iter()
        .position(|(id, ..)| id == "demo")
        .expect("demo serves ES");
    let frame = |layer: &mut LevelsLayer| {
        let d = store.read();
        layer.snapshot(d.market_time);
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp(),
        );
    };
    frame(&mut layer);
    let pivot = layer.kind_look(i, "pivot").expect("declared");
    assert_eq!((pivot.line.color, pivot.line.width), (Color::Own, 2));
    assert_eq!(
        layer.kind_look(i, "zone").unwrap().fill.color,
        Color::Own,
        "the demo's zones keep their own colors"
    );
    // The demo's own mode is the name; its labels say so.
    assert!(
        layer
            .label_texts(i)
            .iter()
            .flatten()
            .any(|t| t == "Pivot 6010")
    );
    let placed = layer.placed_per_source()[i];
    // Hide the pivot type and ask for prices in the labels.
    let (types, default) = layer.source_types("demo").unwrap();
    assert_eq!(default, LabelMode::Name);
    let mut set = study_style::StudyStyles::default();
    let mut looks = Look::defaults(&study_style::LEVELS).to_vec();
    for t in types {
        let mut l = study_style::read_type(&PartStyles::new(), t);
        l.shown = t.id != "pivot";
        looks.push(l);
    }
    set.set_level_style(
        "demo",
        &study_style::LevelEdit {
            looks,
            types,
            label: LabelMode::NamePrice,
            label_default: default,
        },
    );
    layer.set_styles(&set.levels);
    frame(&mut layer);
    assert_eq!(layer.placed_per_source()[i], placed - 1, "the pivot hidden");
    assert!(!layer.kind_look(i, "pivot").unwrap().shown);
    let texts: Vec<&str> = layer
        .label_texts(i)
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    assert!(texts.contains(&"Pivot 6010 : 6010.00"), "{texts:?}");
    assert!(
        texts.contains(&"Supply : 6022.50"),
        "a zone: its upper edge"
    );
    assert!(
        texts.contains(&"VWAP ±7.50"),
        "VWAP-relative: the name only"
    );
    // Back to the defaults: nothing saved, the name only again.
    layer.set_styles(&Default::default());
    frame(&mut layer);
    assert_eq!(layer.placed_per_source()[i], placed);
    assert!(
        layer
            .label_texts(i)
            .iter()
            .flatten()
            .all(|t| !t.contains(" : "))
    );
}

/// Labels stay on their levels whatever the price does: trades walk the last price across
/// every labeled demo level (levels and viewport fixed), and every label's y is the same in
/// every frame (it used to step off the last price's row, so labels near it bounced).
#[test]
fn a_last_price_sweeping_the_levels_never_moves_a_label() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let i = layer
        .sources()
        .iter()
        .position(|(id, ..)| id == "demo")
        .expect("demo serves ES");
    let frame = |layer: &mut LevelsLayer| {
        let d = store.read();
        layer.snapshot(d.market_time);
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp(),
        );
        layer
            .placed(i)
            .iter()
            .map(|p| (p.index, p.label_y))
            .collect::<Vec<_>>()
    };
    let first = frame(&mut layer);
    assert!(first.iter().filter(|(_, y)| y.is_some()).count() >= 3);
    let t0 = UtcMillis::parse_iso("2026-11-05T14:30:01").unwrap();
    // 5990.00 to 6030.00 a tick at a time: through the support, the pivot, the zones and
    // the resistance.
    for (k, p) in (599_000..=603_000).step_by(25).enumerate() {
        store.update(|d| {
            d.apply(&[MarketEvent::Trade {
                time: UtcMillis(t0.0 + k as i64),
                price: Price(p),
                qty: Qty(1),
                aggressor: Some(Aggressor::Buy),
            }])
        });
        assert_eq!(frame(&mut layer), first, "last price {p}");
    }
}

/// A kind a source split from another (`LevelType::parent`): a chart that hid the parent
/// keeps all its children hidden; showing one child from the menu shows only it (its
/// siblings, and the parent while it is still a kind, stay hidden).
#[test]
fn a_hidden_parent_kind_hides_its_children() {
    let mut c: LevelsConfig =
        serde_json::from_str(r#"{"hidden-kinds": ["src/session", "src/band"]}"#).unwrap();
    for child in ["open", "close", "mid"] {
        assert!(
            !c.kind_shown_under("src", child, Some("session")),
            "{child}"
        );
    }
    assert!(!c.kind_shown_under("src", "upper-band", Some("band")));
    assert!(c.kind_shown_under("src", "pivot", None));
    assert!(c.kind_shown_under("other", "open", Some("session")));
    // Show "mid" alone: the parent's entry is replaced by its other children.
    let others: Vec<String> = ["open", "close", "mid"].map(String::from).to_vec();
    c.show_under("src", "mid", "session", &others);
    assert!(c.kind_shown_under("src", "mid", Some("session")));
    assert!(!c.kind_shown_under("src", "open", Some("session")));
    assert!(!c.kind_shown_under("src", "close", Some("session")));
    assert_eq!(
        c.hidden_kinds,
        ["src/band", "src/close", "src/open"].map(String::from)
    );
}
