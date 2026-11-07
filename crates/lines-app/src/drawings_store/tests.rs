use super::*;
use crate::versioned::{Decoded, decode};
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
            color: 1,
            width: 2,
            dashed: true,
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
    let again: serde_json::Value =
        serde_json::from_slice(&DrawingsFile::from_drawings(&es(), &items).encode()).unwrap();
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
    let dir = std::env::temp_dir().join(format!("lines-drawings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let writer = Writer::spawn();
    let mut g = Drawings::new(Some(dir.clone()), Some(writer.clone()));
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

    let mut again = Drawings::new(Some(dir.clone()), None);
    let (q, report) = again.product(&es());
    assert!(report.is_none(), "{report:?}");
    let ids: Vec<u64> = q.lock().items().iter().map(|d| d.id).collect();
    assert_eq!(ids, [1, 2], "the undone third drawing isn't saved");
    // The same Arc for every chart of the product.
    assert!(Arc::ptr_eq(&again.product(&es()).0, &q));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_seed_file_names_its_product() {
    let dir = std::env::temp_dir().join(format!("lines-drawings-seed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("seed.json");
    std::fs::write(&f, V1).unwrap();
    let (p, items) = read_seed(&f).unwrap();
    assert_eq!(p.id, catalog::ES);
    assert_eq!(items.len(), 3);
    std::fs::write(&f, V1.replace("\"ES\"", "\"XX\"")).unwrap();
    assert!(read_seed(&f).unwrap_err().contains("unknown product"));
    let _ = std::fs::remove_dir_all(&dir);
}
