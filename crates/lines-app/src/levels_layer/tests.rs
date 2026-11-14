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
    // Hidden: still listed in the legend, dimmed, saying so (one click from coming back).
    let rows = layer.legend();
    let row = rows.iter().find(|r| r.label == "Demo levels").unwrap();
    assert!(row.hidden && row.dim && row.stats.is_empty() && row.text == "· hidden");
    assert_eq!(row.id, "demo");
    // Levels off as a whole: no rows at all.
    layer.config.hidden = true;
    assert!(layer.legend().is_empty());
    layer.config.hidden = false;
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

/// Every label metric scales with the UI zoom: at 150 % each is 1.5 times its 100 % value
/// (the text, the chip, its paddings, the leader and the stacking step), and the level layer's
/// metrics now are those at the zoom now (100 % in tests).
#[test]
fn label_metrics_scale_with_the_ui_zoom() {
    let (a, b) = (LabelMetrics::at(1.0), LabelMetrics::at(1.5));
    let pairs = [
        (a.text, b.text),
        (a.box_h, b.box_h),
        (a.chrome, b.chrome),
        (a.pad, b.pad),
        (a.gutter_pad, b.gutter_pad),
        (a.gutter_right, b.gutter_right),
        (a.newest_gap, b.newest_gap),
        (a.axis_leader, b.axis_leader),
        (a.axis_edge, b.axis_edge),
        (a.column_pad, b.column_pad),
    ];
    for (i, (x, y)) in pairs.into_iter().enumerate() {
        assert!(
            x > 0.0 && (y - x * 1.5).abs() < 1e-4,
            "metric {i}: {x} → {y}"
        );
    }
    assert_eq!(a.text, 11.0);
    assert_eq!(label_px(), a.text);
    assert_eq!(label_chrome(), a.chrome);
    assert_eq!(column_pad(), a.column_pad);
    // The axis frame and the chart room follow the metrics too.
    let (axis, room) = axis_fit(80.0, 20.0, 60.0, 400.0);
    assert_eq!(axis, (80.0 + a.axis_leader + a.chrome + a.axis_edge).ceil());
    assert_eq!(room, Some(axis - (a.axis_leader + a.chrome + a.axis_edge)));
}

/// At 150 % labels stack a 150 % chip apart: levels closer than that never get overlapping
/// labels (each shown label is at least a chip's height from the next).
#[test]
fn labels_never_overlap_at_150_percent() {
    use lines_core::levels::{LabelSeen, Placed, place_labels};
    let m = LabelMetrics::at(1.5);
    let mut placed: Vec<Placed> = (0..8)
        .map(|i| Placed {
            index: i,
            top: 200.0 + i as f32 * 9.0,
            bottom: 200.0 + i as f32 * 9.0,
            label_y: None,
            vwap_relative: false,
            anchor: lines_core::levels::LabelAt::Own,
            on_edge: false,
            rank: 1,
            x0: 0.0,
            x1: 800.0,
        })
        .collect();
    let mut seen = vec![LabelSeen::New; 8];
    place_labels(&mut placed, m.box_h, 700.0, None, &mut seen, |_| true);
    let mut ys: Vec<f32> = placed.iter().filter_map(|p| p.label_y).collect();
    assert!(ys.len() >= 2, "some labels shown");
    ys.sort_by(f32::total_cmp);
    for w in ys.windows(2) {
        assert!(w[1] - w[0] >= m.box_h - 1e-3, "{ys:?} at {}", m.box_h);
    }
    // At 100 % the same levels fit more labels: the step is the zoom's.
    let mut placed_100 = placed.clone();
    let mut seen = vec![LabelSeen::New; 8];
    place_labels(
        &mut placed_100,
        LabelMetrics::at(1.0).box_h,
        700.0,
        None,
        &mut seen,
        |_| true,
    );
    let n_100 = placed_100.iter().filter(|p| p.label_y.is_some()).count();
    assert!(n_100 >= ys.len());
}

// ---- one label layout for every source -------------------------------------------------

