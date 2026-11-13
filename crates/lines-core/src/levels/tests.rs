use super::*;
use crate::qty::Qty;

const TICK: i64 = 25;
const ES: ProductId = ProductId(1);

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn style() -> LevelStyle {
    LevelStyle::default()
}

fn demo() -> LevelMap {
    LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"))
        .line(
            "pivot",
            LevelValue::Tick(Price(600_000)),
            Some("P"),
            style(),
        )
        .zone(
            "zone",
            LevelValue::Tick(Price(599_500)),
            LevelValue::Tick(Price(599_750)),
            Some("Z1"),
            style(),
        )
        .line("pivot", LevelValue::Raw(600_512.4), None, style())
        .build(TICK)
        .unwrap()
}

#[test]
fn builder_validates_and_indexes_kinds() {
    let m = demo();
    assert_eq!(m.kinds, vec!["pivot", "zone"]);
    assert_eq!(
        m.levels.iter().map(|l| l.kind).collect::<Vec<_>>(),
        [0, 1, 0]
    );
    assert_eq!(m.kind_index("zone"), Some(1));
    assert_eq!(m.levels[1].span_at(None).unwrap(), (599_500.0, 599_750.0));
    // A raw value is kept as the source gave it (rounded only when drawn).
    assert_eq!(m.levels[2].span_at(None).unwrap().0, 600_512.4);

    let b = || LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"));
    let err = |r: Result<LevelMap, LevelError>| r.unwrap_err();
    assert_eq!(
        err(b()
            .line("p", LevelValue::Tick(Price(600_010)), None, style())
            .build(TICK)),
        LevelError::OffTick(Price(600_010))
    );
    assert_eq!(
        err(b()
            .line("p", LevelValue::Raw(f64::NAN), None, style())
            .build(TICK)),
        LevelError::NotFinite
    );
    assert_eq!(
        err(b()
            .zone(
                "z",
                LevelValue::Raw(10.0),
                LevelValue::Raw(5.0),
                None,
                style()
            )
            .build(TICK)),
        LevelError::Inverted
    );
    assert_eq!(
        err(b()
            .line("Bad Kind", LevelValue::Raw(1.0), None, style())
            .build(TICK)),
        LevelError::BadSlug("Bad Kind".into())
    );
    assert_eq!(
        err(LevelMap::builder("", ES, UtcMillis(0)).build(TICK)),
        LevelError::BadSlug(String::new())
    );
    let heavy = LevelStyle {
        weight: 9,
        ..style()
    };
    assert_eq!(
        err(b().line("p", LevelValue::Raw(1.0), None, heavy).build(TICK)),
        LevelError::BadWeight(9)
    );
    let long = "x".repeat(65);
    assert_eq!(
        err(b()
            .line("p", LevelValue::Raw(1.0), Some(&long), style())
            .build(TICK)),
        LevelError::TooLong("label")
    );
    let mut many = b();
    for _ in 0..=MAX_LEVELS {
        many = many.line("p", LevelValue::Raw(1.0), None, style());
    }
    assert_eq!(err(many.build(TICK)), LevelError::TooMany("levels"));
    // An empty map is valid (the source has nothing today): shown as "no levels", not missing.
    assert!(b().build(TICK).unwrap().levels.is_empty());
}

fn vp() -> Viewport {
    let mut v = Viewport::default();
    v.set_size(800.0, 400.0);
    (v.price_lo, v.price_hi) = (599_000.0, 601_000.0); // 2000 units over 400 px: 5 units/px
    v
}

#[test]
fn placement_filters_kinds_and_skips_off_screen_levels() {
    let mut m = demo();
    m.levels.push(Level {
        kind: 0,
        shape: LevelShape::Line(LevelValue::Tick(Price(700_000))), // far above
        label: None,
        style: style(),
        meta: LevelMeta::default(),
    });
    let mut out = Vec::with_capacity(16);
    place(&m, &vp(), None, |_| true, &mut out);
    assert_eq!(out.iter().map(|p| p.index).collect::<Vec<_>>(), [0, 1, 2]);
    // Line at 600_000: (601_000 - 600_000) / 5 = 200 px. Zone 599_500..599_750: 250..300 px.
    assert_eq!((out[0].top, out[0].bottom), (200.0, 200.0));
    assert_eq!((out[1].top, out[1].bottom), (250.0, 300.0));
    // Kind filter: only zones.
    place(&m, &vp(), None, |k| k == 1, &mut out);
    assert_eq!(out.iter().map(|p| p.index).collect::<Vec<_>>(), [1]);
    // Capacity bounds the work; never grows.
    let mut small = Vec::with_capacity(1);
    place(&m, &vp(), None, |_| true, &mut small);
    assert_eq!((small.len(), small.capacity()), (1, 1));
}

