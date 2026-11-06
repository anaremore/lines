//! Import pipeline tests on synthetic Sierra files (decision 0010: no real data in git).

use std::path::Path;

use lines_core::{
    ContractKey, DisplayStyle, Month, Price, PriceFormat, PriceScale, Product, Side, UtcMillis,
};

use crate::format::synth::*;
use crate::format::{FIRST_SUB_TRADE, LAST_SUB_TRADE, ScidRecord, unix_us_to_sc};
use crate::import::{ImportOptions, PriceConv, PriceReject, import};

const DAY: &str = "2025-09-11";

fn sc(h: i64, m: i64, s: i64, ms: i64) -> i64 {
    let day = UtcMillis::parse_iso("2025-09-11T00:00:00").unwrap().0 * 1000;
    unix_us_to_sc(day + ((h * 60 + m) * 60 + s) * 1_000_000 + ms * 1000)
}

/// Prices are stored ×100 as f32, like the real NQ files.
fn px(p: f64) -> f32 {
    (p * 100.0) as f32
}

fn write_fixture(dir: &Path) {
    std::fs::create_dir_all(dir.join("MarketDepthData")).unwrap();
    let mut scid = scid_header();
    let mut t = |r: ScidRecord| scid.extend(scid_record(&r));
    t(tick(
        sc(0, 0, 0, 0) - 1_000_000,
        px(24000.0),
        px(23999.75),
        px(24000.0),
        1,
        true,
    )); // previous day
    t(tick(
        sc(0, 0, 0, 500),
        px(24128.75),
        px(24128.75),
        px(24129.0),
        2,
        false,
    )); // before any depth
    // Between batch 1 (ask 24129.00) and batch 2 (ask moves to 24129.50): at the OLD ask.
    t(tick(
        sc(0, 0, 1, 500),
        px(24129.0),
        px(24128.75),
        px(24129.0),
        1,
        true,
    ));
    // Same timestamp as batch 3: the tie goes to the trade.
    t(tick(
        sc(0, 0, 3, 0),
        px(24129.0),
        px(24128.75),
        px(24129.5),
        1,
        true,
    ));
    t(tick(sc(0, 0, 4, 0), 2_412_880.0, 0.0, 0.0, 1, true)); // off-tick
    t(tick(sc(0, 0, 4, 1), 2_412_875.5, 0.0, 0.0, 1, true)); // off-grid
    t(ScidRecord {
        open: 100.0,
        ..tick(sc(0, 0, 4, 2), px(1.0), 0.0, 0.0, 1, true)
    }); // bar record
    t(tick(sc(0, 0, 4, 3), px(24129.0), 0.0, 0.0, 0, true)); // zero volume
    // A buy sweep (Sierra unbundled trade) after the 6 s batch (bid 24128.75 / ask 24129.50):
    // first fill at the ask, later fills through it.
    t(ScidRecord {
        open: FIRST_SUB_TRADE,
        ..tick(
            sc(0, 0, 7, 0),
            px(24129.5),
            px(24128.75),
            px(24129.5),
            1,
            true,
        )
    });
    t(tick(
        sc(0, 0, 7, 0),
        px(24129.75),
        px(24128.75),
        px(24129.5),
        1,
        true,
    ));
    t(ScidRecord {
        open: LAST_SUB_TRADE,
        ..tick(
            sc(0, 0, 7, 0),
            px(24130.0),
            px(24128.75),
            px(24129.5),
            2,
            true,
        )
    });
    t(tick(
        sc(0, 0, 9, 0),
        px(24130.0),
        px(24129.75),
        px(24130.0),
        3,
        true,
    )); // standalone, above the ask
    t(tick(sc(24, 0, 0, 0), px(1.0), 0.0, 0.0, 1, true)); // next day
    std::fs::write(dir.join("NQZ5.CME.scid"), scid).unwrap();

    let mut d = depth_header();
    let mut r = |time, cmd, flags, orders, price, qty| {
        d.extend(depth_record(time, cmd, flags, orders, price, qty))
    };
    let t1 = sc(0, 0, 1, 0);
    r(t1, 1, 0, 0, 0.0, 0); // clear
    r(t1, 2, 0, 1, px(24128.75), 5); // add bid
    r(t1, 3, 1, 1, px(24129.0), 4); // add ask, end
    let t2 = sc(0, 0, 2, 0);
    r(t2, 7, 0, 0, px(24129.0), 0); // delete ask
    r(t2, 3, 1, 2, px(24129.5), 6); // add ask, end
    let t3 = sc(0, 0, 3, 0);
    r(t3, 4, 0, 0, px(24128.75), 9); // modify bid
    r(t3 - 5_000, 6, 0, 0, px(24100.0), 0); // delete missing level; time goes backwards
    r(t3, 9, 0, 0, px(24128.0), 1); // unknown command
    r(t3, 2, 1, 0, 2_412_875.5, 1); // off-grid price, end
    r(sc(0, 0, 5, 0), 2, 1, 0, px(24129.5), 1); // bid == ask: crossed after batch
    r(sc(0, 0, 6, 0), 6, 1, 0, px(24129.5), 0); // uncross
    r(sc(0, 0, 8, 0), 5, 0, 0, px(24131.0), 2); // modify missing ask; file ends mid-batch
    std::fs::write(
        dir.join("MarketDepthData")
            .join(format!("NQZ5.CME.{DAY}.depth")),
        d,
    )
    .unwrap();
}