/// Whether [`Crowding`] serves ES now (only while a test holds a [`CrowdGuard`]): sources can't
/// be unregistered, and one that serves nothing joins no other test's chart.
static CROWD_SERVES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `demo-crowd` (labels at and beside the demo's, half of them Low priority), serving only
/// while [`CROWD_SERVES`] says so.
struct Crowding;

impl lines_feed::levels::LevelSource for Crowding {
    fn id(&self) -> &'static str {
        lines_feed::levels::demo::CROWD_ID
    }
    fn label(&self) -> &'static str {
        "Crowd levels"
    }
    fn serves(&self, p: &Product) -> bool {
        CROWD_SERVES.load(std::sync::atomic::Ordering::Acquire)
            && lines_feed::levels::demo::DemoCrowd.serves(p)
    }
    fn refresh(&self) -> lines_feed::levels::RefreshMode {
        lines_feed::levels::RefreshMode::OneShot
    }
    fn synthetic(&self) -> bool {
        true
    }
    fn stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(24 * 3600)
    }
    fn types(&self) -> &'static [LevelType] {
        &lines_feed::levels::demo::CROWD_TYPES
    }
    fn connect(
        &self,
        c: &LevelContext,
    ) -> lines_feed::levels::BoxFuture<
        'static,
        anyhow::Result<Box<dyn lines_feed::levels::LevelTransport>>,
    > {
        lines_feed::levels::demo::DemoCrowd.connect(c)
    }
    fn decode(
        &self,
        c: &LevelContext,
        p: &dyn lines_feed::levels::ChartPrices,
        prev: Option<&LevelMap>,
        f: &[lines_feed::levels::LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        lines_feed::levels::demo::DemoCrowd.decode(c, p, prev, f)
    }
}

/// Serves [`Crowding`] for the test that holds it; serving nothing again however it ends.
struct CrowdGuard;

impl CrowdGuard {
    fn on() -> CrowdGuard {
        CROWD_SERVES.store(true, std::sync::atomic::Ordering::Release);
        lines_feed::levels::serving_changed();
        CrowdGuard
    }
}

impl Drop for CrowdGuard {
    fn drop(&mut self) {
        CROWD_SERVES.store(false, std::sync::atomic::Ordering::Release);
        lines_feed::levels::serving_changed();
    }
}

/// A layer on ES with the demo and the crowding source, labels measured, laid out in a plot
/// `h` px tall over 5980 to 6040 (the last price's tag at `blocked`, if any).
fn crowded(h: f64, blocked: Option<(f32, f32)>) -> (LevelsLayer, Arc<ChartStore>) {
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    lines_feed::levels::register(Arc::new(Crowding)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let d = store.read();
    layer.snapshot(d.market_time);
    layer.measure_labels(|t| t.chars().count() as f32 * 6.0);
    layer.set_room(Some(400.0));
    layer.set_blocked(blocked);
    let mut v = vp();
    v.set_size(800.0, h);
    let bars = BarsView::fixed(d.series.bars(), d.series.period_ms());
    layer.prepare(&d, bars, &v);
    drop(d);
    (layer, store)
}

/// Every shown label of every source: (source id, kind, its row).
fn shown(layer: &LevelsLayer) -> Vec<(String, String, f32)> {
    let mut out = Vec::new();
    for (i, (id, ..)) in layer.sources().iter().enumerate() {
        let Some(map) = layer.map(i) else { continue };
        for p in layer.placed(i) {
            if let Some(y) = p.label_y {
                let kind = map.kinds[usize::from(map.levels[p.index as usize].kind)].clone();
                out.push((id.clone(), kind, y));
            }
        }
    }
    out
}

/// Labels from two sources at the same and adjacent prices (the demo's Pivot 6010 beside the
/// crowd's Dyn HP 6010 and Det IRL 6010.25; R1 6025 beside 6024.75 and 6025) never overlap:
/// both sources' labels are laid out together.
#[test]
fn two_sources_labels_never_overlap() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _crowd = CrowdGuard::on();
    let (layer, _store) = crowded(400.0, None);
    let rows = shown(&layer);
    assert!(rows.iter().any(|(id, ..)| id == "demo"), "{rows:?}");
    assert!(rows.iter().any(|(id, ..)| id == "demo-crowd"), "{rows:?}");
    let h = LevelsLayer::label_h();
    for (a, x) in rows.iter().enumerate() {
        for y in &rows[a + 1..] {
            assert!((x.2 - y.2).abs() >= h - 1e-3, "{x:?} over {y:?}");
        }
    }
}