#[test]
fn labels_sit_on_their_levels_and_give_way_to_each_other() {
    let m = demo();
    let mut out = Vec::with_capacity(16);
    let mut shown = [LabelSeen::New; 8];
    place(&m, &vp(), None, |_| true, &mut out);
    // Labels on their levels (the unlabelled raw line gets none).
    place_labels(&mut out, 16.0, 400.0, None, &mut shown, |i| {
        m.levels[i as usize].label.is_some()
    });
    assert_eq!(
        out.iter().map(|p| p.label_y).collect::<Vec<_>>(),
        [Some(200.0), Some(250.0), None]
    );
    assert_eq!(
        &shown[..3],
        &[LabelSeen::Shown, LabelSeen::Shown, LabelSeen::New]
    );
    // Two labels within one label height: the second stacks to the next row (a label's
    // height under the first, 11 px from its own line), never over it.
    let close = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .line("a", LevelValue::Raw(599_975.0), Some("B"), style()) // 5 px lower
        .build(TICK)
        .unwrap();
    place(&close, &vp(), None, |_| true, &mut out);
    place_labels(&mut out, 16.0, 400.0, None, &mut [LabelSeen::New; 2], |i| {
        close.levels[i as usize].label.is_some()
    });
    assert_eq!(out.len(), 2);
    assert_eq!((out[0].label_y, out[1].label_y), (Some(200.0), Some(216.0)));
    // Near the top edge: clamped inside the plot.
    let top = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(601_000.0), Some("T"), style())
        .build(TICK)
        .unwrap();
    place(&top, &vp(), None, |_| true, &mut out);
    place_labels(&mut out, 16.0, 400.0, None, &mut [LabelSeen::New; 1], |i| {
        top.levels[i as usize].label.is_some()
    });
    assert_eq!(out[0].label_y, Some(8.0));
}

fn bar(at: &str, open: i64) -> Bar {
    Bar {
        time: t(at),
        open: Price(open),
        high: Price(open + 100),
        low: Price(open - 100),
        close: Price(open + 25),
        volume: Qty(1),
    }
}

#[test]
fn price_at_time_edge_cases() {
    // No history.
    assert_eq!(price_at(&[], t("2026-11-05T14:30:00"), 0), None);
    let bars = [
        bar("2026-11-05T14:29:00", 600_000),
        bar("2026-11-05T14:30:00", 600_100),
        // gap: no 14:31, 14:32
        bar("2026-11-05T14:33:00", 600_300),
    ];
    // Exact bar boundary: the open of the bar starting there, not the previous close.
    assert_eq!(
        price_at(&bars, t("2026-11-05T14:30:00"), 0),
        Some(Price(600_100))
    );
    // Inside a bar but beyond the tolerance: not a start-stamped price.
    assert_eq!(price_at(&bars, t("2026-11-05T14:30:20"), 5_000), None);
    assert_eq!(
        price_at(&bars, t("2026-11-05T14:30:20"), 30_000),
        Some(Price(600_100))
    );
    // Just before a bar start, within tolerance: that bar.
    assert_eq!(
        price_at(&bars, t("2026-11-05T14:29:59"), 2_000),
        Some(Price(600_100))
    );
    // In the gap: nothing starts near 14:31:30.
    assert_eq!(price_at(&bars, t("2026-11-05T14:31:30"), 30_000), None);
    // Before the first bar and after the last.
    assert_eq!(price_at(&bars, t("2026-11-05T14:00:00"), 60_000), None);
    assert_eq!(price_at(&bars, t("2026-11-05T15:00:00"), 60_000), None);
    // Equally near two starts: the earlier.
    assert_eq!(
        price_at(&bars, t("2026-11-05T14:29:30"), 30_000),
        Some(Price(600_000))
    );
}

#[test]
fn vwap_relative_levels_follow_the_vwap_and_wait_for_it() {
    let b = || LevelMap::builder("demo", ES, UtcMillis(0));
    // VWAP ± 5.00 band and a VWAP + 2.50 line.
    let m = b()
        .zone(
            "band",
            LevelValue::VwapOffset(-500.0),
            LevelValue::VwapOffset(500.0),
            Some("±1"),
            style(),
        )
        .line("band", LevelValue::VwapOffset(250.0), None, style())
        .build(TICK)
        .unwrap();
    assert!(m.levels.iter().all(|l| l.is_vwap_relative()));
    assert_eq!(m.levels[0].vwap_offsets(), Some((-500.0, 500.0)));
    assert_eq!(m.levels[0].span_at(None), None);
    assert_eq!(
        m.levels[0].span_at(Some(600_000.0)),
        Some((599_500.0, 600_500.0))
    );
    let mut out = Vec::with_capacity(8);
    // No VWAP yet: withheld, not drawn at some made-up price.
    place(&m, &vp(), None, |_| true, &mut out);
    assert!(out.is_empty());
    // At VWAP 6000.00 (200 px): the band spans 600_500 (100 px) .. 599_500 (300 px).
    place(&m, &vp(), Some(600_000.0), |_| true, &mut out);
    assert_eq!(out.len(), 2);
    assert!(out[0].vwap_relative);
    assert_eq!((out[0].top, out[0].bottom), (100.0, 300.0));
    // It moves with the VWAP.
    place(&m, &vp(), Some(600_250.0), |_| true, &mut out);
    assert_eq!((out[0].top, out[0].bottom), (50.0, 250.0));
    // A zone mixing relative and absolute bounds is refused; offsets must be ordered.
    assert_eq!(
        b().zone(
            "z",
            LevelValue::VwapOffset(-1.0),
            LevelValue::Raw(600_000.0),
            None,
            style()
        )
        .build(TICK)
        .unwrap_err(),
        LevelError::MixedAnchors
    );
    assert_eq!(
        b().zone(
            "z",
            LevelValue::VwapOffset(5.0),
            LevelValue::VwapOffset(-5.0),
            None,
            style()
        )
        .build(TICK)
        .unwrap_err(),
        LevelError::Inverted
    );
    assert_eq!(
        b().line("z", LevelValue::VwapOffset(f64::INFINITY), None, style())
            .build(TICK)
            .unwrap_err(),
        LevelError::NotFinite
    );
}

