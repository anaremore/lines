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
        (rows[i].text.as_str(), rows[i].title.as_deref()),
        ("", Some("Synthetic")),
        "the legend sees the map drawn (no time while current)"
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
    assert_eq!(layer.legend()[i].text, "");
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
        &[
            "pivot", "resist", "support", "supply", "zone", "band", "anchor"
        ]
    );
    // Current: the source's title and stats, never the map's time.
    let rows = layer.legend();
    assert_eq!(rows[i].text, "");
    assert!(!rows[i].warn && !rows[i].dim);
    assert_eq!(rows[i].title.as_deref(), Some("Synthetic"));
    assert_eq!(
        rows[i]
            .stats
            .iter()
            .map(|s| (s.name.as_str(), s.text.as_str()))
            .collect::<Vec<_>>(),
        [
            ("Range", "12.50"),
            ("Bias", "+1.5"),
            ("Skew", "\u{2212}0.4")
        ]
    );

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
        (rows[i].text.as_str(), rows[i].warn, rows[i].dim),
        ("· stale", false, true),
        "stale: said in words, dimmed, never the map's time"
    );
    assert!(!rows[i].stats.is_empty(), "its stats, dimmed with it");
    layer.force_status(i, LevelStatus::Missing);
    let rows = layer.legend();
    assert_eq!(rows[i].text, "missing");
    assert!(rows[i].stats.is_empty() && rows[i].title.is_none());
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
    // The demo's lines recommend a color per theme (a theme token here); its zones keep
    // their own colors.
    assert!(pivot.line.color != Color::Own && pivot.line.width == 1);
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
    study_style::set_level_style(
        &mut set.levels,
        "demo",
        &study_style::LevelEdit {
            looks,
            types,
            labels: Vec::new(),
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
        texts.contains(&"Supply : 6022.50") && texts.contains(&"Demand : 5990.00"),
        "a zone: the edge its label sits at (supply's high, demand's low)"
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
/// siblings and the parent stay hidden).
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
    // Show "mid" alone: the parent and its other children stay hidden.
    c.show_under("src", "mid");
    assert!(c.kind_shown_under("src", "mid", Some("session")));
    assert!(!c.kind_shown_under("src", "open", Some("session")));
    assert!(!c.kind_shown_under("src", "close", Some("session")));
    assert!(!c.kind_shown("src", "session"), "the parent stays hidden");
    // A parent that is still a kind of its own (listed in the menu): showing a child keeps the
    // parent hidden and the child shown, on every frame (it isn't re-hidden).
    c.show_under("src", "upper-band");
    assert!(c.kind_shown_under("src", "upper-band", Some("band")));
    assert!(!c.kind_shown("src", "band"));
    assert!(c.kind_shown_under("src", "upper-band", Some("band")));
    // Hiding the child again ends its own showing.
    c.toggle_kind("src", "upper-band");
    assert!(!c.kind_shown_under("src", "upper-band", Some("band")));
    c.toggle_kind("src", "upper-band");
    assert!(
        !c.kind_shown_under("src", "upper-band", Some("band")),
        "the parent hides it again"
    );
}

/// The labels' need (what the price axis grows to, what a column fits to) is the widest label
/// of the whole level set (not only those in view); nothing while no label would show (labels
/// off for the chart or the source, or the source hidden). The view doesn't change it.
#[test]
fn the_label_need_follows_the_level_set_not_the_view() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let frame = |layer: &mut LevelsLayer, v: Viewport| {
        let d = store.read();
        layer.snapshot(d.market_time);
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &v,
        );
    };
    frame(&mut layer, vp());
    let (need, cut) = layer.label_need();
    assert!(need > 0.0 && need < 200.0, "{need}");
    assert!(cut > 0.0 && cut <= need, "{cut} {need}");
    // A view with no level in it: the same need.
    let mut far = vp();
    (far.price_lo, far.price_hi) = (500_000.0, 501_000.0);
    frame(&mut layer, far);
    assert_eq!(layer.label_need(), (need, cut));
    layer.config.hide_labels = true;
    assert_eq!(layer.label_need(), (0.0, 0.0), "labels off");
    layer.config.hide_labels = false;
    layer.config.toggle_labels("demo");
    assert_eq!(layer.label_need(), (0.0, 0.0), "this source's labels off");
    layer.config.toggle_labels("demo");
    layer.config.toggle_source("demo");
    assert_eq!(layer.label_need(), (0.0, 0.0), "source hidden");
}

/// No room (not even the shortest cut fits the axis or the column): no labels at all, never a
/// label running past its room. Room: labels, tags in the axis on their own edge's price.
#[test]
fn no_room_no_labels() {
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
    let mono = |t: &str| t.chars().count() as f32 * 6.0;
    let frame = |layer: &mut LevelsLayer, room: Option<f32>| {
        let d = store.read();
        layer.snapshot(d.market_time);
        layer.measure_labels(mono);
        layer.set_room(room);
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp(),
        );
    };
    frame(&mut layer, Some(400.0));
    assert!(layer.placed(i).iter().any(|p| p.label_y.is_some()));
    assert!(
        layer.placed(i).iter().all(|p| p.on_edge),
        "tags in the axis mark their edge's price"
    );
    frame(&mut layer, None);
    assert!(
        layer.placed(i).iter().all(|p| p.label_y.is_none()),
        "no labels"
    );
    // On the chart: beside their levels, as before.
    layer.config.label_place = LabelPlace::Chart;
    layer.set_room_plot();
    frame(&mut layer, Some(400.0));
    assert!(layer.placed(i).iter().all(|p| !p.on_edge));
}