/// A pane too short for every label hides the lowest priority first: while any Normal label
/// is hidden, no Low one shows. Their lines stay placed.
#[test]
fn a_short_pane_hides_low_before_normal() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _crowd = CrowdGuard::on();
    let (layer, _store) = crowded(70.0, None);
    let crowd = layer
        .sources()
        .iter()
        .position(|(id, ..)| id == "demo-crowd")
        .expect("crowd serves ES");
    let map = layer.map(crowd).unwrap().clone();
    let kind = |p: &Placed| map.kinds[usize::from(map.levels[p.index as usize].kind)].clone();
    let placed = layer.placed(crowd);
    assert!(
        placed.iter().any(|p| kind(p) == "det"),
        "Low levels still placed (their lines draw)"
    );
    let low_shown = placed
        .iter()
        .any(|p| kind(p) == "det" && p.label_y.is_some());
    let normal_hidden = (0..layer.sources().len()).any(|i| {
        let m = layer.map(i).cloned();
        layer.placed(i).iter().any(|p| {
            m.as_ref().is_some_and(|m| {
                m.levels[p.index as usize].label.is_some()
                    && m.kinds[usize::from(m.levels[p.index as usize].kind)] != "det"
            }) && p.label_y.is_none()
        })
    });
    assert!(normal_hidden, "the pane is short enough to hide some");
    assert!(
        !low_shown,
        "no Low label shows while a Normal one is hidden"
    );
}

/// The last price's tag outranks every source: no label of either source sits on it, even at
/// the crowd's busiest price.
#[test]
fn the_last_price_always_wins() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _crowd = CrowdGuard::on();
    let at = vp().price_to_y(601_000.0) as f32;
    let tag_h = 18.0;
    let (layer, _store) = crowded(400.0, Some((at, tag_h)));
    let h = LevelsLayer::label_h();
    let rows = shown(&layer);
    assert!(!rows.is_empty());
    for (id, kind, y) in rows {
        assert!(
            (y - at).abs() >= (h + tag_h) * 0.5 - 1e-3,
            "{id}/{kind} at {y} on the last price at {at}"
        );
    }
}

/// A hidden label shows while the pointer is at its level's row: the hover finds it (the
/// highest priority, then the nearest), and only a hidden one.
#[test]
fn pointing_at_a_hidden_label_shows_it() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _crowd = CrowdGuard::on();
    let ph = 70.0f32;
    let (layer, _store) = crowded(f64::from(ph), None);
    let half = LevelsLayer::label_h() * 0.5;
    let mut hidden = None;
    for i in 0..layer.sources().len() {
        let Some(map) = layer.map(i) else { continue };
        for p in layer.placed(i) {
            if p.label_y.is_none() && map.levels[p.index as usize].label.is_some() {
                hidden = Some(p.label_anchor(half).clamp(half, ph - half));
            }
        }
    }
    let hy = hidden.expect("a hidden label");
    let (si, p, y) = layer
        .revealed_label(hy, ph, 800.0)
        .expect("shown for the pointer");
    assert!(p.label_y.is_none(), "only a hidden label is revealed");
    assert!((y - hy).abs() <= half + 1e-3);
    assert!(layer.map(si).is_some());
    // Far from every level: nothing.
    assert_eq!(layer.revealed_label(-500.0, ph, 800.0), None);
}

// ---- event levels ------------------------------------------------------------------------