#[test]
fn named_values_ride_with_the_map() {
    let at = Some(t("2026-11-05T14:31:00"));
    let m = b()
        .value("dd-ratio", 0.62, at)
        .value("res", -3.5, None)
        .build(TICK)
        .unwrap();
    assert_eq!(
        m.value("dd-ratio").map(|v| (v.value, v.as_of)),
        Some((0.62, at))
    );
    assert_eq!(m.value("res").map(|v| v.value), Some(-3.5));
    assert!(m.value("missing").is_none());
    assert!(m.levels.is_empty(), "values are not levels");
}

#[test]
fn named_values_are_checked() {
    for bad in ["", "Caps", "has space", "a/b", &"x".repeat(33)] {
        assert_eq!(
            b().value(bad, 1.0, None).build(TICK).unwrap_err(),
            LevelError::BadValueName(bad.to_string()),
            "{bad:?}"
        );
    }
    for good in ["rsi.14", "dd-ratio", "a_b", &"x".repeat(32)] {
        assert!(b().value(good, 1.0, None).build(TICK).is_ok(), "{good:?}");
    }
    assert_eq!(
        b().value("a", 1.0, None)
            .value("a", 2.0, None)
            .build(TICK)
            .unwrap_err(),
        LevelError::DuplicateValue("a".into())
    );
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            b().value("a", x, None).build(TICK).unwrap_err(),
            LevelError::ValueNotFinite("a".into())
        );
    }
    // One bad value refuses the whole map, levels and all.
    assert!(
        b().line("p", LevelValue::Tick(Price(600_000)), None, style())
            .value("a", f64::NAN, None)
            .build(TICK)
            .is_err()
    );
    let mut exactly = b();
    for i in 0..MAX_VALUES {
        exactly = exactly.value(&format!("v{i}"), 0.0, None);
    }
    assert!(exactly.build(TICK).is_ok());
    let mut many = b();
    for i in 0..=MAX_VALUES {
        many = many.value(&format!("v{i}"), 0.0, None);
    }
    assert_eq!(many.build(TICK).unwrap_err(), LevelError::TooMany("values"));
    // Too many duplicates: refused as too many, without checking every pair.
    let mut dup = b();
    for _ in 0..=MAX_VALUES {
        dup = dup.value("same", 0.0, None);
    }
    assert_eq!(dup.build(TICK).unwrap_err(), LevelError::TooMany("values"));
}

#[test]
fn a_values_as_of_falls_back_to_the_maps() {
    let own = t("2026-11-05T14:31:00");
    let m = b()
        .value("a", 1.0, Some(own))
        .value("b", 2.0, None)
        .build(TICK)
        .unwrap();
    assert_eq!(m.value_as_of("a"), Some(own));
    assert_eq!(m.value_as_of("b"), Some(t("2026-11-05T14:00:00")));
    assert_eq!(m.value_as_of("c"), None);
}

fn b() -> LevelMapBuilder {
    LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"))
}

fn series(key: &str, points: &[(&str, f64)], segments: &[(&str, &str)]) -> std::sync::Arc<Series> {
    std::sync::Arc::new(Series {
        key: key.into(),
        points: points.iter().map(|(a, v)| (t(a), *v)).collect(),
        segments: segments.iter().map(|(a, b)| (t(a), t(b))).collect(),
    })
}

/// Lower-pane series: a value holds from its point to the next or its segment's end; a gap
/// (outside every segment) is no value, never the last one carried over.
#[test]
fn series_values_hold_within_their_segments_and_gaps_stay_gaps() {
    let s = series(
        "osc.a",
        &[
            ("2026-11-05T14:00:00", 1.5),
            ("2026-11-05T14:10:00", 2.0),
            ("2026-11-05T15:00:00", -0.5),
        ],
        &[
            ("2026-11-05T14:00:00", "2026-11-05T14:30:00"),
            ("2026-11-05T15:00:00", "2026-11-05T15:20:00"),
        ],
    );
    assert_eq!(s.value_at(t("2026-11-05T13:59:00")), None, "before");
    assert_eq!(s.value_at(t("2026-11-05T14:05:00")), Some(1.5));
    assert_eq!(
        s.value_at(t("2026-11-05T14:30:00")),
        Some(2.0),
        "to the segment's end"
    );
    assert_eq!(s.value_at(t("2026-11-05T14:45:00")), None, "a gap");
    assert_eq!(s.value_at(t("2026-11-05T15:10:00")), Some(-0.5));
    assert_eq!(
        s.value_at(t("2026-11-05T15:30:00")),
        None,
        "after the last segment"
    );
}