fn opts(dir: &Path, out: &str) -> ImportOptions {
    ImportOptions {
        data_dir: dir.to_owned(),
        symbol: "NQZ5.CME".into(),
        date: DAY.into(),
        out: dir.join(out),
        price_multiplier: None,
    }
}

#[test]
fn price_conversion() {
    let nq = lines_feed::catalog::get(lines_feed::catalog::NQ).unwrap();
    let c = PriceConv::new(&nq, 100.0);
    assert_eq!(c.convert(2_412_875.0), Ok(Price(2_412_875)));
    assert_eq!(c.convert(2_412_880.0), Err(PriceReject::OffTick));
    assert_eq!(c.convert(2_412_875.5), Err(PriceReject::OffGrid));
    assert_eq!(c.convert(f32::NAN), Err(PriceReject::NotFinite));
    // Multiplier 1 (real prices stored): 24128.75 is exact in f32.
    assert_eq!(
        PriceConv::new(&nq, 1.0).convert(24128.75),
        Ok(Price(2_412_875))
    );
    // A decimal price stored as f32 carries float error; the f32-relative tolerance accepts it
    // (a double-precision tolerance would reject 75.43f32 = 75.43000030517578).
    let cents = Product {
        price: PriceFormat::new(
            PriceScale::decimal(2),
            DisplayStyle::Decimal { digits: 2 },
            1,
        ),
        ..nq
    };
    assert_eq!(
        PriceConv::new(&cents, 1.0).convert(75.43_f32),
        Ok(Price(7_543))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_import_report_replay_and_reproducibility() {
    let dir = std::env::temp_dir().join(format!("lines-sierra-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_fixture(&dir);

    let r = import(&opts(&dir, "a.lfr")).unwrap();
    assert_eq!(
        (r.instrument.as_str(), r.contract.as_str()),
        ("NQ", "NQZ25")
    );
    assert_eq!(r.tick_records_in_day, 11, "{r}");
    assert_eq!(r.trades_written, 7, "{r}");
    assert_eq!(
        (
            r.trades_standalone,
            r.trades_first_sub,
            r.trades_middle_sub,
            r.trades_last_sub
        ),
        (4, 1, 1, 1)
    );
    assert_eq!(r.tick_rejected_prices.get("OffTick"), Some(&1));
    assert_eq!(r.tick_rejected_prices.get("OffGrid"), Some(&1));
    assert_eq!((r.tick_bar_records_skipped, r.tick_zero_volume), (1, 1));
    assert_eq!(
        (
            r.depth_records,
            r.depth_batches,
            r.depth_unterminated_batches
        ),
        (12, 6, 1)
    );
    assert_eq!(r.depth_unknown_commands, 1);
    assert_eq!(r.depth_rejected_prices.get("OffGrid"), Some(&1));
    assert_eq!(r.depth_time_backwards, 1);
    assert_eq!(
        (r.deletes_of_missing_level, r.modifies_of_missing_level),
        (1, 1)
    );
    assert_eq!(r.book_crossed_after_batch, 1);
    // First fills are checked against the book as of their own time:
    // 1.5 s at the old ask 24129.00 (batch 2 not applied yet) => within;
    // 3.0 s ties with batch 3 => checked before it => within; 7 s sweep start at the ask =>
    // within; 9 s at 24130 > ask 24129.50 => outside. The 0.5 s trade predates any book.
    assert_eq!(
        (r.touch_checked, r.touch_within, r.touch_outside),
        (4, 3, 1),
        "{r}"
    );
    assert_eq!(
        (r.sweep_fills_checked, r.sweep_fills_through_touch),
        (2, 2),
        "{r}"
    );
    assert_eq!(r.crossed_examples.len(), 1);

    // Reproducible byte for byte.
    let r2 = import(&opts(&dir, "b.lfr")).unwrap();
    assert_eq!(r, r2);
    assert_eq!(
        std::fs::read(dir.join("a.lfr")).unwrap(),
        std::fs::read(dir.join("b.lfr")).unwrap()
    );

    // Replays through the normal feed path with the same book and trades.
    let cfg = lines_feed::FeedConfig::new(lines_feed::Source::Replay {
        path: dir.join("a.lfr"),
        speed: 0.0,
        stop_at: None,
    });
    let store = lines_feed::new_store(&cfg).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    lines_feed::run_feed(cfg, store.clone(), rx).await;
    {
        let d = store.read();
        assert_eq!(d.status, lines_feed::store::FeedStatus::ReplayFinished);
        assert_eq!(d.product.symbol, "NQ");
        let nqz25 = ContractKey::monthly(lines_feed::catalog::NQ, 2025, Month::new(12).unwrap());
        assert_eq!(
            d.contract,
            Some(nqz25),
            "contract carried through the v3 header"
        );
        assert!(d.capabilities.depth);
        assert_eq!(d.stats.trades_in, 7);
        assert_eq!(d.stats.book_crossed, 1);
        assert_eq!(
            d.book.best_bid().map(|(p, l)| (p.0, l.qty.0)),
            Some((2_412_875, 9))
        );
        assert_eq!(d.book.best_ask().map(|(p, _)| p.0), Some(2_412_950));
        assert_eq!(
            d.book.len(Side::Ask),
            2,
            "unterminated final batch is still applied"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_files_and_bad_symbols() {
    let dir = std::env::temp_dir().join(format!("lines-sierra-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(import(&opts(&dir, "x.lfr")).is_err(), "no scid file");
    let mut o = opts(&dir, "x.lfr");
    o.symbol = "CLZ5.CME".into();
    assert!(
        import(&o)
            .unwrap_err()
            .to_string()
            .contains("no catalog product")
    );
    // Header-only scid (as on disk for some contracts) and no depth: clean error.
    std::fs::write(dir.join("NQZ5.CME.scid"), scid_header()).unwrap();
    assert!(
        import(&opts(&dir, "x.lfr"))
            .unwrap_err()
            .to_string()
            .contains("no ticks or depth")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// One aggressor buy order sweeping three ask levels, written as Sierra writes it: the
/// sub-trade markers (`0xfd709ef9` opens, `0xfd709f01` closes), trade times made unique by
/// +1 µs within the millisecond, and that millisecond's depth (the book after the sweep,
/// with the order's remainder resting as the new bid) stamped at the whole ms. A standalone
/// sell then hits the new bid in the same millisecond.
#[test]
fn sweep_bracket_and_same_millisecond_fills() {
    let dir = std::env::temp_dir().join(format!("lines-sierra-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("MarketDepthData")).unwrap();
    let t0 = sc(13, 45, 0, 0);
    let t1 = sc(13, 45, 0, 7); // the sweep's millisecond

    let mut d = depth_header();
    let mut r =
        |time, cmd, flags, price, qty| d.extend(depth_record(time, cmd, flags, 1, price, qty));
    r(t0, 1, 0, 0.0, 0);
    r(t0, 2, 0, px(24000.0), 5);
    r(t0, 3, 0, px(24000.25), 1);
    r(t0, 3, 0, px(24000.5), 1);
    r(t0, 3, 0, px(24000.75), 3);
    r(t0, 3, 1, px(24001.0), 2);
    // Buy 6 at 24000.75: fills 1 + 1 + 3, the last lot rests as the new bid.
    r(t1, 7, 0, px(24000.25), 0);
    r(t1, 7, 0, px(24000.5), 0);
    r(t1, 7, 0, px(24000.75), 0);
    r(t1, 2, 1, px(24000.75), 1);
    std::fs::write(
        dir.join("MarketDepthData")
            .join(format!("NQZ5.CME.{DAY}.depth")),
        d,
    )
    .unwrap();

    let mut scid = scid_header();
    let mut t = |r: ScidRecord| scid.extend(scid_record(&r));
    let (bid, ask) = (px(24000.0), px(24000.25)); // Sierra's stored quote: before the ms
    t(ScidRecord {
        open: f32::from_bits(0xfd70_9ef9),
        ..tick(t1, px(24000.25), bid, ask, 1, true)
    });
    t(tick(t1 + 1, px(24000.5), bid, ask, 1, true));
    t(ScidRecord {
        open: f32::from_bits(0xfd70_9f01),
        ..tick(t1 + 2, px(24000.75), bid, ask, 3, true)
    });
    t(tick(t1 + 3, px(24000.75), bid, ask, 1, false));
    std::fs::write(dir.join("NQZ5.CME.scid"), scid).unwrap();

    let r = import(&opts(&dir, "s.lfr")).unwrap();
    assert_eq!(
        (
            r.trades_standalone,
            r.trades_first_sub,
            r.trades_middle_sub,
            r.trades_last_sub
        ),
        (1, 1, 1, 1),
        "{r}"
    );
    // First fills: the sweep's first (t1 + 0 µs) ties with the ms's depth and is checked
    // before it, at the old ask; the standalone sell (t1 + 3 µs) after it, at the new bid.
    // Both inside. With the markers read the other way round, the sweep's middle fill
    // (24000.50, below the new bid) would be taken for a standalone trade: outside. Merging
    // at whole milliseconds would check the sell against the old book (above its ask).
    assert_eq!(
        (r.touch_checked, r.touch_within, r.touch_outside),
        (2, 2, 0),
        "{r}"
    );
    // Later sweep fills are only counted, against the book after the ms's depth
    // (24000.75 / 24001.00): 24000.50 is through it, 24000.75 at it.
    assert_eq!(
        (r.sweep_fills_checked, r.sweep_fills_through_touch),
        (2, 1),
        "{r}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