/// Whether [`Eventing`] serves ES now (only while a test holds an [`EventGuard`]).
static EVENT_SERVES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `demo-event` (an event: lines from its bars, markers, a box, one group), serving only while
/// [`EVENT_SERVES`] says so.
struct Eventing;

impl lines_feed::levels::LevelSource for Eventing {
    fn id(&self) -> &'static str {
        lines_feed::levels::demo::EVENT_ID
    }
    fn label(&self) -> &'static str {
        "Event levels"
    }
    fn serves(&self, p: &Product) -> bool {
        EVENT_SERVES.load(std::sync::atomic::Ordering::Acquire)
            && lines_feed::levels::demo::DemoEvent.serves(p)
    }
    fn refresh(&self) -> lines_feed::levels::RefreshMode {
        lines_feed::levels::RefreshMode::OneShot
    }
    fn synthetic(&self) -> bool {
        true
    }
    fn stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(24 * 3600)
    }
    fn types(&self) -> &'static [LevelType] {
        &lines_feed::levels::demo::EVENT_TYPES
    }
    fn connect(
        &self,
        c: &LevelContext,
    ) -> lines_feed::levels::BoxFuture<
        'static,
        anyhow::Result<Box<dyn lines_feed::levels::LevelTransport>>,
    > {
        lines_feed::levels::demo::DemoEvent.connect(c)
    }
    fn decode(
        &self,
        c: &LevelContext,
        p: &dyn lines_feed::levels::ChartPrices,
        prev: Option<&LevelMap>,
        f: &[lines_feed::levels::LevelFrame],
    ) -> Result<Option<LevelMap>, String> {
        lines_feed::levels::demo::DemoEvent.decode(c, p, prev, f)
    }
}

/// Serves [`Eventing`] for the test that holds it; nothing again however it ends.
struct EventGuard;

impl EventGuard {
    fn on() -> EventGuard {
        EVENT_SERVES.store(true, std::sync::atomic::Ordering::Release);
        lines_feed::levels::serving_changed();
        EventGuard
    }
}

impl Drop for EventGuard {
    fn drop(&mut self) {
        EVENT_SERVES.store(false, std::sync::atomic::Ordering::Release);
        lines_feed::levels::serving_changed();
    }
}

/// 31 one-minute bars from 20:50 to 21:20 UTC (the demo event's A at 20:58, E1 21:06, V
/// 21:12), 10 px apart, the newest `right` bars from the start at the plot's right edge.
fn event_bars() -> Vec<lines_core::Bar> {
    (0..31i64)
        .map(|m| lines_core::Bar {
            time: UtcMillis(UtcMillis::parse_iso("2026-11-05T20:50:00").unwrap().0 + m * 60_000),
            open: Price(601_000),
            high: Price(601_100),
            low: Price(600_900),
            close: Price(601_025),
            volume: Qty(1),
        })
        .collect()
}

/// A layer on ES with the demo and the event source, placed over 5990 to 6030 on `bars` with
/// the plot's right edge at bar `right`.
fn evented(bars: &[lines_core::Bar], right: f64) -> (LevelsLayer, Arc<ChartStore>, Viewport) {
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    lines_feed::levels::register(Arc::new(Eventing)).unwrap();
    let store = es_store();
    let mut layer = LevelsLayer::new(LevelsConfig::default());
    layer.attach(&store);
    assert!(lines_feed::levels::runtime::wait_settled(
        std::time::Duration::from_secs(10)
    ));
    let d = store.read();
    layer.snapshot(d.market_time);
    layer.measure_labels(|t| t.chars().count() as f32 * 6.0);
    layer.set_room(Some(400.0));
    let mut v = Viewport::default();
    v.set_size(800.0, 400.0);
    (v.price_lo, v.price_hi) = (599_000.0, 603_000.0);
    v.bar_px = 10.0;
    v.right_index = right;
    layer.prepare(&d, BarsView::fixed(bars, 60_000), &v);
    drop(d);
    (layer, store, v)
}