#[test]
fn malformed_series_are_refused() {
    let ok = series(
        "osc.a",
        &[("2026-11-05T14:00:00", 1.0)],
        &[("2026-11-05T14:00:00", "2026-11-05T14:01:00")],
    );
    let map = |s: Vec<std::sync::Arc<Series>>| {
        s.into_iter()
            .fold(
                LevelMap::builder("demo", ES, t("2026-11-05T14:00:00")),
                |b, s| b.series(s),
            )
            .build(25)
    };
    assert_eq!(map(vec![ok.clone()]).unwrap().series.len(), 1);
    assert!(map(vec![ok.clone(), ok.clone()]).is_err(), "a repeated key");
    let bad_key = series("Res A", &[], &[]);
    assert!(map(vec![bad_key]).is_err());
    let unsorted = series(
        "osc.b",
        &[("2026-11-05T14:10:00", 1.0), ("2026-11-05T14:00:00", 2.0)],
        &[],
    );
    assert!(map(vec![unsorted]).is_err());
    let nan = series("osc.c", &[("2026-11-05T14:00:00", f64::NAN)], &[]);
    assert!(map(vec![nan]).is_err());
    let overlapping = series(
        "osc.d",
        &[],
        &[
            ("2026-11-05T14:00:00", "2026-11-05T14:30:00"),
            ("2026-11-05T14:20:00", "2026-11-05T14:40:00"),
        ],
    );
    assert!(map(vec![overlapping]).is_err());
}

fn es_format() -> PriceFormat {
    PriceFormat::new(
        crate::price::PriceScale::decimal(2),
        crate::price::DisplayStyle::Decimal { digits: 2 },
        TICK,
    )
}

/// A label in name-and-price mode reads `name : price` in the product's format: a line's
/// price, a raw value at the nearest price unit (not rounded to the tick), a zone's upper edge;
/// a VWAP-relative level keeps its name (its price moves); a level without a label has none.
#[test]
fn label_text_is_name_and_price() {
    let f = es_format();
    let m = LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"))
        .line(
            "demand",
            LevelValue::Tick(Price(755_625)),
            Some("Demand zone"),
            style(),
        )
        .line("dd", LevelValue::Raw(748_207.4), Some("DD"), style())
        .zone(
            "zone",
            LevelValue::Tick(Price(599_500)),
            LevelValue::Tick(Price(599_750)),
            Some("Z1"),
            style(),
        )
        .line(
            "vw",
            LevelValue::VwapOffset(750.0),
            Some("+1 Band"),
            style(),
        )
        .line("nolabel", LevelValue::Tick(Price(600_000)), None, style())
        .build(TICK)
        .unwrap();
    let texts: Vec<Option<String>> = m
        .levels
        .iter()
        .map(|l| label_text(l, LabelMode::NamePrice, LabelAt::Own, &f))
        .collect();
    assert_eq!(
        texts,
        vec![
            Some("Demand zone : 7556.25".into()),
            Some("DD : 7482.07".into()),
            Some("Z1 : 5997.50".into()),
            Some("+1 Band".into()),
            None,
        ]
    );
    assert_eq!(
        label_text(&m.levels[0], LabelMode::Name, LabelAt::Own, &f).as_deref(),
        Some("Demand zone")
    );
    for mode in [LabelMode::Name, LabelMode::NamePrice] {
        assert_eq!(LabelMode::from_key(mode.key()), Some(mode));
    }
    assert_eq!(LabelMode::from_key("price-only"), None);
    assert_eq!(LabelMode::default(), LabelMode::Name);
}

/// Labels are placed by height alone (they are right-aligned at the plot's edge, so any two at
/// the same height overlap whatever their length): a longer `name : price` label five pixels
/// from another stacks a row away, never drawn over it; with both nearby rows taken it is
/// dropped, and the level stays drawn.
#[test]
fn close_long_labels_give_way() {
    let m = LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"))
        .line(
            "open",
            LevelValue::Tick(Price(600_000)),
            Some("Open : 6000.00"),
            style(),
        )
        .line(
            "mid",
            LevelValue::Tick(Price(600_025)),
            Some("Mid line : 6000.25"),
            style(),
        )
        .build(TICK)
        .unwrap();
    let mut out = Vec::with_capacity(8);
    place(&m, &vp(), None, |_| true, &mut out);
    assert_eq!(out.len(), 2);
    place_labels(&mut out, 17.6, 400.0, None, &mut [LabelSeen::New; 2], |i| {
        m.levels[i as usize].label.is_some()
    });
    let (a, b) = (out[0].label_y.unwrap(), out[1].label_y.unwrap());
    assert_eq!(a, 200.0);
    assert!(
        (a - b).abs() >= 17.6 - 1e-3,
        "{a} {b}: not drawn over the first"
    );
    assert!(
        (b - out[1].top).abs() <= 17.6,
        "at most a row from its line"
    );
    // Rows above and below taken (labels a row from the first either way): dropped.
    let m = LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"))
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .line("a", LevelValue::Raw(600_090.0), Some("B"), style()) // 18 px up
        .line("a", LevelValue::Raw(599_910.0), Some("C"), style()) // 18 px down
        .line("a", LevelValue::Raw(600_025.0), Some("D"), style()) // 5 px up
        .build(TICK)
        .unwrap();
    place(&m, &vp(), None, |_| true, &mut out);
    place_labels(
        &mut out,
        17.6,
        400.0,
        None,
        &mut [LabelSeen::New; 4],
        |_| true,
    );
    assert!(out[..3].iter().all(|p| p.label_y == Some(p.top)));
    assert_eq!(out[3].label_y, None, "no free row within one: dropped");
}