/// Tags in the price axis never cover the last-price tag: one on it stacks off it (or goes).
#[test]
fn axis_tags_keep_off_the_last_price_tag() {
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
    let d = store.read();
    layer.snapshot(d.market_time);
    layer.measure_labels(|t| t.chars().count() as f32 * 6.0);
    layer.set_room(Some(400.0));
    let bars = BarsView::fixed(d.series.bars(), d.series.period_ms());
    layer.prepare(&d, bars, &vp());
    // The last price where the first tag is.
    let at = layer
        .placed(i)
        .iter()
        .find_map(|p| p.label_y)
        .expect("a tag");
    let tag_h = 18.0;
    layer.set_blocked(Some((at, tag_h)));
    layer.prepare(&d, bars, &vp());
    let h = LevelsLayer::label_h();
    for y in layer.placed(i).iter().filter_map(|p| p.label_y) {
        assert!((y - at).abs() >= (h + tag_h) * 0.5 - 1e-3, "{y} on {at}");
    }
}

/// A new batch of the same levels (a new `Arc`, as each refresh is) measures nothing again and
/// keeps the need exactly (the axis doesn't move), frame after frame.
#[test]
fn an_identical_batch_keeps_the_need() {
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
    {
        let d = store.read();
        layer.snapshot(d.market_time);
    }
    let calls = std::cell::Cell::new(0);
    let mono = |t: &str| {
        calls.set(calls.get() + 1);
        t.chars().count() as f32 * 6.0
    };
    layer.measure_labels(mono);
    assert!(calls.get() > 0);
    let first = layer.label_need();
    for _ in 0..3 {
        layer.renew_map(i);
        // Before this frame's measuring and after it: the same need, nothing measured.
        assert_eq!(layer.label_need(), first);
        calls.set(0);
        layer.measure_labels(mono);
        assert_eq!(calls.get(), 0, "an identical batch measures nothing");
        assert_eq!(layer.label_need(), first);
    }
}

/// The widest label fits its own tag whole, for any width: the axis grows to it (never below
/// its own width nor past the cap); past the cap, a label is cut to the room left; with no
/// labels, the axis keeps its width and there is no room.
#[test]
fn the_axis_grows_to_its_widest_tag() {
    let (base, cap) = (78.0, 168.0);
    let mut w = 1.0f32;
    while w < 300.0 {
        let (axis, room) = axis_fit(w, w.min(30.0), base, cap);
        assert!((base..=cap).contains(&axis), "{w}: {axis}");
        let room = room.expect("a short cut fits");
        if axis < cap {
            assert!(w <= room, "{w} px in a {axis} px axis: room {room}");
        } else {
            assert!(room < w + 1.0, "{w}: cut to {room}");
        }
        w += 0.75;
    }
    assert_eq!(axis_fit(0.0, 0.0, base, cap), (base, None), "no labels");
    // Not even the shortest cut fits the widest axis: no tags, and no wider axis.
    assert_eq!(axis_fit(400.0, 300.0, base, cap), (base, None));
}

/// On the chart the labels sit just right of the newest bar while their widest fits there,
/// else they all pin to the plot's edge; the column never takes more than 40 % of the chart
/// beside the axis; its edge follows the pointer (dragged left, it widens), at any zoom.
#[test]
fn chart_x_column_width_and_drag() {
    // Newest bar's right edge at 600 in an 800 px plot: a 120 px label fits beside it.
    assert_eq!(chart_label_x(600.0, 120.0, 800.0), Some(606.0));
    // Near the edge (the live view's small margin): pinned.
    assert_eq!(chart_label_x(760.0, 120.0, 800.0), None);
    // Exactly to the edge's margin: still beside.
    assert_eq!(chart_label_x(668.0, 120.0, 800.0), Some(674.0));
    assert_eq!(column_px(120.0, 1000.0), 120.0);
    assert_eq!(column_px(180.0, 300.0), 120.0, "40 % of the chart");
    assert_eq!(column_px(120.0, -5.0), 0.0);
    // A fit at 150 % (a 100 % width times 1.5) is never rounded below the label it fits.
    assert_eq!(column_px(80.87 * 1.5, 1000.0), 122.0);
    assert_eq!(
        dragged_column(120.0, 500.0, 470.0, 1.0),
        150.0,
        "left: wider"
    );
    assert_eq!(
        dragged_column(120.0, 500.0, 530.0, 1.0),
        90.0,
        "right: narrower"
    );
    // At 150 % a 30 px drag is 20 px of the saved (100 %) width.
    assert_eq!(dragged_column(120.0, 500.0, 470.0, 1.5), 140.0);
}