fn source_at(layer: &LevelsLayer, id: &str) -> usize {
    layer
        .sources()
        .iter()
        .position(|(s, ..)| s == id)
        .unwrap_or_else(|| panic!("{id} serves ES"))
}

/// The placed level labelled `label` of source `i`.
fn placed_named(layer: &LevelsLayer, i: usize, label: &str) -> Placed {
    let map = layer.map(i).unwrap();
    *layer
        .placed(i)
        .iter()
        .find(|p| map.levels[p.index as usize].label.as_deref() == Some(label))
        .unwrap_or_else(|| panic!("{label} placed"))
}

/// An event's lines begin at their own bars, never earlier: A's, the IRL and the YL at the
/// anchor's (20:58: bar 8), E1's at its own (21:06: bar 16); the box from the anchor's bar to
/// its end bar; an ordinary level (no start time) and every other source's across the plot.
#[test]
fn event_lines_start_at_their_bars() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _ev = EventGuard::on();
    let bars = event_bars();
    let (layer, _store, v) = evented(&bars, 30.0);
    let x = |i: f64| v.index_to_x(i) as f32;
    let ev = source_at(&layer, "demo-event");
    let a = placed_named(&layer, ev, "A 6005");
    assert_eq!((a.x0, a.x1), (x(8.0), 800.0));
    assert_eq!(placed_named(&layer, ev, "Impact 6013.50").x0, x(8.0));
    assert_eq!(placed_named(&layer, ev, "E1 6022").x0, x(16.0));
    let prior = layer.map(ev).unwrap();
    let boxed = layer
        .placed(ev)
        .iter()
        .find(|p| prior.levels[p.index as usize].to.is_some())
        .copied()
        .expect("the box");
    // Its bars' whole slots: half a bar either side of their centres.
    assert_eq!((boxed.x0, boxed.x1), (x(8.0) - 5.0, x(16.0) + 5.0));
    // The run's line from V's bar (21:12: bar 22).
    assert_eq!(placed_named(&layer, ev, "Run 6016").x0, x(22.0));
    // An old source's levels: across the whole plot, as before.
    let demo = source_at(&layer, "demo");
    assert!(
        layer
            .placed(demo)
            .iter()
            .all(|p| (p.x0, p.x1) == (0.0, 800.0))
    );
}

/// Panned back so the anchor's bar is right of the plot: the event's lines haven't begun, so
/// none is drawn and none keeps a label; the ordinary line still is.
#[test]
fn an_event_not_begun_draws_nothing() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _ev = EventGuard::on();
    let bars = event_bars();
    let (layer, _store, _) = evented(&bars, 4.0);
    let ev = source_at(&layer, "demo-event");
    let a = placed_named(&layer, ev, "A 6005");
    assert!(a.x0 > a.x1, "not drawn");
    assert_eq!(a.label_y, None, "no label");
    // It claims no label row (laid out once the times are known), and pointing at its row
    // reveals nothing of it.
    let half = LevelsLayer::label_h() * 0.5;
    let row = a.label_anchor(half);
    let mut rows = Vec::new();
    layer.tag_rows(|y| rows.push(y));
    assert!(rows.iter().all(|y| (y - row).abs() > 1e-3), "{rows:?}");
    assert!(
        layer
            .revealed_label(row, 400.0, 800.0)
            .is_none_or(|(si, p, _)| !(si == ev && p.index == a.index)),
        "an undrawn level isn't revealed"
    );
}