/// The source's `TypeStyle::OWN` is the look before types: own color, solid, 1 px, zones at
/// the 25 % that is 10 %.
#[test]
fn own_type_style_is_todays_look() {
    let s = TypeStyle::OWN;
    assert!(s.shown);
    assert_eq!(s.line.color, Color::Own);
    assert_eq!(
        (s.line.line, s.line.width, s.line.opacity),
        (LineType::Solid, 1, 100)
    );
    assert_eq!((s.fill.color, s.fill.opacity), (Color::Own, 25));
    let z = LevelType::zone("demand", "Demand zone", "Zones", s);
    assert!(z.zone && !LevelType::line("hp", "HP", "Lines", s).zone);
}

/// Labels never move for the price: placement takes no price at all, so a frame's layout is
/// only the levels' and the viewport's. A determinism check: with the levels and the viewport
/// fixed, 400 frames in a row (the frames a last price sweeping the plot would draw) leave
/// every label where it was. The layer's own test drives a real moving price.
#[test]
fn the_label_layout_is_deterministic_frame_to_frame() {
    let mut b = LevelMap::builder("demo", ES, t("2026-11-05T14:00:00"));
    // Labeled levels a few ticks apart, some closer than a label's height.
    for (i, p) in [
        600_000, 600_025, 600_100, 600_150, 600_175, 600_400, 599_800,
    ]
    .into_iter()
    .enumerate()
    {
        b = b.line(
            "k",
            LevelValue::Tick(Price(p)),
            Some(&format!("L{i}")),
            style(),
        );
    }
    let m = b.build(TICK).unwrap();
    let mut out = Vec::with_capacity(16);
    let mut shown = vec![LabelSeen::New; m.levels.len()];
    place(&m, &vp(), None, |_| true, &mut out);
    place_labels(&mut out, 17.6, 400.0, None, &mut shown, |i| {
        m.levels[i as usize].label.is_some()
    });
    let first: Vec<Option<f32>> = out.iter().map(|p| p.label_y).collect();
    assert!(first.iter().filter(|y| y.is_some()).count() >= 4);
    // Frame after frame (with its hysteresis state carried): the same layout.
    for step in 0..400 {
        place(&m, &vp(), None, |_| true, &mut out);
        place_labels(&mut out, 17.6, 400.0, None, &mut shown, |i| {
            m.levels[i as usize].label.is_some()
        });
        let now: Vec<Option<f32>> = out.iter().map(|p| p.label_y).collect();
        assert_eq!(now, first, "frame {step}");
    }
}

/// Hysteresis: two labeled levels whose separation wobbles around the clash distance by less
/// than the margin keep one layout (whichever they had), and a moving band's label never
/// changes which fixed labels show.
#[test]
fn close_labels_keep_one_layout() {
    let h = 16.0;
    let margin = h * LABEL_HYSTERESIS;
    // A pair shown apart, then pulled to just inside and outside the clash distance, less
    // than the margin each way: shown apart stays apart until it really overlaps.
    let pair = |sep_px: f32| {
        // vp(): 5 units/px; a level 200 px down is 600_000.
        let lo = 600_000.0 - f64::from(sep_px) * 5.0;
        LevelMap::builder("demo", ES, UtcMillis(0))
            .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
            .line("a", LevelValue::Raw(lo), Some("B"), style())
            .build(TICK)
            .unwrap()
    };
    let mut out = Vec::with_capacity(4);
    let run = |sep: f32, out: &mut Vec<Placed>, shown: &mut [LabelSeen; 2]| {
        let m = pair(sep);
        place(&m, &vp(), None, |_| true, out);
        place_labels(out, h, 400.0, None, shown, |i| {
            m.levels[i as usize].label.is_some()
        });
        // At its own line (else stacked a row away).
        out[1].label_y == Some(out[1].top)
    };
    // Hidden (too close), then wobbling between h - margin/2 and h + margin/2: stays hidden.
    let mut shown = [LabelSeen::New; 2];
    assert!(!run(h - 4.0, &mut out, &mut shown));
    for k in 0..20 {
        let sep = h + if k % 2 == 0 {
            margin * 0.5
        } else {
            -margin * 0.5
        };
        assert!(
            !run(sep, &mut out, &mut shown),
            "hidden stays hidden ({sep})"
        );
    }
    // Clear by more than the margin: shows; then wobbling above the clash distance: stays.
    assert!(run(h + margin + 1.0, &mut out, &mut shown));
    for k in 0..20 {
        let sep = h + if k % 2 == 0 { margin * 0.5 } else { 0.1 };
        assert!(run(sep, &mut out, &mut shown), "shown stays shown ({sep})");
    }
    // Overlapping for real: gives way (stacked a row away).
    assert!(!run(h - 1.0, &mut out, &mut shown));

    // A VWAP-relative label near a fixed one: placed after it, so the fixed label shows
    // whatever the band does.
    let m = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("vw", LevelValue::VwapOffset(0.0), Some("+1 Band"), style())
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .build(TICK)
        .unwrap();
    let mut shown = [LabelSeen::New; 2];
    for vwap in [599_990.0, 600_000.0, 600_010.0, 600_500.0, 600_005.0] {
        place(&m, &vp(), Some(vwap), |_| true, &mut out);
        place_labels(&mut out, h, 400.0, None, &mut shown, |i| {
            m.levels[i as usize].label.is_some()
        });
        let fixed = out.iter().find(|p| p.index == 1).unwrap();
        assert_eq!(fixed.label_y, Some(200.0), "VWAP at {vwap}");
    }
}

