use super::*;
use crate::versioned::{Decoded, decode};
use lines_core::drawings::Style;
use lines_core::style::{Color, FillStyle, LineStyle, LineType, Rgb, TextSize, TextStyle};
use lines_feed::catalog;

fn es() -> Product {
    catalog::get(catalog::ES).unwrap()
}

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn sample() -> Vec<Drawing> {
    let a = Anchor {
        time: t("2026-11-05T14:07:00Z"),
        price: Price(600_100),
    };
    let b = Anchor {
        time: t("2026-11-05T15:22:00Z"),
        price: Price(600_500),
    };
    let mut trend = Drawing::new(
        Tool::Trend,
        a,
        b,
        Style {
            line: LineStyle {
                color: Color::Palette(1),
                width: 2,
                line: LineType::Dashed,
                ..LineStyle::DEFAULT
            },
            // A zone's legacy fill follows its line (unused on a trend line, but saved alike).
            fill: FillStyle {
                color: Color::Palette(1),
                ..FillStyle::DEFAULT
            },
            ..Style::DEFAULT
        },
    );
    trend.id = 1;
    let mut note = Drawing::new(Tool::Note, a, a, Style::DEFAULT);
    note.id = 2;
    note.text = "Opening drive".into();
    let mut hline = Drawing::new(Tool::HLine, b, b, Style::DEFAULT);
    hline.id = 5;
    vec![trend, note, hline]
}

/// Version 1, pinned: this exact text must keep loading as these drawings.
const V1: &str = r#"{
  "version": 1,
  "product": "ES",
  "drawings": [
    {
      "id": 1,
      "tool": "trend",
      "a": { "time": "2026-11-05T14:07:00.000Z", "price": 600100 },
      "b": { "time": "2026-11-05T15:22:00.000Z", "price": 600500 },
      "color": 1,
      "width": 2,
      "dashed": true
    },
    {
      "id": 2,
      "tool": "note",
      "a": { "time": "2026-11-05T14:07:00.000Z", "price": 600100 },
      "text": "Opening drive"
    },
    {
      "id": 5,
      "tool": "hline",
      "a": { "time": "2026-11-05T15:22:00.000Z", "price": 600500 }
    }
  ]
}"#;

#[test]
fn version_1_is_pinned() {
    let Decoded::Ok {
        value,
        migrated_from,
    } = decode::<DrawingsFile>(&FORMAT, V1.as_bytes())
    else {
        panic!("v1 must load");
    };
    assert_eq!(migrated_from, None);
    let (items, skipped) = value.to_drawings(25);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(items, sample());
    // And it is written back as the same document.
    let again: serde_json::Value = serde_json::from_slice(
        &DrawingsFile::from_drawings(&es(), &items, &HashMap::new()).encode(),
    )
    .unwrap();
    let pinned: serde_json::Value = serde_json::from_str(V1).unwrap();
    assert_eq!(again, pinned);
}

/// Files from before versioning (no `version`) migrate to v1 unchanged.
#[test]
fn version_0_migrates() {
    let v0 = V1.replace("\"version\": 1,\n", "");
    let Decoded::Ok {
        value,
        migrated_from,
    } = decode::<DrawingsFile>(&FORMAT, v0.as_bytes())
    else {
        panic!("v0 must load");
    };
    assert_eq!(migrated_from, Some(0));
    assert_eq!(value.to_drawings(25).0, sample());
    let newer = V1.replace("\"version\": 1", "\"version\": 9");
    assert!(matches!(
        decode::<DrawingsFile>(&FORMAT, newer.as_bytes()),
        Decoded::Newer { found: 9 }
    ));
}

/// Unusable drawings are skipped with a reason, never repaired: an off-grid price is not
/// rounded; unknown tools and missing anchors are refused; ids stay unique.
#[test]
fn unusable_drawings_are_skipped_and_reported() {
    let mut f: DrawingsFile = serde_json::from_str(V1).unwrap();
    f.drawings[0].a.price = 600_110;
    f.drawings[1].tool = "spiral".into();
    f.drawings[2].id = 2; // duplicate of the (now skipped) note: kept, it's the first 2
    let mut extra = f.drawings[2].clone();
    extra.id = 3;
    extra.tool = "rect".into();
    extra.b = None;
    f.drawings.push(extra);
    let (items, skipped) = f.to_drawings(25);
    assert_eq!(items.iter().map(|d| d.id).collect::<Vec<_>>(), [2]);
    assert_eq!(skipped.len(), 3, "{skipped:?}");
    assert!(skipped[0].contains("off the tick grid"));
    assert!(skipped[1].contains("unknown tool"));
    assert!(skipped[2].contains("second anchor"));
}

