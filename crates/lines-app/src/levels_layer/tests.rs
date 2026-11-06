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

/// The demo map on an ES chart: placement, per-kind and per-source visibility, labels off
/// the last-price row, legend freshness, stale and missing.
#[test]
fn layer_places_filters_and_reports_freshness() {
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
    let prepare = |layer: &mut LevelsLayer, last_y: Option<f32>| {
        let d = store.read();
        layer.prepare(&d, &vp(), last_y);
    };
    prepare(&mut layer, None);
    // 3 lines + 2 zones + the anchored line (the chart has the 14:30 bar): all on screen.
    assert_eq!(layer.placed_per_source()[i], 6);
    let (_, _, kinds) = &layer.sources()[i];
    assert_eq!(kinds, &["pivot", "resist", "support", "zone", "anchor"]);
    let rows = layer.legend();
    assert_eq!(rows[i].text, "as of 13:00Z");
    assert!(!rows[i].warn);

    // Kinds and sources hide; labels move off the last-price row.
    layer.config.toggle_kind("demo", "zone");
    prepare(&mut layer, None);
    assert_eq!(layer.placed_per_source()[i], 4);
    layer.config.toggle_kind("demo", "zone");
    // The pivot (6010.00) sits at (604000 - 601000) / 15 = 200 px; a last price there pushes
    // its label one label height (17.6 px) above.
    prepare(&mut layer, Some(200.0));
    let pivot = layer.placed(i).iter().find(|p| p.index == 0).unwrap();
    assert_eq!(pivot.top, 200.0);
    assert!((pivot.label_y.unwrap() - (200.0 - LABEL_PX * 1.6)).abs() < 1e-3);
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