/// A label that can't be drawn (too wide for the plot) claims no room, so a neighbor shows;
/// a plot shorter than a label shows none.
#[test]
fn an_undrawable_label_claims_no_room() {
    let m = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .line("a", LevelValue::Raw(599_975.0), Some("B"), style())
        .build(TICK)
        .unwrap();
    let mut out = Vec::with_capacity(4);
    place(&m, &vp(), None, |_| true, &mut out);
    let mut seen = [LabelSeen::New; 2];
    place_labels(&mut out, 16.0, 400.0, None, &mut seen, |i| i != 0);
    assert_eq!((out[0].label_y, out[1].label_y), (None, Some(205.0)));
    assert_eq!(seen, [LabelSeen::New, LabelSeen::Shown]);
    place_labels(&mut out, 16.0, 12.0, None, &mut seen, |_| true);
    assert!(out.iter().all(|p| p.label_y.is_none()));
}

/// A tag in the price axis never covers the last-price tag (the blocked span): one on it
/// stacks just off it, nearest first; labels still never overlap each other; nothing blocked
/// leaves them home.
#[test]
fn labels_stack_off_the_blocked_span() {
    let m = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .line("a", LevelValue::Raw(599_975.0), Some("B"), style())
        .build(TICK)
        .unwrap();
    let mut out = Vec::with_capacity(4);
    place(&m, &vp(), None, |_| true, &mut out);
    let (h, tag_h) = (16.0, 18.0);
    let a_y = out[0].top;
    let mut seen = [LabelSeen::New; 2];
    place_labels(&mut out, h, 400.0, Some((a_y, tag_h)), &mut seen, |_| true);
    let ys: Vec<f32> = out.iter().filter_map(|p| p.label_y).collect();
    assert!(!ys.is_empty());
    for y in &ys {
        assert!(
            (y - a_y).abs() >= (h + tag_h) * 0.5 - 1e-3,
            "{y} on the last price at {a_y}"
        );
    }
    for (i, y) in ys.iter().enumerate() {
        for z in &ys[i + 1..] {
            assert!((y - z).abs() >= h - 1e-3, "{y} and {z} overlap");
        }
    }
    // The nearest row off it: just above or just below, not further.
    let a = out[0].label_y.unwrap();
    assert!(((a - a_y).abs() - (h + tag_h) * 0.5).abs() < 1e-3, "{a}");
    // Nothing blocked: home.
    let mut seen = [LabelSeen::New; 2];
    place_labels(&mut out, h, 400.0, None, &mut seen, |i| i == 0);
    assert_eq!(out[0].label_y, Some(a_y));
}

/// A last price sweeping across tags in the axis, half a px a frame: at every step no tag
/// covers it and no two tags overlap.
#[test]
fn a_sweeping_last_price_never_covers_a_tag() {
    let m = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(600_100.0), Some("A"), style())
        .line("a", LevelValue::Raw(600_000.0), Some("B"), style())
        .line("a", LevelValue::Raw(599_950.0), Some("C"), style())
        .build(TICK)
        .unwrap();
    let mut out = Vec::with_capacity(4);
    place(&m, &vp(), None, |_| true, &mut out);
    let (h, tag_h) = (16.0, 18.0);
    let mut seen = [LabelSeen::New; 3];
    let at = |out: &mut Vec<Placed>, seen: &mut [LabelSeen; 3], b: f32| {
        place_labels(out, h, 400.0, Some((b, tag_h)), seen, |_| true);
        let ys: Vec<f32> = out.iter().filter_map(|p| p.label_y).collect();
        for y in &ys {
            assert!((y - b).abs() >= (h + tag_h) * 0.5 - 1e-3, "{y} under {b}");
        }
        for (i, y) in ys.iter().enumerate() {
            for z in &ys[i + 1..] {
                assert!((y - z).abs() >= h - 1e-3, "{y} {z} at {b}");
            }
        }
    };
    let mut b = 150.0;
    while b < 260.0 {
        at(&mut out, &mut seen, b);
        b += 0.5;
    }
}