/// Entries this version can't use are never lost: every save writes them back unchanged, and
/// new drawings don't reuse their ids.
#[test]
fn unusable_drawings_survive_saves() {
    let dir = lines_core::scratch::Scratch::new("drawings-keep");
    let mut f: DrawingsFile = serde_json::from_str(V1).unwrap();
    f.drawings[1].tool = "spiral".into(); // id 2: unusable
    f.drawings[1].id = 9;
    let file = path(&dir, &es());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, f.encode()).unwrap();
    let writer = Writer::spawn();
    let mut g = Drawings::new(Some(dir.to_path_buf()), Some(writer.clone()));
    let (p, report) = g.product(&es());
    assert!(report.unwrap().contains("kept in the file"));
    let id = p.change(|s| (true, s.add(sample()[2].clone())));
    assert_eq!(id, 10, "ids continue above the kept entry's");
    assert!(writer.flush(std::time::Duration::from_secs(5)));
    let saved: DrawingsFile = match decode(&FORMAT, &std::fs::read(&file).unwrap()) {
        Decoded::Ok { value, .. } => value,
        other => panic!("{other:?}"),
    };
    let spiral = saved.drawings.iter().find(|d| d.id == 9).unwrap();
    assert_eq!(spiral, &f.drawings[1], "written back exactly as read");
    assert_eq!(saved.drawings.len(), 4);
}

#[test]
fn files_are_named_by_product() {
    let dir = Path::new("cfg");
    assert_eq!(path(dir, &es()), dir.join("drawings").join("ES.json"));
    let btc = catalog::all()
        .into_iter()
        .find(|p| p.symbol == "BTC/USD.KR")
        .unwrap();
    assert_eq!(
        path(dir, &btc),
        dir.join("drawings").join("BTC-USD.KR.json")
    );
}

/// Every change is saved through the writer and read back on the next start; the revision
/// moves so other charts of the product repaint.
#[test]
fn changes_are_saved_and_restored() {
    let dir = lines_core::scratch::Scratch::new("drawings");
    let writer = Writer::spawn();
    let mut g = Drawings::new(Some(dir.to_path_buf()), Some(writer.clone()));
    let (p, report) = g.product(&es());
    assert!(report.is_none());
    assert!(p.lock().items().is_empty());
    let rev = p.revision();
    for d in sample() {
        p.change(|s| {
            s.add(d);
            (true, ())
        });
    }
    assert!(p.revision() > rev);
    p.change(|s| (s.undo(), ()));
    assert!(writer.flush(std::time::Duration::from_secs(5)));

    let mut again = Drawings::new(Some(dir.to_path_buf()), None);
    let (q, report) = again.product(&es());
    assert!(report.is_none(), "{report:?}");
    let ids: Vec<u64> = q.lock().items().iter().map(|d| d.id).collect();
    assert_eq!(ids, [1, 2], "the undone third drawing isn't saved");
    // The same Arc for every chart of the product.
    assert!(Arc::ptr_eq(&again.product(&es()).0, &q));
}

#[test]
fn a_seed_file_names_its_product() {
    let dir = lines_core::scratch::Scratch::new("drawings-seed");
    let f = dir.join("seed.json");
    std::fs::write(&f, V1).unwrap();
    let (p, items) = read_seed(&f).unwrap();
    assert_eq!(p.id, catalog::ES);
    assert_eq!(items.len(), 3);
    std::fs::write(&f, V1.replace("\"ES\"", "\"XX\"")).unwrap();
    assert!(read_seed(&f).unwrap_err().contains("unknown product"));
}

/// Every style field survives a save and a restart; a file written before styles loads with
/// the defaults (above: `version_1_is_pinned`).
#[test]
fn styles_round_trip_through_the_file() {
    let mut d = sample();
    d[0].style = Style {
        line: LineStyle {
            color: Color::Custom(Rgb::hex(0xe5484d)),
            line: LineType::Dotted,
            width: 4,
            opacity: 50,
        },
        fill: FillStyle {
            color: Color::Palette(3),
            opacity: 40,
        },
        extend_left: true,
        extend_right: true,
        ..Style::DEFAULT
    };
    d[1].style.text = TextStyle {
        size: TextSize::Large,
        color: Color::Palette(1),
        background: false,
    };
    d[2].style.axis_tag = true;
    let bytes = DrawingsFile::from_drawings(&es(), &d, &HashMap::new()).encode();
    let Decoded::Ok { value, .. } = decode::<DrawingsFile>(&FORMAT, &bytes) else {
        panic!("must load");
    };
    let (items, skipped) = value.to_drawings(25);
    assert!(skipped.is_empty());
    assert_eq!(items, d);
    // Older builds still read a dashed line in palette color 0.
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let first = &raw["drawings"][0];
    assert_eq!(first["dashed"], true);
    assert_eq!(first["line"], "dotted");
    assert_eq!(first["line_color"], "#e5484d");
    assert!(first.get("color").is_none());
}