/// Pointing at any member (a marker, a level's line where it is drawn) finds its event; left
/// of where a line begins, or on an ungrouped level, nothing. While an event is highlighted
/// its members stay whole and everything else dims.
#[test]
fn pointing_at_a_member_finds_its_event() {
    let _one = DEMO.lock().unwrap_or_else(|e| e.into_inner());
    let _ev = EventGuard::on();
    let bars = event_bars();
    let (layer, _store, v) = evented(&bars, 30.0);
    let ev = source_at(&layer, "demo-event");
    let g = layer
        .map(ev)
        .unwrap()
        .groups
        .iter()
        .position(|g| g == "demo/event-1");
    let want = Some((ev, g.unwrap() as u16));
    // On the A marker.
    let (ax, ay) = (v.index_to_x(8.0) as f32, v.price_to_y(600_500.0) as f32);
    assert_eq!(layer.group_at(ax + 1.0, ay - 1.0), want);
    // On the IRL line, right of where it begins.
    let irl = v.price_to_y(601_350.0) as f32;
    assert_eq!(layer.group_at(700.0, irl + 1.0), want);
    // Left of where it begins: not on it.
    assert_eq!(layer.group_at(100.0, irl), None);
    // Far from everything.
    assert_eq!(layer.group_at(100.0, 395.0), None);
    // The highlight: members whole, the rest dimmed.
    let hot = want;
    let g = want.map(|(_, g)| g);
    assert_eq!(group_dim(hot, ev, g), 1.0);
    assert_eq!(group_dim(hot, ev, None), GROUP_DIM);
    assert_eq!(group_dim(hot, ev + 1, g), GROUP_DIM);
    assert_eq!(group_dim(None, ev, None), 1.0);
}

/// The markers' ink is the accent (the approved violet) and their labels are AA on their plates
/// in every theme: the accent where it is, else the theme's text. Dots keep 3:1.
#[test]
fn marker_colors_read_in_every_theme() {
    use crate::theme::{Theme, contrast};
    for t in Theme::all() {
        assert!(
            contrast(marker_ink(&t), t.bg) >= 3.0,
            "dot 3:1 ({})",
            t.is_dark()
        );
        assert!(
            contrast(marker_text(&t), t.legend_box()) >= 4.5,
            "label AA on its plate ({})",
            t.is_dark()
        );
    }
}

/// The highlight sets everything else clearly back (the theme's fade, 35 %); a highlighted
/// event's label the crosshair's tag would cover steps just beside the tag, never under it.
#[test]
fn a_highlighted_event_stands_out() {
    assert!((0.35..=0.4).contains(&crate::theme::GROUP_FADE));
    assert_eq!(GROUP_DIM, crate::theme::GROUP_FADE);
    // A tag 20 tall at 100, a label 16 tall, a plot 400 tall: above it from at or above its
    // centre, else below, each clear of it.
    let (cy, ch, bh, ph) = (100.0, 20.0, 16.0, 400.0);
    for y in [92.0, 100.0, 108.0] {
        let at = beside_cross(y, cy, ch, bh, ph, &[], None).unwrap();
        assert!((at - cy).abs() >= (ch + bh) * 0.5, "{y} -> {at}");
        assert_eq!(at < cy, y <= cy);
    }
    // Two of the event's labels on the same side: the second takes the next free row, never
    // the first's.
    let first = beside_cross(95.0, cy, ch, bh, ph, &[], None).unwrap();
    let second = beside_cross(90.0, cy, ch, bh, ph, &[first], None).unwrap();
    assert!((second - first).abs() >= bh, "{first} {second}");
    // Another source's label already drawn just above the tag: stepped past it.
    let other = cy - 22.5;
    let at = beside_cross(95.0, cy, ch, bh, ph, &[other], None).unwrap();
    assert!((at - other).abs() >= bh && (at - cy).abs() >= (ch + bh) * 0.5);
    // Clear of the last-price tag.
    let last = Some((cy - 19.0, 18.0));
    let at = beside_cross(95.0, cy, ch, bh, ph, &[], last).unwrap();
    assert!((at - (cy - 19.0)).abs() >= (18.0 + bh) * 0.5);
    // At the plot's top: below the tag instead, never off the plot.
    let at = beside_cross(3.0, 5.0, ch, bh, ph, &[], None).unwrap();
    assert!(at > 5.0 && at <= ph - bh * 0.5);
    // No room either way (a plot hardly taller than the tag): none.
    assert_eq!(beside_cross(10.0, 10.0, ch, bh, 24.0, &[], None), None);
}