/// The placement saves only when it isn't the default (tags in the axis), reads a value from a
/// newer build as the default (never a corrupt workspace), and the column's width stays in its
/// limits.
#[test]
fn the_label_placement_is_optional_and_lenient() {
    let d = LevelsConfig::default();
    assert_eq!(d.label_place, LabelPlace::Axis);
    assert_eq!(serde_json::to_string(&d).unwrap(), "{}");
    let c = LevelsConfig {
        label_place: LabelPlace::Column,
        label_column: Some(140),
        ..Default::default()
    };
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(json, r#"{"label-place":"column","label-column":140}"#);
    assert_eq!(serde_json::from_str::<LevelsConfig>(&json).unwrap(), c);
    for odd in [r#""margin""#, "null", "3", "{}"] {
        let n: LevelsConfig =
            serde_json::from_str(&format!(r#"{{"label-place": {odd}, "hide-labels": true}}"#))
                .unwrap();
        assert_eq!(n.label_place, LabelPlace::Axis, "{odd}");
        assert!(n.hide_labels);
    }
    assert_eq!(d.column_w(), f32::from(COLUMN_W_DEFAULT));
    let wide = LevelsConfig {
        label_column: Some(9000),
        ..Default::default()
    };
    assert_eq!(wide.column_w(), f32::from(COLUMN_W_MAX));
}

/// At the boundary: a label that fits whole shows; one that doesn't fit whole shows cut (its
/// cut for this room is not empty), or until it was cut; one that doesn't fit even cut (an
/// empty cut for this room) doesn't; a cut made for another room doesn't decide.
#[test]
fn whether_a_label_can_show_at_the_boundary() {
    let room = 64.0;
    assert!(label_can_show(64.0, None, room), "exactly fits");
    assert!(
        label_can_show(70.0, None, room),
        "not cut yet: the paint decides"
    );
    let cut = (room, "Mid… : 7517.25".to_owned());
    assert!(label_can_show(90.0, Some(&cut), room), "fits cut");
    let none = (room, String::new());
    assert!(
        !label_can_show(90.0, Some(&none), room),
        "doesn't fit even cut"
    );
    let other = (72.0, String::new());
    assert!(
        label_can_show(90.0, Some(&other), room),
        "a cut for another room"
    );
    // On the chart: the plot's width less the chip and margins, in whole 8 px steps.
    let r = plot_room(803.0);
    assert_eq!(r % 8.0, 0.0);
    assert!(r <= 803.0 - LABEL_CHROME - GUTTER_PAD);
}

/// One app-wide style per source: the same styles given to an ES chart and an MES chart (the
/// app's `level_styles`, after an edit on either) restyle both, so a second chart and another
/// product look the same.
#[test]
fn one_style_restyles_every_chart_of_the_source() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let mes = {
        let p = catalog::get(catalog::MES).unwrap();
        let c = ContractKey::monthly(p.id, 2026, Month::new(12).unwrap());
        let store = Arc::new(ChartStore::new(
            p,
            Some(c),
            Capabilities::default(),
            "test-mes",
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
    };
    let stores = [es_store(), es_store(), mes];
    let mut layers: Vec<LevelsLayer> = stores
        .iter()
        .map(|s| {
            let mut l = LevelsLayer::new(LevelsConfig::default());
            l.attach(s);
            l
        })
        .collect();
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let frame = |l: &mut LevelsLayer, s: &Arc<ChartStore>| {
        let d = s.read();
        l.snapshot(d.market_time);
        l.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp(),
        );
    };
    // The edit (made on the first chart): the pivot 3 px wide.
    let (types, label) = layers[0].source_types("demo").unwrap();
    let mut looks = Look::defaults(&study_style::LEVELS).to_vec();
    for t in types {
        let mut l = study_style::read_type(&PartStyles::new(), t);
        if t.id == "pivot"
            && let crate::style_editor::PartStyle::Line(ls) = &mut l.style
        {
            ls.width = 3;
        }
        looks.push(l);
    }
    let mut app = BTreeMap::new();
    study_style::set_level_style(
        &mut app,
        "demo",
        &study_style::LevelEdit {
            looks,
            types,
            labels: Vec::new(),
            label,
            label_default: label,
        },
    );
    for (l, s) in layers.iter_mut().zip(&stores) {
        l.set_styles(&app);
        frame(l, s);
        let i = l
            .sources()
            .iter()
            .position(|(id, ..)| id == "demo")
            .expect("demo serves ES and MES");
        assert_eq!(l.kind_look(i, "pivot").unwrap().line.width, 3);
    }
}

/// A demo layer attached to an ES store, its first snapshot taken; the demo's index.
fn demo_layer() -> (LevelsLayer, Arc<ChartStore>, usize) {
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    {
        let d = store.read();
        layer.snapshot(d.market_time);
    }
    let i = layer
        .sources()
        .iter()
        .position(|(id, ..)| id == "demo")
        .expect("demo serves ES");
    (layer, store, i)
}

/// The text caches drop past their bound only texts no current map uses: the current map's
/// stay, so nothing is measured again and the gutter doesn't move.
#[test]
fn eviction_keeps_the_current_texts() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let (mut layer, _store, _) = demo_layer();
    let mono = |t: &str| t.chars().count() as f32 * 6.0;
    layer.measure_labels(mono);
    let need = layer.label_need();
    layer.stuff_caches(3 * lines_core::levels::MAX_LEVELS);
    let calls = std::cell::Cell::new(0);
    layer.measure_labels(|t| {
        calls.set(calls.get() + 1);
        mono(t)
    });
    assert_eq!(calls.get(), 0, "nothing measured again");
    assert!(
        layer.cached_widths() < lines_core::levels::MAX_LEVELS,
        "the unused went"
    );
    assert_eq!(layer.label_need(), need);
}

/// The shortest cut follows the shown filter: hiding the type whose label has a long price
/// lowers it.
#[test]
fn a_hidden_types_long_price_doesnt_set_the_shortest_cut() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let (mut layer, _store, _) = demo_layer();
    let styles: BTreeMap<String, PartStyles> =
        serde_json::from_str(r#"{"demo": {"label": "name-price"}}"#).unwrap();
    layer.set_styles(&styles);
    // The resistance's price measures long.
    let measure = |t: &str| {
        if t.contains("6025") {
            300.0
        } else {
            t.chars().count() as f32 * 6.0
        }
    };
    layer.measure_labels(measure);
    let (_, cut_all) = layer.label_need();
    let hidden: BTreeMap<String, PartStyles> = serde_json::from_str(
        r#"{"demo": {"label": "name-price", "types": {"resist": {"shown": false}}}}"#,
    )
    .unwrap();
    layer.set_styles(&hidden);
    layer.measure_labels(measure);
    let (_, cut_shown) = layer.label_need();
    assert!(cut_shown < cut_all, "{cut_shown} < {cut_all}");
}

/// Cuts are kept by text and room: a new identical batch keeps them (no measuring), a new room
/// cuts again.
#[test]
fn cuts_survive_an_identical_batch_and_follow_the_room() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let (mut layer, _store, i) = demo_layer();
    let calls = std::cell::Cell::new(0);
    let mono = |t: &str| {
        calls.set(calls.get() + 1);
        t.chars().count() as f32 * 6.0
    };
    // The pivot ("Pivot 6010") in a 40 px room: cut.
    let cut = layer.cut_for(i, 0, 40.0, mono).expect("a cut fits");
    assert!(cut.ends_with('…'), "{cut}");
    assert!(calls.get() > 0);
    layer.renew_map(i);
    calls.set(0);
    assert_eq!(
        layer.cut_for(i, 0, 40.0, mono).as_deref(),
        Some(cut.as_str())
    );
    assert_eq!(calls.get(), 0, "kept across the batch");
    let wider = layer.cut_for(i, 0, 48.0, mono).expect("fits");
    assert!(calls.get() > 0, "a new room cuts again");
    assert!(wider.chars().count() > cut.chars().count());
}

/// A one-letter name can't be cut: its shortest text is the full label, as `cut_label` can
/// cut it to (nothing shorter), so the gutter's fit and the paint agree.
#[test]
fn a_one_letter_name_is_its_own_shortest() {
    assert_eq!(super::shortest_cut("A", "A : 6010.00"), None);
    assert_eq!(
        lines_core::levels::cut_label("A", Some("6010.00"), 1000.0, |t| t.len() as f32),
        None
    );
    assert_eq!(
        super::shortest_cut("AB", "AB : 6010.00").as_deref(),
        Some("A… : 6010.00")
    );
    assert_eq!(super::shortest_cut("Mid", "Mid").as_deref(), Some("M…"));
}

/// A series-only push (its levels are the previous map's) keeps each label's hysteresis; a new
/// map with levels, or a series push whose level count differs, starts them over.
#[test]
fn a_series_only_push_keeps_label_state() {
    use lines_core::levels::{LevelStyle, LevelValue};
    let build = |series_update: bool| {
        let b = LevelMap::builder("demo", lines_core::ProductId(1), lines_core::UtcMillis(0)).line(
            "pivot",
            LevelValue::Tick(Price(600_000)),
            Some("P"),
            LevelStyle::default(),
        );
        let b = if series_update { b.series_update() } else { b };
        b.build(25).unwrap()
    };
    let mut seen = vec![LabelSeen::Hidden];
    reset_seen(Some(&build(true)), &mut seen);
    assert_eq!(seen, vec![LabelSeen::Hidden], "a push keeps it");
    reset_seen(Some(&build(false)), &mut seen);
    assert_eq!(seen, vec![LabelSeen::New], "a whole refresh starts over");
    let mut two = vec![LabelSeen::Hidden, LabelSeen::Shown];
    reset_seen(Some(&build(true)), &mut two);
    assert_eq!(two, vec![LabelSeen::New], "a different count starts over");
    reset_seen(None, &mut two);
    assert!(two.is_empty());
}

/// A theme switch drops the labels shaped in the old theme's text color; the cuts (texts and
/// widths, the same in every theme) stay, so the gutter doesn't move.
#[test]
fn theme_change_drops_shaped_labels_only() {
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.shaped.get_or_make(
        "Pivot 6010",
        gpui::Hsla::default(),
        gpui::ShapedLine::default,
    );
    layer
        .cuts
        .insert("Pivot 6010".into(), (40.0, "P… 6010".into()));
    layer.theme_changed();
    assert_eq!(layer.shaped.len(), 0);
    assert_eq!(layer.cuts.len(), 1);
}

/// The colors with a meaning a demo level's default color keeps clear of (CIEDE2000 >= 20,
/// the drawing colors' rule): up and down in both palettes of the theme, the accent and the
/// last price, warn, and the bracket target and stop.
fn reserved(t: &crate::theme::Theme) -> Vec<(&'static str, lines_core::style::Rgb)> {
    use crate::theme::{Palette, Theme, rgb_of};
    let twins = if t.is_dark() {
        [
            Theme::dark(Palette::Standard),
            Theme::dark(Palette::Colorblind),
        ]
    } else {
        [
            Theme::light(Palette::Standard),
            Theme::light(Palette::Colorblind),
        ]
    };
    let mut v = Vec::new();
    for m in &twins {
        v.push(("up", rgb_of(m.up)));
        v.push(("down", rgb_of(m.down)));
    }
    for (n, c) in [
        ("accent", t.accent),
        ("last", t.last),
        ("warn", t.warn),
        ("order target", t.order_target),
        ("order stop", t.order_stop),
    ] {
        v.push((n, rgb_of(c)));
    }
    v
}

/// Every demo level's default color, as the layer resolves it (`study_style::resolve_type`,
/// then `TypeLook::line_color` for the theme; a type left at the level's own color draws the
/// map's color, decision 0022): a line (3:1) on the chart of every theme and palette (a zone's
/// edges a dim gray, seen but quieter than the candles), and clear of every color with a
/// meaning there.
#[test]
fn demo_level_colors_read_and_stay_off_meanings() {
    use crate::theme::{Theme, rgb_of, source_colour};
    use lines_core::style::{Color, contrast, delta_e_2000};
    use lines_feed::levels::demo::{BODY, TYPES};
    let doc: serde_json::Value = serde_json::from_str(BODY).unwrap();
    let mut rows: Vec<(String, String)> = doc["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().to_owned(),
                r["color"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let a = &doc["anchor"];
    rows.push(("anchor".into(), a["color"].as_str().unwrap().to_owned()));
    assert_eq!(rows.len(), 7, "every level and the anchor");
    // Support and the VWAP band's edges are told apart by their pattern.
    let line_of = |id: &str| TYPES.iter().find(|t| t.id == id).unwrap().style.line.line;
    assert_ne!(line_of("support"), line_of("band"));
    let source = Look::defaults(&study_style::LEVELS);
    let source = [source[0], source[1]];
    for t in Theme::all() {
        for (kind, hex) in &rows {
            let ty = TYPES.iter().find(|ty| ty.id == kind).expect("declared");
            let look = study_style::resolve_type(
                ty,
                &source,
                &study_style::read_type(&PartStyles::new(), ty),
            );
            // Every demo stroke is drawn whole (a level's own look draws zone edges at 60 %,
            // under 3:1 for a mid-tone): what's checked below is what's drawn.
            assert!(!look.own_line, "{kind}: drawn in the level's own look");
            let own = source_colour(u32::from_str_radix(hex, 16).unwrap(), 1.0);
            let drawn = match look.line_color(t.is_dark()) {
                Color::Own => own,
                c => t.part_color(c, own, 100),
            };
            let c = rgb_of(drawn);
            let r = contrast(c, rgb_of(t.bg));
            if ty.zone {
                // A zone's or band's edges frame it: a dim gray, quieter than the candles but
                // still seen.
                let candles = contrast(rgb_of(t.up), rgb_of(t.bg))
                    .min(contrast(rgb_of(t.down), rgb_of(t.bg)));
                assert!(
                    (1.5..candles).contains(&r),
                    "{}: {kind}: {r:.2} (candles {candles:.2})",
                    t.name
                );
            } else {
                assert!(r >= 3.0, "{}: {kind}: {r:.2}", t.name);
            }
            for (what, m) in reserved(&t) {
                let de = delta_e_2000(c, m);
                assert!(
                    de >= 20.0,
                    "{}: {kind} ({hex}) near {what}: {de:.1}",
                    t.name
                );
            }
        }
    }
}

/// Labels are in each level's color where that is AA on the label (4.5:1 on its box over
/// every underlay and the source's zone fills), else theme text for that label only: in every
/// theme and palette, for the type's recommended color, a user's color and a type asking for
/// text. The demo's demand zone sits under its low edge and its supply zone above its high
/// edge, each with that edge's price.
#[test]
fn labels_take_the_level_color_where_it_reads_and_their_types_edge() {
    use crate::theme::rgb_of;
    use lines_core::levels::{LabelAt, LabelColor, LabelMode};
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
    // Every label color reads (AA) on its box over the gutter's chart and grid, or is the
    // text.
    let reads = |t: &Theme, c: Hsla| t.level_label_ratio(c, &[]) >= 4.5;
    let mut colored = 0;
    let mut fell_back = 0;
    for t in Theme::all() {
        for c in layer.label_colors_for(i, &t) {
            if c == t.text_main {
                fell_back += 1;
            } else {
                colored += 1;
                assert!(reads(&t, c), "{}: {c:?}", t.name);
            }
        }
    }
    assert!(
        colored > 0,
        "the demo's labels take their colors where they read"
    );
    assert!(
        fell_back > 0,
        "and fall back where they don't (the light grays)"
    );
    // On dark, the pivot's label is the pivot's line color.
    let themes = Theme::all();
    let dark = &themes[0];
    assert!(dark.is_dark());
    let pivot = layer.label_colors_for(i, dark)[0];
    assert_ne!(pivot, dark.text_main);

    // A user's color that can't read (the chart's own background) falls back; a type asking
    // for text gets text.
    let (types, default) = layer.source_types("demo").unwrap();
    let mut styles = BTreeMap::new();
    let mut looks = Look::defaults(&study_style::LEVELS).to_vec();
    for t in types {
        looks.push(study_style::read_type(&PartStyles::new(), t));
    }
    let pivot_at = study_style::LEVELS.len();
    if let crate::style_editor::PartStyle::Line(l) = &mut looks[pivot_at].style {
        l.color = Color::Custom(rgb_of(dark.bg));
    }
    let mut labels: Vec<_> = types.iter().map(study_style::TypeLabel::of).collect();
    let resist = types.iter().position(|t| t.id == "resist").unwrap();
    labels[resist].color = LabelColor::Text;
    study_style::set_level_style(
        &mut styles,
        "demo",
        &study_style::LevelEdit {
            looks,
            types,
            labels,
            label: LabelMode::NamePrice,
            label_default: default,
        },
    );
    layer.set_styles(&styles);
    frame(&mut layer);
    let colors = layer.label_colors_for(i, dark);
    assert_eq!(colors[0], dark.text_main, "a color that can't read: text");
    let map = layer.map(i).unwrap().clone();
    let resist_level = map
        .levels
        .iter()
        .position(|l| map.kinds[usize::from(l.kind)] == "resist")
        .unwrap();
    assert_eq!(colors[resist_level], dark.text_main, "asked for text");
    // Saved as the type's own field, only while it differs.
    assert_eq!(
        styles["demo"]["types"]["resist"]["label-color"],
        serde_json::json!("text")
    );
    assert!(
        styles["demo"]["types"]["pivot"]
            .get("label-color")
            .is_none()
    );

    // The demo's demand zone: labeled under its low edge, with that edge's price; its supply
    // zone above its high edge, with that edge's price.
    for (kind, at, text) in [
        ("zone", LabelAt::Low, "Demand : 5990.00"),
        ("supply", LabelAt::High, "Supply : 6022.50"),
    ] {
        let zone = map
            .levels
            .iter()
            .position(|l| map.kinds[usize::from(l.kind)] == kind)
            .unwrap();
        let placed = *layer
            .placed(i)
            .iter()
            .find(|p| p.index as usize == zone)
            .unwrap();
        assert_eq!(placed.anchor, at, "{kind}");
        // On the chart beside its edge; a tag in the price axis on the edge's price itself.
        if let Some(y) = placed.label_y {
            let h = LevelsLayer::label_h();
            match (at, placed.on_edge) {
                (LabelAt::Low, true) => {
                    assert!((y - placed.bottom).abs() <= h, "on its low edge: {y}")
                }
                (_, true) => assert!((y - placed.top).abs() <= h, "on its high edge: {y}"),
                (LabelAt::Low, false) => assert!(y > placed.bottom, "under its low edge: {y}"),
                _ => assert!(y < placed.top, "above its high edge: {y}"),
            }
        }
        assert_eq!(layer.label_texts(i)[zone].as_deref(), Some(text), "{kind}");
    }
}

/// A child kind (`LevelType::parent`) hides with its parent's type too: hiding the parent in
/// its style (the style editor's Show; a saved `types.<parent>.shown: false`) hides the child
/// as well (the demo's supply zone is a kind of its zones).
#[test]
fn a_parent_type_hidden_in_its_style_hides_its_children() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let (mut layer, store, i) = demo_layer();
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
    let map = layer.map(i).unwrap().clone();
    let of = |kind: &str| {
        map.levels
            .iter()
            .position(|l| map.kinds[usize::from(l.kind)] == kind)
            .unwrap()
    };
    let (zone, supply) = (of("zone"), of("supply"));
    let placed =
        |layer: &LevelsLayer, at: usize| layer.placed(i).iter().any(|p| p.index as usize == at);
    assert!(placed(&layer, zone) && placed(&layer, supply));
    assert!(layer.kind_styled_shown("demo", "supply"));
    let hidden: BTreeMap<String, PartStyles> =
        serde_json::from_str(r#"{"demo": {"types": {"zone": {"shown": false}}}}"#).unwrap();
    layer.set_styles(&hidden);
    frame(&mut layer);
    assert!(!placed(&layer, zone), "the parent hidden");
    assert!(!placed(&layer, supply), "and its child with it");
    assert!(!layer.kind_styled_shown("demo", "supply"));
    // Hiding only the child leaves the parent.
    let child: BTreeMap<String, PartStyles> =
        serde_json::from_str(r#"{"demo": {"types": {"supply": {"shown": false}}}}"#).unwrap();
    layer.set_styles(&child);
    frame(&mut layer);
    assert!(placed(&layer, zone) && !placed(&layer, supply));
}

/// A legend row that doesn't fit the legend's width: a long title is cut with "…" at a whole
/// character, within the room; stats that don't fit are dropped whole (never a number cut
/// mid-glyph), leaving room for the closing "…"; a row that fits is left whole.
#[test]
fn a_long_legend_row_is_cut_whole() {
    use super::fit_legend;
    let mono = |t: &str| t.chars().count() as f32 * 7.0;
    // Fits: whole.
    let f = fit_legend(300.0, 70.0, 0.0, Some("Map A"), &[50.0, 60.0], 12.0, mono);
    assert_eq!(f.title.as_deref(), Some("Map A"));
    assert_eq!((f.stats, f.more), (2, false));
    // A title wider than its room: cut at a whole character, "…" last, within the room.
    let long = "A very long legend title from a source";
    let f = fit_legend(200.0, 70.0, 0.0, Some(long), &[], 12.0, mono);
    let t = f.title.unwrap();
    assert!(t.ends_with('…') && mono(&t) <= 130.0, "{t}");
    assert!(long.starts_with(t.trim_end_matches('…')), "{t}");
    // Stats: as many whole as fit with the "…" after them.
    let w = [80.0, 80.0, 80.0, 80.0];
    let f = fit_legend(200.0, 70.0, 0.0, None, &w, 12.0, mono);
    assert_eq!((f.stats, f.more), (2, true));
    assert!(80.0 * 2.0 + 12.0 + 12.0 + mono("…") <= 200.0);
    // The last one fits without room for "…": all shown.
    let f = fit_legend(80.0 * 2.0 + 12.0, 70.0, 0.0, None, &w[..2], 12.0, mono);
    assert_eq!((f.stats, f.more), (2, false));
    // Not even one stat: none, and the "…".
    let f = fit_legend(60.0, 70.0, 0.0, None, &w, 12.0, mono);
    assert_eq!((f.stats, f.more), (0, true));
    // A title whose room can't hold even "…": no title.
    assert_eq!(
        fit_legend(70.0, 70.0, 0.0, Some(long), &[], 12.0, mono).title,
        None
    );
}

/// A stale row keeps its "stale" whole: a 40-character title at 12 px (about 7 px a
/// character) is cut to leave room for the state and its gap, never the state squeezed out.
#[test]
fn a_stale_legend_row_keeps_stale_beside_a_long_title() {
    let mono = |t: &str| t.chars().count() as f32 * 7.0;
    // The legend's room inside its padding (360 px, 8 px a side, a pixel spare).
    let room = 360.0 - 16.0 - 1.0;
    let gap = 6.0;
    let head = mono("Acme Levels") + gap + mono("· ");
    let status = gap + mono("· stale");
    let title = "Map ABC with a forty character long title";
    assert!(title.chars().count() >= 40);
    let f = fit_legend(room, head, status, Some(title), &[], 12.0, mono);
    let t = f.title.expect("a cut title");
    assert!(t.ends_with('…'), "{t}");
    assert!(
        head + mono(&t) + status <= room,
        "{t}: stale keeps its room"
    );
    // Current (no state): the title gets that room too.
    let f = fit_legend(room, head, 0.0, Some(title), &[], 12.0, mono);
    assert!(f.title.unwrap().chars().count() > t.chars().count());
}

/// Labels of one text in four or more colors (levels of the same name in their own colors,
/// the stale dim) are each shaped once and kept: a second frame shapes nothing, and a flip
/// back and forth shapes nothing either. The cache stays bounded.
#[test]
fn same_text_labels_in_many_colors_are_shaped_once() {
    let mut cache: ColorCache<u32> = ColorCache::new();
    let mut made = 0;
    let colors: Vec<gpui::Hsla> = (0..6)
        .map(|i| gpui::hsla(i as f32 / 6.0, 0.6, 0.5, 1.0))
        .collect();
    for _frame in 0..3 {
        for c in &colors {
            cache.get_or_make("Demand Zone : 7555.00", *c, || {
                made += 1;
                made
            });
        }
    }
    assert_eq!(made, 6, "each color shaped once");
    // Bounded: past MAX_SHAPED it starts over, never grows past it.
    for k in 0..(MAX_SHAPED * 2) {
        cache.get_or_make(&format!("t{k}"), colors[0], || 0);
        assert!(cache.len() <= MAX_SHAPED);
    }
}

/// Level lines and zone edges leave the last-price line's row clear (it is drawn later,
/// translucent, and reads above them): a stroke across it is split around it, one beside it
/// is whole, one inside it is gone.
#[test]
fn level_strokes_leave_the_last_price_row_clear() {
    let parts = |y, w, last| around_last(y, w, last, 1.0).collect::<Vec<_>>();
    assert_eq!(parts(10.0, 2.0, None), [(10.0, 2.0)]);
    assert_eq!(parts(10.0, 2.0, Some(20.0)), [(10.0, 2.0)]);
    assert_eq!(parts(10.0, 1.0, Some(10.0)), []);
    assert_eq!(parts(10.0, 3.0, Some(11.0)), [(10.0, 1.0), (12.0, 1.0)]);
    assert_eq!(parts(10.0, 2.0, Some(9.5)), [(10.5, 1.5)]);
}

/// In footprint mode the last-price line breaks at drawn cells, so a level stroke keeps its
/// last-price gap only where that line is drawn: whole across a cell, split beside it.
#[test]
fn level_strokes_gap_only_where_the_last_price_line_is_drawn() {
    use crate::footprint::{FootprintConfig, FootprintLayer};
    let mut fp = FootprintLayer::new(FootprintConfig::default());
    let quads = |fp: &FootprintLayer, last| {
        let mut v = Vec::new();
        stroke_around_last(0.0, 100.0, 10.0, 3.0, last, 1.0, fp, &mut |a, b, t, h| {
            v.push((a, b, t, h))
        });
        v
    };
    // No footprint: split around the row across the whole stroke.
    assert_eq!(
        quads(&fp, Some(11.0)),
        [(0.0, 100.0, 10.0, 1.0), (0.0, 100.0, 12.0, 1.0)]
    );
    // Away from the row: whole.
    assert_eq!(quads(&fp, Some(40.0)), [(0.0, 100.0, 10.0, 3.0)]);
    // A cell across the row at 40..60: the stroke is whole there.
    fp.set_drawn(vec![[40.0, 60.0, 0.0, 20.0]]);
    assert_eq!(
        quads(&fp, Some(11.0)),
        [
            (0.0, 40.0, 10.0, 1.0),
            (0.0, 40.0, 12.0, 1.0),
            (40.0, 60.0, 10.0, 3.0),
            (60.0, 100.0, 10.0, 1.0),
            (60.0, 100.0, 12.0, 1.0),
        ]
    );
}

/// The last-price line never runs through a level label's chip: it breaks at each chip on its
/// row and runs whole past the others.
#[test]
fn the_last_price_line_breaks_at_label_chips() {
    let segs = |chips: &[[f32; 4]], y| {
        let mut v = Vec::new();
        around_chips(chips, y, 1.0, 0.0, 500.0, |a, b| v.push((a, b)));
        v
    };
    let chips = [[420.0, 496.0, 100.0, 116.0], [400.0, 496.0, 140.0, 156.0]];
    assert_eq!(segs(&chips, 50.0), [(0.0, 500.0)]);
    assert_eq!(segs(&chips, 108.0), [(0.0, 420.0), (496.0, 500.0)]);
    assert_eq!(segs(&chips, 150.0), [(0.0, 400.0), (496.0, 500.0)]);
    // A row touching a chip's edge only: a hair above its top is clear.
    assert_eq!(segs(&chips, 99.0), [(0.0, 500.0)]);
    // Two sources' chips overlapping on one row: one gap across both.
    let both = [[420.0, 496.0, 100.0, 116.0], [380.0, 450.0, 104.0, 120.0]];
    assert_eq!(segs(&both, 110.0), [(0.0, 380.0), (496.0, 500.0)]);
    assert_eq!(segs(&[], 110.0), [(0.0, 500.0)]);
}

/// A stale map's zone fills, zone edges and lines dim with it, whatever their color: a type's
/// fixed color (the demo's) and the source's own alike, in every theme.
#[test]
fn stale_fills_and_edges_dim_with_their_map() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let (layer, _store, i) = demo_layer();
    let slot = &layer.slots[i];
    let map = slot.map.clone().expect("a demo map");
    let mut zones = 0;
    for t in Theme::all() {
        for l in &map.levels {
            let zone = matches!(l.shape, LevelShape::Zone { .. });
            let pairs = [
                (
                    slot.stroke(&t, l, 1.0, false, 1.0).color,
                    slot.stroke(&t, l, 0.4, false, 1.0).color,
                ),
                (
                    slot.stroke(&t, l, 1.0, zone, 1.0).color,
                    slot.stroke(&t, l, 0.4, zone, 1.0).color,
                ),
            ];
            for (now, stale) in pairs {
                assert!(
                    (stale.a - now.a * 0.4).abs() < 1e-5,
                    "{}: {:?} {:?}",
                    t.name,
                    now,
                    stale
                );
            }
            if zone {
                zones += 1;
                let (now, stale) = (slot.zone_fill(&t, l, 1.0), slot.zone_fill(&t, l, 0.4));
                assert!(now.a > 0.0);
                assert!((stale.a - now.a * 0.4).abs() < 1e-5, "{}: fill", t.name);
                assert_eq!((stale.h, stale.s, stale.l), (now.h, now.s, now.l));
            }
        }
    }
    assert!(zones > 0, "the demo has zones");
}