/// A name too long for the room is cut with "…" before the price; the price is never cut.
#[test]
fn long_names_are_cut_before_the_price() {
    let mono = |t: &str| t.chars().count() as f32 * 6.0;
    let cut = cut_label("Previous session close", Some("7517.25"), 120.0, mono).unwrap();
    assert!(cut.ends_with("… : 7517.25"), "{cut}");
    assert!(mono(&cut) <= 120.0);
    assert!(cut.starts_with("Prev"));
    assert_eq!(
        cut_label("Upper band", None, 36.0, mono).as_deref(),
        Some("Upper…")
    );
    // Not even a letter with the price: none (the price is never cut).
    assert_eq!(cut_label("Mid line", Some("7517.25"), 40.0, mono), None);
}

/// A label clashing only with another label stacks at most a label's height away, even while
/// a blocked span elsewhere lets rows beside it reach further.
#[test]
fn only_rows_beside_the_blocked_span_reach_further() {
    let m = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(600_100.0), Some("A"), style())
        .line("a", LevelValue::Raw(600_075.0), Some("B"), style())
        .build(TICK)
        .unwrap();
    let mut out = Vec::with_capacity(4);
    place(&m, &vp(), None, |_| true, &mut out);
    let h = 16.0;
    let mut seen = [LabelSeen::New; 2];
    // The blocked span far away (at the bottom of the plot).
    place_labels(&mut out, h, 400.0, Some((390.0, 18.0)), &mut seen, |_| true);
    let (a, b) = (out[0].label_y.unwrap(), out[1].label_y.unwrap());
    assert!((a - b).abs() >= h - 1e-3);
    assert!(
        (b - out[1].top).abs() <= h + 1e-3,
        "{b} from {}",
        out[1].top
    );
}

/// A type's label edge: just under a zone's low edge (with the low edge's price), just above
/// its high edge, or on its own price (a zone's high edge, as before).
#[test]
fn zone_labels_sit_at_their_types_edge() {
    let m = demo();
    let mut out = Vec::with_capacity(8);
    let half = 8.0;
    for (at, want) in [
        (LabelAt::Own, 250.0),
        (LabelAt::High, 250.0 - half),
        (LabelAt::Low, 300.0 + half),
    ] {
        place(&m, &vp(), None, |k| k == 1, &mut out);
        out[0].anchor = at;
        place_labels(
            &mut out,
            16.0,
            400.0,
            None,
            &mut [LabelSeen::New; 3],
            |_| true,
        );
        assert_eq!(out[0].label_y, Some(want), "{at:?}");
    }
    let f = es_format();
    let z = &m.levels[1];
    assert_eq!(
        label_text(z, LabelMode::NamePrice, LabelAt::Low, &f).as_deref(),
        Some("Z1 : 5995.00")
    );
    assert_eq!(
        label_text(z, LabelMode::NamePrice, LabelAt::High, &f).as_deref(),
        Some("Z1 : 5997.50")
    );
    for at in LabelAt::ALL {
        assert_eq!(LabelAt::from_key(at.key()), Some(at));
    }
    for c in LabelColor::ALL {
        assert_eq!(LabelColor::from_key(c.key()), Some(c));
    }
    assert_eq!(TypeStyle::OWN.label_at, LabelAt::Own);
    assert_eq!(TypeStyle::OWN.label, LabelColor::Level);
}

/// A clashing zone-edge label stacks a row further from its zone (a low-edge label further
/// down), never to the zone's other edge where there is room, and is dropped when that row is
/// taken too; with hysteresis it comes back to its own edge once clear.
#[test]
fn a_clashing_edge_label_stacks_away_never_to_the_other_edge() {
    let m = |lines: &[f64]| {
        let mut b = LevelMap::builder("demo", ES, UtcMillis(0)).zone(
            "zone",
            LevelValue::Tick(Price(599_500)),
            LevelValue::Tick(Price(599_750)),
            Some("Z"),
            style(),
        );
        for p in lines {
            b = b.line("a", LevelValue::Raw(*p), Some("A"), style());
        }
        b.build(TICK).unwrap()
    };
    let mut out = Vec::with_capacity(8);
    let mut seen = [LabelSeen::New; 3];
    // Lines are placed first in map order here: the zone is level 0, so place the zone last.
    let run = |m: &LevelMap, out: &mut Vec<Placed>, seen: &mut [LabelSeen; 3]| {
        place(m, &vp(), None, |_| true, out);
        out.rotate_left(1);
        out.last_mut().unwrap().anchor = LabelAt::Low;
        place_labels(out, 16.0, 400.0, None, seen, |_| true);
        out.last().unwrap().label_y
    };
    // The line at 304 px (599_480) sits where the zone's low-edge label (308) would: the
    // zone's label goes a row under the line (320), not to its high edge (242).
    let z = run(&m(&[599_480.0]), &mut out, &mut seen);
    assert_eq!(out[0].label_y, Some(304.0));
    assert_eq!(z, Some(320.0), "a row further down");
    assert_eq!(seen[0], LabelSeen::Hidden);
    // A second line under it takes that row: dropped, never moved up to its high edge.
    let z = run(&m(&[599_480.0, 599_400.0]), &mut out, &mut seen);
    assert_eq!(z, None, "dropped, not moved to the high edge");
    // The line moves away a little: within the hysteresis band, still off its edge.
    let z = run(&m(&[599_400.0]), &mut out, &mut seen); // 320 px: 12 px from 308
    assert_ne!(z, Some(308.0));
    let z = run(&m(&[599_300.0]), &mut out, &mut seen); // 340 px: clear by more than 20
    assert_eq!(z, Some(308.0), "back at its own edge");
}

