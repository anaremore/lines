use super::*;

#[test]
fn hex_round_trips_and_rejects_junk() {
    let c = Rgb::parse("#8C7BFF").unwrap();
    assert_eq!(c, Rgb::hex(0x8c7bff));
    assert_eq!(Rgb::parse("8c7bff"), Some(c));
    let mut s = String::new();
    c.write_hex(&mut s).unwrap();
    assert_eq!(s, "#8c7bff");
    for bad in ["", "#", "#12345", "#1234567", "#12345g", "＃12345", "#ééé"] {
        assert_eq!(Rgb::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn styles_normalise_into_range() {
    let l = LineStyle {
        width: 9,
        opacity: 0,
        ..LineStyle::DEFAULT
    }
    .normalised();
    assert_eq!((l.width, l.opacity), (MAX_WIDTH, MIN_OPACITY));
    let l = LineStyle {
        width: 0,
        opacity: 255,
        ..LineStyle::DEFAULT
    }
    .normalised();
    assert_eq!((l.width, l.opacity), (MIN_WIDTH, 100));
    let f = FillStyle {
        opacity: 3,
        ..FillStyle::DEFAULT
    }
    .normalised();
    assert_eq!(f.opacity, MIN_OPACITY);
}

#[test]
fn keys_round_trip() {
    for t in LineType::ALL {
        assert_eq!(LineType::from_key(t.key()), Some(t));
    }
    for s in TextSize::ALL {
        assert_eq!(TextSize::from_key(s.key()), Some(s));
    }
    assert_eq!(LineType::from_key("wavy"), None);
    // Sizes are on the type scale (docs/design.md §5).
    for s in TextSize::ALL {
        assert!([11.0, 12.0, 13.0, 15.0].contains(&s.px()));
    }
}

#[test]
fn dash_segments_cover_the_line_in_pattern_order() {
    let mut got = Vec::new();
    DashPattern::SOLID.segments(10.0, |a, b| got.push((a, b)));
    assert_eq!(got, [(0.0, 10.0)]);
    got.clear();
    let p = DashPattern {
        seg: [4.0, 2.0, 1.0, 2.0],
        n: 4,
    };
    p.segments(15.0, |a, b| got.push((a, b)));
    assert_eq!(got, [(0.0, 4.0), (6.0, 7.0), (9.0, 13.0)]);
    // A zero-length pattern can't loop forever.
    got.clear();
    DashPattern {
        seg: [0.0; 4],
        n: 2,
    }
    .segments(5.0, |a, b| got.push((a, b)));
    assert_eq!(got, [(0.0, 5.0)]);
}

#[test]
fn contrast_and_over_match_known_values() {
    let (black, white) = (Rgb::hex(0), Rgb::hex(0xffffff));
    assert!((contrast(black, white) - 21.0).abs() < 0.01);
    // Brand violet on ink: 5.61:1.
    assert!((contrast(Rgb::hex(0x8c7bff), Rgb::hex(0x11141b)) - 5.61).abs() < 0.02);
    assert_eq!(over(white, 100, black), white);
    assert_eq!(over(white, 50, black), Rgb::hex(0x808080));
}

#[test]
fn delta_e_2000_matches_reference_values() {
    assert!(delta_e_2000(Rgb::hex(0x808080), Rgb::hex(0x808080)) < 1e-9);
    let bw = delta_e_2000(Rgb::hex(0), Rgb::hex(0xffffff));
    assert!((bw - 100.0).abs() < 0.5, "{bw}");
    // Symmetric.
    let (a, b) = (Rgb::hex(0x3fcf9a), Rgb::hex(0x4fa8ff));
    assert!((delta_e_2000(a, b) - delta_e_2000(b, a)).abs() < 1e-9);
}

#[test]
fn readability_warns_near_meanings_and_on_low_contrast_never_blocks() {
    let bg = Rgb::hex(0x11141b);
    let meanings = [("up", Rgb::hex(0x3fcf9a)), ("down", Rgb::hex(0xff6b7f))];
    // A green close to mint up.
    let r = readability(Rgb::hex(0x30c090), 100, bg, &meanings, false);
    assert_eq!(r.near, Some("up"));
    assert!(r.low_contrast.is_none());
    assert!(!r.ok());
    // Paper on ink: clear.
    let r = readability(Rgb::hex(0xeceaf4), 100, bg, &meanings, true);
    assert!(r.ok(), "{r:?}");
    // The same at 25% opacity is too faint for a line, and much too faint for text.
    let r = readability(Rgb::hex(0xeceaf4), 25, bg, &meanings, false);
    assert!(r.low_contrast.is_some_and(|c| c < LINE_CONTRAST));
    // Dark gray on ink: low contrast; text needs more than lines.
    let mid = Rgb::hex(0x5a6070);
    let line = readability(mid, 100, bg, &meanings, false);
    let text = readability(mid, 100, bg, &meanings, true);
    assert_eq!(line.needed, LINE_CONTRAST);
    assert_eq!(text.needed, TEXT_CONTRAST);
    assert!(text.low_contrast.is_some());
    // The nearest meaning wins when two are close.
    let r = readability(Rgb::hex(0xff6b80), 100, bg, &meanings, false);
    assert_eq!(r.near, Some("down"));
}
