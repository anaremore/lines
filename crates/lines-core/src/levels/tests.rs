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
    let mut shown = [false; 8];
    place(&m, &vp(), None, |_| true, &mut out);
    // Labels on their levels (the unlabelled raw line gets none).
    place_labels(&mut out, &m, 16.0, 400.0, &mut shown);
    assert_eq!(
        out.iter().map(|p| p.label_y).collect::<Vec<_>>(),
        [Some(200.0), Some(250.0), None]
    );
    assert_eq!(&shown[..3], &[true, true, false]);
    // Two labels within one label height: the second is dropped (its level still drawn).
    let close = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .line("a", LevelValue::Raw(599_975.0), Some("B"), style()) // 5 px lower
        .build(TICK)
        .unwrap();
    place(&close, &vp(), None, |_| true, &mut out);
    place_labels(&mut out, &close, 16.0, 400.0, &mut [false; 2]);
    assert_eq!(out.len(), 2);
    assert_eq!((out[0].label_y, out[1].label_y), (Some(200.0), None));
    // Near the top edge: clamped inside the plot.
    let top = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("a", LevelValue::Raw(601_000.0), Some("T"), style())
        .build(TICK)
        .unwrap();
    place(&top, &vp(), None, |_| true, &mut out);
    place_labels(&mut out, &top, 16.0, 400.0, &mut [false; 1]);
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
            "zone-bull",
            LevelValue::Tick(Price(755_625)),
            Some("Bull Zone"),
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
        .line("vw", LevelValue::VwapOffset(750.0), Some("+1 RI"), style())
        .line("nolabel", LevelValue::Tick(Price(600_000)), None, style())
        .build(TICK)
        .unwrap();
    let texts: Vec<Option<String>> = m
        .levels
        .iter()
        .map(|l| label_text(l, LabelMode::NamePrice, &f))
        .collect();
    assert_eq!(
        texts,
        vec![
            Some("Bull Zone : 7556.25".into()),
            Some("DD : 7482.07".into()),
            Some("Z1 : 5997.50".into()),
            Some("+1 RI".into()),
            None,
        ]
    );
    assert_eq!(
        label_text(&m.levels[0], LabelMode::Name, &f).as_deref(),
        Some("Bull Zone")
    );
    for mode in [LabelMode::Name, LabelMode::NamePrice] {
        assert_eq!(LabelMode::from_key(mode.key()), Some(mode));
    }
    assert_eq!(LabelMode::from_key("price-only"), None);
    assert_eq!(LabelMode::default(), LabelMode::Name);
}

/// Labels are placed by height alone (they are right-aligned at the plot's edge, so any two at
/// the same height overlap whatever their length): a longer `name : price` label two pixels
/// from another is still dropped, never drawn over it, and the level stays drawn.
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
            "half-gap",
            LevelValue::Tick(Price(600_025)),
            Some("Half Gap : 6000.25"),
            style(),
        )
        .build(TICK)
        .unwrap();
    let mut out = Vec::with_capacity(8);
    place(&m, &vp(), None, |_| true, &mut out);
    assert_eq!(out.len(), 2);
    place_labels(&mut out, &m, 17.6, 400.0, &mut [false; 2]);
    assert!(out[0].label_y.is_some());
    assert_eq!(out[1].label_y, None, "dropped, not drawn over the first");
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
    let z = LevelType::zone("zone-bull", "Bull Zone", "Zones", s);
    assert!(z.zone && !LevelType::line("hp", "HP", "Lines", s).zone);
}

/// Labels never move for the price: with the levels and the viewport fixed, a last price
/// sweeping across every labeled level (the old `avoid` row) leaves every label where it was,
/// frame after frame. (Placement takes no price at all; this pins that a frame's layout is
/// only the levels' and the viewport's.)
#[test]
fn a_moving_price_never_moves_a_label() {
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
    let mut shown = vec![false; m.levels.len()];
    place(&m, &vp(), None, |_| true, &mut out);
    place_labels(&mut out, &m, 17.6, 400.0, &mut shown);
    let first: Vec<Option<f32>> = out.iter().map(|p| p.label_y).collect();
    assert!(first.iter().filter(|y| y.is_some()).count() >= 4);
    // The last price walks the whole plot, one tick a frame; nothing it can reach moves.
    for step in 0..400 {
        let _last_price = 599_000 + step * TICK / 5;
        place(&m, &vp(), None, |_| true, &mut out);
        place_labels(&mut out, &m, 17.6, 400.0, &mut shown);
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
    let run = |sep: f32, out: &mut Vec<Placed>, shown: &mut [bool; 2]| {
        let m = pair(sep);
        place(&m, &vp(), None, |_| true, out);
        place_labels(out, &m, h, 400.0, shown);
        out[1].label_y.is_some()
    };
    // Hidden (too close), then wobbling between h - margin/2 and h + margin/2: stays hidden.
    let mut shown = [false; 2];
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
    // Overlapping for real: gives way.
    assert!(!run(h - 1.0, &mut out, &mut shown));

    // A VWAP-relative label near a fixed one: placed after it, so the fixed label shows
    // whatever the band does.
    let m = LevelMap::builder("demo", ES, UtcMillis(0))
        .line("vw", LevelValue::VwapOffset(0.0), Some("+1 RI"), style())
        .line("a", LevelValue::Raw(600_000.0), Some("A"), style())
        .build(TICK)
        .unwrap();
    let mut shown = [false; 2];
    for vwap in [599_990.0, 600_000.0, 600_010.0, 600_500.0, 600_005.0] {
        place(&m, &vp(), Some(vwap), |_| true, &mut out);
        place_labels(&mut out, &m, h, 400.0, &mut shown);
        let fixed = out.iter().find(|p| p.index == 1).unwrap();
        assert_eq!(fixed.label_y, Some(200.0), "VWAP at {vwap}");
    }
}