/// A newer build's fields, on a drawing and on the file, survive this build's saves even when
/// the drawing is edited here.
#[test]
fn a_newer_files_unknown_fields_survive() {
    let dir = lines_core::scratch::Scratch::new("drawings-newer");
    let mut v: serde_json::Value = serde_json::from_str(V1).unwrap();
    v["drawings"][0]["glow"] = serde_json::json!({"radius": 3});
    v["drawings"][0]["label_font"] = "serif".into();
    v["layers"] = serde_json::json!(["a", "b"]);
    let file = path(&dir, &es());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
    let writer = Writer::spawn();
    let mut g = Drawings::new(Some(dir.to_path_buf()), Some(writer.clone()));
    let (p, report) = g.product(&es());
    assert!(report.is_none(), "{report:?}");
    // Edit the drawing that carries them: restyled here, its unknown fields stay.
    p.change(|s| {
        let mut d = s.get(1).unwrap().clone();
        d.style.line.width = 3;
        (s.edit(d), ())
    });
    assert!(writer.flush(std::time::Duration::from_secs(5)));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let first = &saved["drawings"][0];
    assert_eq!(first["id"], 1);
    assert_eq!(first["width"], 3);
    assert_eq!(first["glow"], serde_json::json!({"radius": 3}));
    assert_eq!(first["label_font"], "serif");
    assert_eq!(saved["layers"], serde_json::json!(["a", "b"]));
    assert_eq!(saved["version"], 1);
    // Fields this build knows are never duplicated into the unknown ones.
    let doc: DrawingsFile = serde_json::from_value(saved).unwrap();
    assert!(!doc.drawings[0].extra.contains_key("width"));
    assert!(!doc.drawings[0].extra.contains_key("tool"));
}

/// A brush stroke and an arrow round-trip through the file: the stroke's points in order (its
/// optional `points`), the arrowhead (an optional `arrow_end`); a file with neither (older
/// drawings) loads as before. A stroke with more points than kept, or one point, is skipped,
/// reported and kept as written.
#[test]
fn brush_strokes_and_arrows_round_trip() {
    let p = |m: i64, price: i64| Anchor {
        time: UtcMillis(t("2026-11-05T14:00:00Z").0 + m * 60_000 + 7_500),
        price: Price(price),
    };
    let mut brush = Drawing::stroke(
        vec![p(0, 600_000), p(3, 599_500), p(6, 599_250), p(9, 600_750)],
        Style {
            arrow_end: true,
            ..Style::for_tool(Tool::Brush)
        },
    )
    .unwrap();
    brush.id = 7;
    let mut arrow = Drawing::new(
        Tool::Arrow,
        p(1, 600_000),
        p(8, 600_500),
        Style::for_tool(Tool::Arrow),
    );
    arrow.id = 8;
    let items = vec![brush.clone(), arrow.clone()];
    let file = DrawingsFile::from_drawings(&es(), &items, &HashMap::new());
    let json = String::from_utf8(file.encode()).unwrap();
    assert!(
        json.contains(r#""tool": "brush""#) || json.contains(r#""tool":"brush""#),
        "{json}"
    );
    assert!(json.contains("arrow_end"), "{json}");
    let Decoded::Ok { value: back, .. } = decode::<DrawingsFile>(&FORMAT, json.as_bytes()) else {
        panic!("decodes")
    };
    let (loaded, skipped) = back.to_drawings(25);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(loaded, items);
    assert_eq!(loaded[0].points.len(), 4);
    assert!(loaded[1].style.arrow_end, "an arrow keeps its head");
    // A trend line without the field: no head (as before).
    let (old, _) = DrawingsFile::from_drawings(&es(), &sample(), &HashMap::new()).to_drawings(25);
    assert!(
        old.iter()
            .all(|d| !d.style.arrow_end && d.points.is_empty())
    );
    // Too many points, or one: skipped, reported, kept as written.
    let mut big = file.clone();
    big.drawings[0].points = (0..600)
        .map(|k| AnchorDoc {
            time: format!("2026-11-05T14:{:02}:00.000Z", k % 60),
            price: 600_000,
        })
        .collect();
    big.drawings[1].points.clear();
    let mut one = big.drawings[0].clone();
    one.id = 9;
    one.points.truncate(1);
    big.drawings.push(one);
    let (loaded, skipped) = big.to_drawings(25);
    assert_eq!(loaded.len(), 1, "the arrow");
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert_eq!(big.unusable(&loaded).len(), 2);
}
