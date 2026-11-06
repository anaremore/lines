//! Opt-in test against the owner's real Sierra Chart data. Runs only with
//! `LINES_SIERRA_DATA=<Sierra Data folder> cargo test -p lines-sierra -- --ignored`.
//! Output goes to the temp dir and is deleted; nothing derived from real data is kept.

use std::path::PathBuf;

use lines_feed::store::FeedStatus;
use lines_feed::{FeedConfig, Source, new_store, run_feed};
use lines_sierra::{ImportOptions, import};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs LINES_SIERRA_DATA (owner's local Sierra Chart data)"]
async fn nqz5_2025_09_11_imports_and_replays() {
    let Ok(data) = std::env::var("LINES_SIERRA_DATA") else {
        eprintln!("LINES_SIERRA_DATA not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("lines-sierra-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let opts = |out: &str| ImportOptions {
        data_dir: PathBuf::from(&data),
        symbol: "NQZ5.CME".into(),
        date: "2025-09-11".into(),
        out: dir.join(out),
        price_multiplier: None,
    };

    let r = import(&opts("a.lfr")).expect("import");
    println!("{r}");

    // Exactness: every price converts to a whole NQ tick (multiplier is right).
    assert!(
        r.depth_rejected_prices.is_empty() && r.tick_rejected_prices.is_empty(),
        "{r}"
    );
    assert_eq!(r.depth_unknown_commands, 0);
    assert_eq!(r.depth_unterminated_batches, 0);
    // Every tick record is a trade (no unknown `open` markers).
    assert_eq!(r.tick_bar_records_skipped, 0);
    assert_eq!(r.trades_written, r.tick_records_in_day);
    assert!(
        r.depth_batches > 1_000_000 && r.trades_written > 5_000,
        "{r}"
    );

    // The book is never *crossed* (bid > ask) after a batch. It is briefly *locked*
    // (bid == ask) 81 times around the 12:30 UTC CPI release; that is reported, not failed.
    assert_eq!(
        r.book_crossed_after_batch, r.book_locked_after_batch,
        "a truly crossed book: {r}"
    );

    // First fills of aggressor orders print at the touch. Observed 685/775 = 88.4%; the rest
    // are 1–10+ ticks away, spread over the day (cause under investigation). Later fills of
    // sweeps legitimately print through the touch and are not asserted.
    let ratio = r.touch_within as f64 / r.touch_checked as f64;
    assert!(ratio > 0.85, "first fills at the touch: {ratio:.3}\n{r}");

    // Reproducible byte for byte.
    let r2 = import(&opts("b.lfr")).expect("import again");
    assert_eq!(r, r2);
    assert_eq!(
        std::fs::read(dir.join("a.lfr")).unwrap(),
        std::fs::read(dir.join("b.lfr")).unwrap()
    );
    std::fs::remove_file(dir.join("b.lfr")).unwrap();

    // Replays through the normal feed path into the same trades and book checks.
    let cfg = FeedConfig::new(Source::Replay {
        path: dir.join("a.lfr"),
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&cfg).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    run_feed(cfg, store.clone(), rx).await;
    {
        let d = store.read();
        assert_eq!(d.status, FeedStatus::ReplayFinished);
        assert_eq!(d.product.symbol, "NQ");
        // NQZ5.CME: 1-digit year resolved from the file's data (decision 0013).
        assert_eq!(
            d.contract
                .map(|k| k.label(&d.product).as_str().to_owned())
                .as_deref(),
            Some("NQZ25")
        );
        assert_eq!(d.stats.trades_in, r.trades_written);
        assert_eq!(d.stats.book_crossed, r.book_crossed_after_batch);
        assert_eq!(d.stats.rejected + d.stats.decode_errors, 0);
        assert!(!d.book.is_crossed());
        assert!(
            d.series.len() > 100,
            "bars built from trades: {}",
            d.series.len()
        );
        println!(
            "replay: {} trades, {} bars, final bid {:?} ask {:?}, levels {}/{}",
            d.stats.trades_in,
            d.series.len(),
            d.bid,
            d.ask,
            d.book.len(lines_core::Side::Bid),
            d.book.len(lines_core::Side::Ask)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