/// A legend's title and stats are validated with the map: a signed tone needs its sign, and
/// lengths and counts are bounded. A bad legend is dropped (`legend_dropped`), never the map:
/// its levels are kept.
#[test]
fn legend_is_validated_with_the_map() {
    let stat = |name: &str, text: &str, tone| LegendStat {
        name: name.into(),
        text: text.into(),
        tone,
    };
    let b = |l: LevelLegend| {
        LevelMap::builder("demo", ES, UtcMillis(0))
            .line(
                "pivot",
                LevelValue::Tick(Price(600_000)),
                Some("P"),
                LevelStyle::default(),
            )
            .legend(l)
            .build(TICK)
    };
    let good = LevelLegend {
        title: Some("Map A".into()),
        stats: vec![
            stat("Range", "72.25", StatTone::Plain),
            stat("Bias", "\u{2212}7.9", StatTone::Down),
            stat("Skew", "-1", StatTone::Down),
            stat("Drift", "+11.0", StatTone::Up),
        ],
    };
    assert_eq!(good.check(), Ok(()));
    let m = b(good.clone()).unwrap();
    assert_eq!(m.legend, Some(good));
    assert!(!m.legend_dropped);
    let mut bads: Vec<LevelLegend> = [
        vec![stat("Bias", "7.9", StatTone::Down)],
        vec![stat("Bias", "-7.9", StatTone::Up)],
        vec![stat("", "1", StatTone::Plain)],
        vec![stat("R", "", StatTone::Plain)],
        vec![stat("R", "1234567890123", StatTone::Plain)],
        vec![stat("R", "1", StatTone::Plain); MAX_LEGEND_STATS + 1],
    ]
    .into_iter()
    .map(|stats| LevelLegend { title: None, stats })
    .collect();
    bads.push(LevelLegend {
        title: Some("x".repeat(MAX_LEGEND_TEXT + 1)),
        stats: Vec::new(),
    });
    for bad in bads {
        assert_eq!(bad.check(), Err(LevelError::BadLegend), "{bad:?}");
        let m = b(bad.clone()).expect("the map is kept");
        assert_eq!(m.legend, None, "{bad:?}");
        assert!(m.legend_dropped, "{bad:?}");
        assert_eq!(m.levels.len(), 1, "its levels kept");
    }
    // No legend: none.
    assert_eq!(demo().legend, None);
    assert!(!demo().legend_dropped);
}

/// A higher-priority label claims its row first whatever its order: a Low label listed first
/// gives way to a Normal one at the same price, and a plot with room for one shows the Normal.
#[test]
fn labels_place_by_priority_first() {
    let at = |index: u32, rank: u8| Placed {
        index,
        top: 100.0,
        bottom: 100.0,
        label_y: None,
        vwap_relative: false,
        anchor: LabelAt::Own,
        on_edge: false,
        rank,
    };
    let low = LabelPriority::Low.rank();
    let normal = LabelPriority::Normal.rank();
    let mut out = vec![at(0, low), at(1, normal)];
    place_labels(
        &mut out,
        16.0,
        400.0,
        None,
        &mut [LabelSeen::New; 2],
        |_| true,
    );
    assert_eq!(out[1].label_y, Some(100.0), "Normal on its own row");
    assert!(
        out[0]
            .label_y
            .is_some_and(|y| (y - 100.0).abs() >= 16.0 - 1e-3)
    );
    // Room for one label only: the Normal one.
    let mut out = vec![at(0, low), at(1, normal)];
    for p in &mut out {
        (p.top, p.bottom) = (8.0, 8.0);
    }
    place_labels(&mut out, 16.0, 16.0, None, &mut [LabelSeen::New; 2], |_| {
        true
    });
    assert_eq!(out[0].label_y, None, "Low hidden");
    assert!(out[1].label_y.is_some(), "Normal shown");
    assert!(LabelPriority::High.rank() < normal && normal < low);
    assert_eq!(LabelPriority::default(), LabelPriority::Normal);
}
